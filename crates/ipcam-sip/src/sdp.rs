//! SDP offer/answer 的类型化构造与解析 (基于 sdp-rs).
//!
//! 不手拼字符串: 字段顺序, `\r\n`, 必填行 (v/o/s/t) 错一个对端就
//! 直接拒. `SessionDescription` 实现了 `Display`/`FromStr`, 构造完
//! `to_string()` 即发出去, 收到 answer `try_from` 读回来, 天然可
//! round-trip 测试.

use anyhow::{Result, anyhow};
use sdp_rs::lines::attribute::Rtpmap;
use sdp_rs::lines::common::{Addrtype, Nettype};
use sdp_rs::lines::connection::ConnectionAddress;
use sdp_rs::lines::media::{MediaType, ProtoType};
use sdp_rs::lines::{Attribute, Connection, Media, Origin, SessionName, Version};
use sdp_rs::{MediaDescription, SessionDescription, Time};
use std::net::{IpAddr, SocketAddr};
use tracing::info;
use vec1::vec1;

/// RFC 3551 静态 payload type
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
        .push(video_media_description(video_port));
    sdp
}

fn video_media_description(port: u16) -> MediaDescription {
    MediaDescription {
        media: Media {
            media: MediaType::Video,
            port,
            num_of_ports: None,
            proto: ProtoType::RtpAvp,
            fmt: PT_H264.to_string(),
        },
        info: None,
        connections: vec![],
        bandwidths: vec![],
        key: None,
        attributes: vec![
            Attribute::Rtpmap(Rtpmap {
                payload_type: PT_H264 as u32,
                encoding_name: "H264".into(),
                clock_rate: 90000,
                encoding_params: None,
            }),
            // profile-level-id=42e01f: Baseline profile level 3.1.
            // 不写 packetization-mode = mode 0 (单 NAL 模式): 门口机/
            // 室内机的 RTP 接收器不认 FU-A 分片, 实测 Linphone (mode 0)
            // 能出画面而我们 mode 1 黑屏. 代价是片源必须切成小于 MTU 的
            // slice (rtp_send 的测试片源已按 slice-max-size=1300 重编码).
            // sdp-rs 0.2.1 的 Attribute 枚举没有 Fmtp 变体, 走 Other,
            // Display 出来就是标准 a=fmtp:... 行
            Attribute::Other(
                "fmtp".into(),
                Some(format!("{PT_H264} profile-level-id=42e01f")),
            ),
            Attribute::Sendrecv,
        ],
    }
}

