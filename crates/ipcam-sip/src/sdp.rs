//! SDP offer/answer 的类型化构造与解析 (基于 sdp-rs).
//!
//! 不手拼字符串: 字段顺序, `\r\n`, 必填行 (v/o/s/t) 错一个对端就
//! 直接拒. `SessionDescription` 实现了 `Display`/`FromStr`, 构造完
//! `to_string()` 即发出去, 收到 answer `try_from` 读回来, 天然可
//! round-trip 测试.

use anyhow::{Result, anyhow};
use ipcam_core::AudioCodec;
use sdp_rs::lines::attribute::Rtpmap;
use sdp_rs::lines::common::{Addrtype, Nettype};
use sdp_rs::lines::connection::ConnectionAddress;
use sdp_rs::lines::media::{MediaType, ProtoType};
use sdp_rs::lines::{Attribute, Connection, Media, Origin, SessionName, Version};
use sdp_rs::{MediaDescription, SessionDescription, Time};
use std::net::{IpAddr, SocketAddr};
use tracing::info;
use vec1::vec1;

/// RFC 3551 静态 payload type; 取值和 `AudioCodec::static_pt` 的一致性
/// 由 tests::pt_constants_match_static_pt 守住
pub const PT_PCMU: u8 = 0;
pub const PT_PCMA: u8 = 8;
/// H264 视频没有静态 pt, 走动态段 (96-127), 必须配 a=rtpmap + a=fmtp
pub const PT_H264: u8 = 96;

/// 构造音视频通话的 SDP offer: 在音频 offer 上再挂一路 H264 视频.
///
/// ```text
/// m=video 40002 RTP/AVP 96             <- 视频用动态 pt 96
/// a=rtpmap:96 H264/90000               <- 视频时钟固定 90000, 写错播放速率全乱
/// a=fmtp:96 profile-level-id=42e01f
/// a=sendrecv
/// ```
///
/// 视频和音频的三个关键差别 (都体现在 `video_media_description` 里):
/// 1. pt 必须走动态段 (96-127), 所以 rtpmap 是强制的, 不能像 PCMA 那样省
/// 2. 时钟 90000 而不是 8000
/// 3. 要 a=fmtp 带 H264 参数, 有些对端 (尤其 SIP 门禁/视频话机) 缺这行
///    协商不过
pub fn build_av_offer(
    local_ip: IpAddr,
    audio_port: u16,
    video_port: u16,
    audio_pts: &[u8],
) -> SessionDescription {
    let mut sdp = build_audio_offer(local_ip, audio_port, audio_pts);
    sdp.media_descriptions
        .push(video_media_description(video_port, PT_H264, None));
    sdp
}

/// H264 视频媒体描述. offer/answer 结构完全相同, 只是 pt 取值不同
/// (offer 用我们自己的 PT_H264, answer 用对端 offer 里声明的 pt).
///
/// `fmtp`: answer 传对端 offer 里选中编码的 fmtp 参数, 原样回显
/// (packetization-mode 等对端按 answer 理解收发模式, 不回显容易
/// 协商错位); offer 或对端没带 fmtp 时传 None, 用默认值
/// profile-level-id=42e01f: Baseline profile level 3.1.
/// 不写 packetization-mode = mode 0 (单 NAL 模式): 门口机/
/// 室内机的 RTP 接收器不认 FU-A 分片, 实测 Linphone (mode 0)
/// 能出画面而我们 mode 1 黑屏. 代价是片源必须切成小于 MTU 的
/// slice (rtp_send 的测试片源已按 slice-max-size=1300 重编码).
/// sdp-rs 0.2.1 的 Attribute 枚举没有 Fmtp 变体, 走 Other,
/// Display 出来就是标准 a=fmtp:... 行
fn video_media_description(port: u16, pt: u8, fmtp: Option<&str>) -> MediaDescription {
    let fmtp_params = fmtp.unwrap_or("profile-level-id=42e01f");
    MediaDescription {
        media: Media {
            media: MediaType::Video,
            port,
            num_of_ports: None,
            proto: ProtoType::RtpAvp,
            fmt: pt.to_string(),
        },
        info: None,
        connections: vec![],
        bandwidths: vec![],
        key: None,
        attributes: vec![
            Attribute::Rtpmap(Rtpmap {
                payload_type: pt as u32,
                encoding_name: "H264".into(),
                clock_rate: 90000,
                encoding_params: None,
            }),
            Attribute::Other("fmtp".into(), Some(format!("{pt} {fmtp_params}"))),
            Attribute::Sendrecv,
        ],
    }
}

/// 构造音频通话的 SDP offer (单路 audio, sendrecv).
///
/// `local_ip`/`port` 是本端 RTP 收包地址 -- 必须是对端路由可达的
/// LAN 地址 (同 SipClient 的 Contact), 不能是 127.0.0.1.
///
/// `to_string()` 出来的就是这段 SDP (以 PCMA 为例), 逐行对应结构体字段:
///
/// ```text
/// v=0                                  <- version: 协议版本, 恒为 0
/// o=- 1 1 IN IP4 192.168.1.100        <- origin: 发起方标识
/// s=ipcam-sip                          <- session_name: 会话名 (必填行)
/// c=IN IP4 192.168.1.100              <- connection: 媒体数据发到这个地址
/// t=0 0                                <- times: 会话起止时间, 0 0 = 不限时
/// m=audio 40000 RTP/AVP 8              <- media: 媒体类型/端口/协议/载荷类型
/// a=rtpmap:8 PCMA/8000                 <- attribute: pt 8 -> PCMA 编码, 8kHz
/// a=ptime:20                           <- attribute: 每包 20ms 音频
/// a=sendrecv                           <- attribute: 双向收发
/// ```
pub fn build_audio_offer(local_ip: IpAddr, port: u16, payload_types: &[u8]) -> SessionDescription {
    let mut sdp = session_skeleton(local_ip, "1");
    sdp.media_descriptions
        .push(audio_media_description(port, payload_types));
    sdp
}

/// 会话级骨架 (v/o/s/c/t 等), offer/answer 完全一致, 唯一差别是
/// origin 的 sess_version (answer 的要大于 offer 的).
///
/// 各字段说明:
/// - v=0: SDP 版本号, RFC 4566 定死就是 0, 没有 1
/// - o=: 发起方身份标识. 对端基本不看内容, 只看格式合法性.
///   sess_id/sess_version 本应每次会话唯一 (常用 NTP 时间戳), 重新
///   INVITE 改参数时 sess_version 要 +1. 骨架阶段固定值够用 --
///   对讲场景不做会话内 re-INVITE 改参数
/// - s=: 会话名, 必填行. 内容无所谓, 很多设备直接写 "-",
///   这里写项目名纯粹是抓包时好认
/// - i=/u=/e=/p=: 纯展示用元信息, 设备间通话全都不填
/// - c=: 最重要的一行: 告诉对端 "把 RTP 发到这个 IP". 写错 (比如
///   127.0.0.1) 的典型症状: 信令全通, 呼叫建立, 但单向或双向无声.
///   ttl/numaddr 只有组播才用, 单播留空
/// - b=: 带宽建议, 对讲场景不声明, 让对方按编码默认来
/// - t=0 0: 会话永久有效. SDP 规范强制至少一条 t= 行, sdp-rs 用
///   Vec1 (非空 Vec) 从类型上杜绝漏写
/// - k=: 媒体加密密钥 (明文传输, 早已废弃), SRTP 走别的机制
/// - 会话级 a=: 我们的属性都是媒体级的, 故为空
fn session_skeleton(local_ip: IpAddr, sess_version: &str) -> SessionDescription {
    let addrtype = match local_ip {
        IpAddr::V4(_) => Addrtype::Ip4,
        IpAddr::V6(_) => Addrtype::Ip6,
    };
    SessionDescription {
        version: Version::V0,
        origin: Origin {
            username: "-".into(),
            sess_id: "1".into(),
            sess_version: sess_version.into(),
            nettype: Nettype::In,       // IN = Internet, 目前只有这一个合法值
            addrtype: addrtype.clone(), // IP4 / IP6
            unicast_address: local_ip,
        },
        session_name: SessionName::new("ipcam-sip".into()),
        session_info: None,
        uri: None,
        emails: vec![],
        phones: vec![],
        connection: Some(Connection {
            nettype: Nettype::In,
            addrtype,
            connection_address: ConnectionAddress {
                base: local_ip,
                ttl: None,
                numaddr: None,
            },
        }),
        bandwidths: vec![],
        times: vec1![Time {
            active: sdp_rs::lines::Active { start: 0, stop: 0 },
            repeat: vec![],
            zone: None,
        }],
        key: None,
        attributes: vec![],
        media_descriptions: vec![],
    }
}