/// 构造音频通话的 SDP offer (单路 audio, sendrecv).
///
/// `local_ip`/`port` 是本端 RTP 收包地址 -- 必须是对端路由可达的
/// LAN 地址 (同 SipClient 的 Contact), 不能是 127.0.0.1.
///
/// 这个结构体 `to_string()` 出来的就是下面这段 SDP (以 PCMA 为例),
/// 逐行对应结构体字段:
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
    let addrtype = match local_ip {
        IpAddr::V4(_) => Addrtype::Ip4,
        IpAddr::V6(_) => Addrtype::Ip6,
    };
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
                clock_rate: 8000,      // G.711 家族固定 8kHz 采样
                encoding_params: None, // 声道数等附加参数, 单声道留空
            })
        })
        .collect();
    attributes.push(Attribute::Ptime(20.0));
    attributes.push(Attribute::Sendrecv);
    SessionDescription {
        // v=0 -- SDP 版本号, RFC 4566 定死就是 0, 没有 1
        version: Version::V0,
        // o=<用户名> <会话ID> <会话版本> <网络类型> <地址类型> <地址>
        // 发起方身份标识. 对端基本不看内容, 只看格式合法性
        origin: Origin {
            username: "-".into(),
            // sess_id/sess_version 本应每次会话唯一 (常用 NTP 时间戳),
            // 重新 INVITE 改参数时 sess_version 要 +1 表示 "新版本的描述".
            // 骨架阶段固定 "1" 够用 -- 对讲场景不做会话内 re-INVITE 改参数
            sess_id: "1".into(),
            sess_version: "1".into(),
            nettype: Nettype::In,       // IN = Internet, 目前只有这一个合法值
            addrtype: addrtype.clone(), // IP4 / IP6
            unicast_address: local_ip,
        },
        // s= 会话名, 必填行. 内容无所谓, 很多设备直接写 "-",
        // 这里写项目名纯粹是抓包时好认
        session_name: SessionName::new("ipcam-sip".into()),
        // i=/u=/e=/p=: 会话描述, 链接, 邮箱, 电话 -- 纯展示用元信息,
        // 设备间通话全都不填
        session_info: None,
        uri: None,
        emails: vec![],
        phones: vec![],
        // c=IN IP4 <地址> -- 最重要的一行: 告诉对端 "把 RTP 发到这个 IP".
        // 写错 (比如 127.0.0.1) 的典型症状: 信令全通, 呼叫建立, 但单向或
        // 双向无声. ttl/numaddr 只有组播才用, 单播留空
        connection: Some(Connection {
            nettype: Nettype::In,
            addrtype,
            connection_address: ConnectionAddress {
                base: local_ip,
                ttl: None,
                numaddr: None,
            },
        }),
        // b= 带宽建议, 对讲场景不声明, 让对方按编码默认来
        bandwidths: vec![],
        // t=<开始> <结束> -- 会话有效时间 (NTP 秒). 0 0 = 永久有效.
        // SDP 规范强制至少一条 t= 行, 所以 sdp-rs 用 Vec1 (非空 Vec)
        // 从类型上杜绝漏写 -- 这是类型化构造相对手拼的第一个好处
        times: vec1![Time {
            active: sdp_rs::lines::Active { start: 0, stop: 0 },
            // r=/z=: 周期性会话的时间表和时区修正, SIP 通话用不到
            repeat: vec![],
            zone: None,
        }],
        // k= 媒体加密密钥 (明文传输, 早已废弃), SRTP 走别的机制
        key: None,
        // 会话级 a= 属性放这里, 我们的属性都是媒体级的, 故为空
        attributes: vec![],
        media_descriptions: vec![MediaDescription {
            // m=<类型> <端口> <协议> <载荷类型列表>
            // audio + RTP/AVP (裸 RTP/UDP; SAVP 才是 SRTP).
            // fmt 里列出所有支持的编码, 对端 answer 从中挑一个
            // 它支持的 -- 只给一个就是 "没得挑, 不行就拒"
            media: Media {
                media: MediaType::Audio,
                port,
                num_of_ports: None, // 组播端口组才用, 单播留空
                proto: ProtoType::RtpAvp,
                fmt,
            },
            info: None,
            // 媒体级 c= 为空 -> 继承上面的会话级 c= (RFC 4566 的继承规则).
            // 多路媒体各发各的地址时才需要在这里单独写
            connections: vec![],
            bandwidths: vec![],
            key: None,
            // a=rtpmap:<pt> <编码名>/<时钟> -- 把数字 pt 映射到具体编码.
            // PCMA(8)/PCMU(0) 是 RFC 3551 静态分配的, 这行其实可省,
            // 写上是为了显式可读; 动态 pt (96-127, 比如 H264 视频)
            // 这行就是强制的, 少了对端直接不认.
            // a=ptime:20 -- 每个 RTP 包承载 20ms 音频 (G.711 即 160 字节
            // 载荷). 对端按这个节奏发包, 收端 jitter buffer 按它估算.
            // a=sendrecv -- 方向协商: 双向收发. 对讲通话必须是它;
            // sendonly/recvonly 用于单向广播/监听场景
            attributes,
        }],
    }
}