/// 音频媒体描述: fmt 列出全部候选 pt + 每个 pt 一条 rtpmap +
/// ptime + sendrecv. offer 传全部支持的 pt (对端从中挑一个 --
/// 只给一个就是 "没得挑, 不行就拒"), answer 只传选中的那个.
///
/// - num_of_ports: 组播端口组才用, 单播留空
/// - RTP/AVP = 裸 RTP/UDP; SAVP 才是 SRTP
/// - 媒体级 c= 为空 -> 继承会话级 c= (RFC 4566 的继承规则)
/// - a=rtpmap: 把数字 pt 映射到具体编码. PCMA(8)/PCMU(0) 是 RFC 3551
///   静态分配的, 这行其实可省, 写上是为了显式可读; 动态 pt 这行
///   是强制的, 少了对端直接不认. G.711 家族固定 8kHz 采样, 单声道
///   留空 encoding_params
/// - a=ptime:20: 每个 RTP 包承载 20ms 音频 (G.711 即 160 字节载荷).
///   对端按这个节奏发包, 收端 jitter buffer 按它估算
/// - a=sendrecv: 方向协商, 对讲通话必须双向; sendonly/recvonly
///   用于单向广播/监听场景
fn audio_media_description(port: u16, payload_types: &[u8]) -> MediaDescription {
    let fmt = payload_types
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let mut attributes: Vec<Attribute> = payload_types
        .iter()
        .map(|&pt| {
            Attribute::Rtpmap(Rtpmap {
                payload_type: pt as u32,
                encoding_name: codec_name(pt).into(),
                clock_rate: 8000,
                encoding_params: None,
            })
        })
        .collect();
    attributes.push(Attribute::Ptime(20.0));
    attributes.push(Attribute::Sendrecv);
    MediaDescription {
        media: Media {
            media: MediaType::Audio,
            port,
            num_of_ports: None,
            proto: ProtoType::RtpAvp,
            fmt,
        },
        info: None,
        connections: vec![],
        bandwidths: vec![],
        key: None,
        attributes,
    }
}

/// 从 answer 里协商出的对端媒体信息 -- 拿到它就能往这个地址发/收 RTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerMedia {
    pub addr: SocketAddr,
    pub payload_type: u8,
    pub codec: String,
}

/// offer 里声明的一个候选编码 (pt + 编码名). 顺序即对端偏好:
/// m= 行的 fmt 列表按偏好降序排列 (RFC 3264), 排前面的优先选
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferCodec {
    pub payload_type: u8,
    pub codec: String,
    /// a=fmtp 里该 pt 的编码参数 (不含 pt 前缀), 如 H264 的
    /// "profile-level-id=42e01f;packetization-mode=1", G729 的
    /// "annexb=yes". 该 pt 没有 fmtp 行为 None (G.711 本来就没有).
    /// answer 应回显选中编码的 fmtp, 尤其是 H264 的
    /// packetization-mode -- 不回显有些对端按默认值理解, 协商错位
    pub fmtp: Option<String>,
}

/// 从 offer 里解析出的本端媒体信息（用于构造 answer）.
///
/// `codecs` 是对端声明的全部候选编码 (按对端偏好排序), 不是只有
/// 第一个 -- PBX 的典型 offer 是 "96 0 8 18 ..." (opus 打头, 后面
/// 跟着 PCMU/PCMA), 只看第一个就会把本来能谈的 G.711 误判成
/// "不支持的编码" 而拒掉整路音频. 调用方从中挑第一个自己支持的.
#[derive(Debug, Clone)]
pub struct OfferMedia {
    pub addr: IpAddr,
    pub port: u16,
    pub codecs: Vec<OfferCodec>,
}

/// offer 解析结果.
#[derive(Debug, Clone, Default)]
pub struct OfferMedias {
    pub audio: Option<OfferMedia>,
    pub video: Option<OfferMedia>,
}

/// 解析对端 (200 OK 或 18x) 带回的 SDP answer, 只取第一路媒体.
///
/// 音视频双路的场景用 `parse_answer_all`. m= 行没带 c= 时回落到
/// session 级 c= (RFC 4566 允许的继承).
pub fn parse_answer(body: &[u8]) -> Result<PeerMedia> {
    let sdp = parse_sdp(body)?;
    let media = sdp
        .media_descriptions
        .first()
        .ok_or_else(|| anyhow!("answer has no media description"))?;
    peer_from_media(sdp.connection.as_ref(), media)
}

fn parse_sdp(body: &[u8]) -> Result<SessionDescription> {
    let text = std::str::from_utf8(body)?;
    SessionDescription::try_from(text).map_err(|e| anyhow!("invalid SDP: {e}"))
}

/// 按媒体类型分别提取 audio/video 两路. 两路都没有才算失败;
/// 只有一路是正常情况, 不算错. offer/answer 的差别只在 from_media
/// (offer 要全部候选编码, answer 取协商出的第一个)
fn parse_media_pair<T>(
    body: &[u8],
    what: &str,
    from_media: impl Fn(Option<&Connection>, &MediaDescription) -> Result<T>,
) -> Result<(Option<T>, Option<T>)> {
    let sdp = parse_sdp(body)?;
    info!("{what} sdp:\n{sdp}");
    let mut audio = None;
    let mut video = None;
    for media in &sdp.media_descriptions {
        let parsed = from_media(sdp.connection.as_ref(), media)?;
        if media.media.media == MediaType::Audio {
            audio = Some(parsed);
        } else if media.media.media == MediaType::Video {
            video = Some(parsed);
        }
    }
    if audio.is_none() && video.is_none() {
        anyhow::bail!("{what} has no audio/video media");
    }
    Ok((audio, video))
}

/// 双路协商结果: audio/video 各自独立, 对端可能只接了一路
/// (比如老门禁没有摄像头, answer 里只有 m=audio).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerMedias {
    pub audio: Option<PeerMedia>,
    pub video: Option<PeerMedia>,
}

/// 解析 answer, 按媒体类型分别提取 audio/video 两路的协商结果.
pub fn parse_answer_all(body: &[u8]) -> Result<PeerMedias> {
    let (audio, video) = parse_media_pair(body, "answer", peer_from_media)?;
    Ok(PeerMedias { audio, video })
}

/// 解析 offer SDP, 按媒体类型提取 audio/video 两路的信息（用于构造 answer）.
pub fn parse_offer_all(body: &[u8]) -> Result<OfferMedias> {
    let (audio, video) = parse_media_pair(body, "offer", offer_from_media)?;
    Ok(OfferMedias { audio, video })
}

fn offer_from_media(
    session_conn: Option<&Connection>,
    media: &MediaDescription,
) -> Result<OfferMedia> {
    Ok(OfferMedia {
        addr: media_ip(session_conn, media)?,
        port: media.media.port,
        codecs: parse_fmt_codecs(media)?,
    })
}

/// 会话级/媒体级 c= 继承 (RFC 4566): 媒体级没带 c= 就用会话级的
fn media_ip(session_conn: Option<&Connection>, media: &MediaDescription) -> Result<IpAddr> {
    media
        .connections
        .first()
        .or(session_conn)
        .map(|c| c.connection_address.base)
        .ok_or_else(|| anyhow!("SDP has no connection address"))
}

/// 解析 m= 行 fmt 列表为候选编码列表, 顺序保持对端的偏好降序.
/// 空列表或非数字 pt 都报错 (m= 行至少得有一个能用的 pt)
fn parse_fmt_codecs(media: &MediaDescription) -> Result<Vec<OfferCodec>> {
    let codecs: Vec<OfferCodec> = media
        .media
        .fmt
        .split_whitespace()
        .map(|pt| {
            let payload_type: u8 = pt
                .parse()
                .map_err(|_| anyhow!("unsupported fmt list: {}", media.media.fmt))?;
            Ok(OfferCodec {
                payload_type,
                codec: resolve_codec(media, payload_type),
                fmtp: resolve_fmtp(media, pt).map(str::to_string),
            })
        })
        .collect::<Result<_>>()?;
    if codecs.is_empty() {
        anyhow::bail!("m= line has no payload type");
    }
    Ok(codecs)
}