/// 从 answer 里协商出的对端媒体信息 -- 拿到它就能往这个地址发/收 RTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerMedia {
    pub addr: SocketAddr,
    pub payload_type: u8,
    pub codec: String,
}

/// 解析对端 (200 OK 或 18x) 带回的 SDP answer, 只取第一路媒体.
///
/// 音视频双路的场景用 `parse_answer_all`. m= 行没带 c= 时回落到
/// session 级 c= (RFC 4566 允许的继承).
pub fn parse_answer(body: &[u8]) -> Result<PeerMedia> {
    let text = std::str::from_utf8(body)?;
    let sdp = SessionDescription::try_from(text).map_err(|e| anyhow!("invalid SDP: {e}"))?;
    let media = sdp
        .media_descriptions
        .first()
        .ok_or_else(|| anyhow!("answer has no media description"))?;
    peer_from_media(sdp.connection.as_ref(), media)
}

/// 双路协商结果: audio/video 各自独立, 对端可能只接了一路
/// (比如老门禁没有摄像头, answer 里只有 m=audio).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerMedias {
    pub audio: Option<PeerMedia>,
    pub video: Option<PeerMedia>,
}

/// 解析 answer, 按媒体类型分别提取 audio/video 两路的协商结果.
/// 两路都没有才算失败; 只有一路是正常情况, 不算错.
pub fn parse_answer_all(body: &[u8]) -> Result<PeerMedias> {
    let text = std::str::from_utf8(body)?;
    let sdp = SessionDescription::try_from(text).map_err(|e| anyhow!("invalid SDP: {e}"))?;
    info!("calling, answer sdp = {}", sdp.to_string());
    let mut out = PeerMedias::default();
    for media in &sdp.media_descriptions {
        let peer = peer_from_media(sdp.connection.as_ref(), media)?;
        if media.media.media == MediaType::Audio {
            out.audio = Some(peer);
        } else if media.media.media == MediaType::Video {
            out.video = Some(peer);
        }
    }
    if out.audio.is_none() && out.video.is_none() {
        anyhow::bail!("answer has no audio/video media");
    }
    Ok(out)
}

/// 从一路 MediaDescription 提取对端地址/编码. session_conn 是会话级
/// c=, 媒体级没写 c= 时按 RFC 4566 继承它.
fn peer_from_media(
    session_conn: Option<&Connection>,
    media: &MediaDescription,
) -> Result<PeerMedia> {
    let payload_type: u8 = media
        .media
        .fmt
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("m= line has no payload type"))?
        .parse()
        .map_err(|_| anyhow!("unsupported fmt list: {}", media.media.fmt))?;

    let ip = media
        .connections
        .first()
        .or(session_conn)
        .ok_or_else(|| anyhow!("answer has no connection address"))?
        .connection_address
        .base;

    // 动态 pt (>=96) 从 rtpmap 拿编码名; 静态 pt 查 RFC 3551 表,
    // 都没有就原样报数字, 不猜
    let codec = media
        .attributes
        .iter()
        .find_map(|a| match a {
            Attribute::Rtpmap(r) if r.payload_type == payload_type as u32 => {
                Some(r.encoding_name.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| codec_name(payload_type).into());

    Ok(PeerMedia {
        addr: SocketAddr::new(ip, media.media.port),
        payload_type,
        codec,
    })
}

/// RFC 3551 静态 payload type 表 (只列音频里常见的), 未知动态 pt
/// 原样返回数字串
fn codec_name(payload_type: u8) -> &'static str {
    match payload_type {
        PT_PCMU => "PCMU",
        3 => "GSM",
        PT_PCMA => "PCMA",
        9 => "G722",
        18 => "G729",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

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
    fn parse_answer_rejects_garbage() {
        assert!(parse_answer(b"not sdp at all").is_err());
        // 没有 m= 行
        assert!(parse_answer(b"v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\ns=-\r\nt=0 0\r\n").is_err());
    }
}