/// pt -> 编码名: 先查 rtpmap (动态 pt >=96 必须有), 静态 pt 回退
/// RFC 3551 表, 都没有就报 "unknown", 不猜
fn resolve_codec(media: &MediaDescription, payload_type: u8) -> String {
    media
        .attributes
        .iter()
        .find_map(|a| match a {
            Attribute::Rtpmap(r) if r.payload_type == payload_type as u32 => {
                Some(r.encoding_name.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| codec_name(payload_type).into())
}

/// pt -> a=fmtp 参数: a=fmtp:<pt> <参数...>, 取匹配 pt 的那条,
/// 返回去掉 pt 前缀后的参数部分; 该 pt 没有 fmtp 行返回 None
fn resolve_fmtp<'a>(media: &'a MediaDescription, pt: &str) -> Option<&'a str> {
    media.attributes.iter().find_map(|a| match a {
        Attribute::Other(name, Some(value)) if name == "fmtp" => {
            let (prefix, params) = value.split_once(char::is_whitespace)?;
            (prefix == pt).then_some(params)
        }
        _ => None,
    })
}

/// 构造音视频通话的 SDP answer.
///
/// `selected_video_fmtp` 传对端 offer 里选中视频编码的 fmtp 参数
/// (回显, 见 `video_media_description`); 对端没带 fmtp 传 None.
pub fn build_av_answer(
    local_ip: IpAddr,
    audio_port: u16,
    video_port: u16,
    selected_audio_pt: u8,
    selected_video_pt: u8,
    selected_video_fmtp: Option<&str>,
) -> SessionDescription {
    let mut sdp = build_audio_answer(local_ip, audio_port, selected_audio_pt);
    sdp.media_descriptions.push(video_media_description(
        video_port,
        selected_video_pt,
        selected_video_fmtp,
    ));
    sdp
}

fn build_audio_answer(local_ip: IpAddr, port: u16, selected_pt: u8) -> SessionDescription {
    // answer 的 sess_version 要大于 offer 的 ("1")
    let mut sdp = session_skeleton(local_ip, "2");
    sdp.media_descriptions
        .push(audio_media_description(port, &[selected_pt]));
    sdp
}

/// 从一路 MediaDescription 提取对端地址/编码. session_conn 是会话级
/// c=, 媒体级没写 c= 时按 RFC 4566 继承它. answer 正常每路只收窄到
/// 一个编码 (RFC 3264), 取 fmt 列表第一个
fn peer_from_media(
    session_conn: Option<&Connection>,
    media: &MediaDescription,
) -> Result<PeerMedia> {
    let first = parse_fmt_codecs(media)?.remove(0);
    Ok(PeerMedia {
        addr: SocketAddr::new(media_ip(session_conn, media)?, media.media.port),
        payload_type: first.payload_type,
        codec: first.codec,
    })
}

/// RFC 3551 静态 payload type 表: PCMA/PCMU 走 `AudioCodec` 的映射,
/// 其余只列音频里常见的; 未知动态 pt 原样返回 "unknown"
fn codec_name(payload_type: u8) -> &'static str {
    match AudioCodec::from_static_pt(payload_type) {
        AudioCodec::Unknown => match payload_type {
            3 => "GSM",
            9 => "G722",
            18 => "G729",
            _ => "unknown",
        },
        c => c.rtpmap_name(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// PT_* 常量必须和 AudioCodec::static_pt 一致 (两处都是 RFC 3551
    /// 的取值, 只许对, 不许漂)
    #[test]
    fn pt_constants_match_static_pt() {
        assert_eq!(AudioCodec::G711U.static_pt(), Some(PT_PCMU));
        assert_eq!(AudioCodec::G711A.static_pt(), Some(PT_PCMA));
        assert_eq!(AudioCodec::from_static_pt(PT_PCMU), AudioCodec::G711U);
        assert_eq!(AudioCodec::from_static_pt(PT_PCMA), AudioCodec::G711A);
    }

    #[test]
    fn offer_roundtrip() {
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 11, 100));
        let offer = build_audio_offer(ip, 40000, &[PT_PCMU, PT_PCMA]);
        let text = offer.to_string();

        // 必须能被标准解析器读回来 (手拼字符串最容易死在这)
        let parsed = SessionDescription::try_from(text.as_str()).expect("offer must parse");
        let m = &parsed.media_descriptions[0];
        assert_eq!(m.media.port, 40000);
        assert_eq!(m.media.fmt, "0 8");
        assert!(
            text.contains("a=rtpmap:0 PCMU/8000") && text.contains("a=rtpmap:8 PCMA/8000"),
            "offer:\n{text}"
        );
        assert_eq!(
            parsed.connection.unwrap().connection_address.base,
            IpAddr::V4(Ipv4Addr::new(192, 168, 11, 100))
        );
        assert!(
            m.attributes.contains(&Attribute::Sendrecv),
            "offer must be sendrecv"
        );
    }

    #[test]
    fn av_offer_roundtrip() {
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 11, 100));
        let offer = build_av_offer(ip, 40000, 40002, &[PT_PCMU, PT_PCMA]);
        let text = offer.to_string();

        // 视频路的关键行必须原样出现在序列化结果里
        assert!(text.contains("m=video 40002 RTP/AVP 96"), "offer:\n{text}");
        assert!(text.contains("a=rtpmap:96 H264/90000"), "offer:\n{text}");
        assert!(
            text.contains("a=fmtp:96 profile-level-id="),
            "offer:\n{text}"
        );

        // 整体必须能被解析回来, 且两路齐全
        let parsed = SessionDescription::try_from(text.as_str()).expect("offer must parse");
        assert_eq!(parsed.media_descriptions.len(), 2);
        assert_eq!(parsed.media_descriptions[0].media.media, MediaType::Audio);
        assert_eq!(parsed.media_descriptions[1].media.media, MediaType::Video);
    }

    #[test]
    fn parse_answer_with_audio_and_video() {
        // 视频话机的典型 answer: 会话级 c=, 音频一路 + 视频一路
        let answer = concat!(
            "v=0\r\n",
            "o=doorphone 1 1 IN IP4 192.168.1.50\r\n",
            "s=call\r\n",
            "c=IN IP4 192.168.1.50\r\n",
            "t=0 0\r\n",
            "m=audio 30000 RTP/AVP 8\r\n",
            "a=rtpmap:8 PCMA/8000\r\n",
            "a=sendrecv\r\n",
            "m=video 30002 RTP/AVP 96\r\n",
            "a=rtpmap:96 H264/90000\r\n",
            "a=fmtp:96 profile-level-id=42e01f;packetization-mode=1\r\n",
            "a=sendrecv\r\n",
        );
        let peers = parse_answer_all(answer.as_bytes()).unwrap();
        let audio = peers.audio.expect("audio negotiated");
        let video = peers.video.expect("video negotiated");
        assert_eq!(audio.addr.port(), 30000);
        assert_eq!(audio.codec, "PCMA");
        assert_eq!(video.addr.port(), 30002);
        assert_eq!(video.payload_type, 96);
        assert_eq!(video.codec, "H264");
        assert_eq!(audio.addr.ip(), video.addr.ip());
    }

    #[test]
    fn parse_answer_all_audio_only_is_ok() {
        // 老门禁没有摄像头: answer 只有 m=audio, 这是正常协商结果不是错误
        let answer = concat!(
            "v=0\r\n",
            "o=pbx 1 1 IN IP4 192.168.1.17\r\n",
            "s=call\r\n",
            "c=IN IP4 192.168.1.17\r\n",
            "t=0 0\r\n",
            "m=audio 18000 RTP/AVP 8\r\n",
        );
        let peers = parse_answer_all(answer.as_bytes()).unwrap();
        assert!(peers.audio.is_some());
        assert!(peers.video.is_none());
    }

    #[test]
    fn parse_typical_answer() {
        // 典型 PBX 回的 answer: session 级 c=, m= 不带 c=
        let answer = concat!(
            "v=0\r\n",
            "o=pbx 123 456 IN IP4 192.168.1.17\r\n",
            "s=call\r\n",
            "c=IN IP4 192.168.1.17\r\n",
            "t=0 0\r\n",
            "m=audio 18000 RTP/AVP 8\r\n",
            "a=rtpmap:8 PCMA/8000\r\n",
            "a=sendrecv\r\n",
        );
        let peer = parse_answer(answer.as_bytes()).unwrap();
        assert_eq!(
            peer,
            PeerMedia {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 17)), 18000),
                payload_type: 8,
                codec: "PCMA".into(),
            }
        );
    }

    #[test]
    fn parse_answer_with_media_level_connection() {
        // m= 自带 c= 时优先于 session 级
        let answer = concat!(
            "v=0\r\n",
            "o=gw 1 1 IN IP4 10.0.0.1\r\n",
            "s=-\r\n",
            "c=IN IP4 10.0.0.1\r\n",
            "t=0 0\r\n",
            "m=audio 9000 RTP/AVP 0\r\n",
            "c=IN IP4 10.0.0.2\r\n",
        );
        let peer = parse_answer(answer.as_bytes()).unwrap();
        assert_eq!(
            peer.addr,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 9000)
        );
        // 静态 pt 无 rtpmap 时查表
        assert_eq!(peer.codec, "PCMU");
    }

    #[test]
    fn parse_offer_multi_pt_audio_keeps_all_candidates() {
        // 真实 PBX 的 offer: opus 打头, PCMU/PCMA 跟在后面, 尾部还有
        // telephone-event. 只取第一个 pt 会把整路音频误判成 "不支持"
        let offer = concat!(
            "v=0\r\n",
            "o=- 1790495688 1790495689 IN IP4 192.168.2.113\r\n",
            "s=-\r\n",
            "c=IN IP4 192.168.2.113\r\n",
            "t=0 0\r\n",
            "m=audio 13782 RTP/AVP 96 0 8 18 9 101 100\r\n",
            "a=sendrecv\r\n",
            "a=rtpmap:96 opus/48000/2\r\n",
            "a=fmtp:96 useinbandfec=1\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
            "a=rtpmap:8 PCMA/8000\r\n",
            "a=rtpmap:18 G729/8000\r\n",
            "a=fmtp:18 annexb=yes\r\n",
            "a=rtpmap:9 G722/8000\r\n",
            "a=rtpmap:101 telephone-event/48000\r\n",
            "a=rtpmap:100 telephone-event/8000\r\n",
        );
        let medias = parse_offer_all(offer.as_bytes()).unwrap();
        let audio = medias.audio.expect("audio offered");
        assert_eq!(audio.port, 13782);
        let names: Vec<&str> = audio.codecs.iter().map(|c| c.codec.as_str()).collect();
        assert_eq!(
            names,
            [
                "opus",
                "PCMU",
                "PCMA",
                "G729",
                "G722",
                "telephone-event",
                "telephone-event"
            ]
        );
        // fmtp 按 pt 各自归属: opus 的 useinbandfec, G729 的 annexb,
        // 没有 fmtp 行的 pt (PCMU/PCMA/G722/telephone-event) 是 None
        let fmtp_of = |pt: u8| {
            audio
                .codecs
                .iter()
                .find(|c| c.payload_type == pt)
                .and_then(|c| c.fmtp.as_deref())
        };
        assert_eq!(fmtp_of(96), Some("useinbandfec=1"));
        assert_eq!(fmtp_of(18), Some("annexb=yes"));
        assert_eq!(fmtp_of(8), None);
        assert_eq!(fmtp_of(9), None);
        // 顺序即对端偏好, 调用方从前到后挑自己支持的 -> 8/PCMA 应可挑中
        let picked = audio
            .codecs
            .iter()
            .find(|c| c.codec == "PCMA" || c.codec == "PCMU")
            .expect("G.711 candidate must be selectable");
        assert_eq!(picked.payload_type, 0); // PCMU 排在 PCMA 前面
    }

    #[test]
    fn parse_answer_rejects_garbage() {
        assert!(parse_answer(b"not sdp at all").is_err());
        // 没有 m= 行
        assert!(parse_answer(b"v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\ns=-\r\nt=0 0\r\n").is_err());
    }
}
