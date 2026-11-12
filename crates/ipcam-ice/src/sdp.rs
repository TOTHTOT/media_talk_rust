//! 标准媒体 SDP 与 ICE 属性的组装/解析.
//!
//! libnice 的 `nice_agent_generate_local_sdp` 只产出 ICE-only SDP
//! (`m=- ... ICE/SDP`). 本模块把 ICE 凭据/候选合并进标准 `m=video` /
//! `m=audio` 媒体段 (sigserver 直连与 SIP offer/answer 共用).
//!
//! 媒体编码固定与 `ipcam-gst::ice_media` 对齐:
//! - video: H264, payload 96, clock 90000
//! - audio: Opus, payload 111, clock 48000, channels 2

use crate::blob::IceBlob;

pub const VIDEO_PT: u8 = 96;
pub const VIDEO_CLOCK: u32 = 90000;
pub const AUDIO_PT: u8 = 111;
pub const AUDIO_CLOCK: u32 = 48000;
pub const AUDIO_CHANNELS: u8 = 2;

/// SDP 组装/解析错误.
#[derive(Debug, thiserror::Error)]
pub enum SdpError {
    #[error("ICE SDP is missing {0}")]
    MissingIce(&'static str),
    #[error("SDP has no {0} media section")]
    MissingMedia(&'static str),
    #[error("SDP has no usable ICE candidate for {0}")]
    MissingCandidate(&'static str),
}

/// 解析后的对端媒体 ICE 信息. 至少一路存在 (解析时校验).
pub struct RemoteMedia {
    pub video: Option<IceBlob>,
    pub audio: Option<IceBlob>,
}

#[derive(Default)]
struct IcePart {
    ufrag: String,
    pwd: String,
    candidates: Vec<String>,
}

impl IcePart {
    fn is_complete(&self) -> bool {
        !self.ufrag.is_empty() && !self.pwd.is_empty()
    }
}

/// 把一路本端 ICE blob (libnice 生成) 包装成标准音视频 SDP.
///
/// `video_ice` 必填; `audio_ice` 为 None 时只输出 `m=video`.
pub fn build_offer(video_ice: &IceBlob, audio_ice: Option<&IceBlob>) -> Result<String, SdpError> {
    let video = parse_ice_blob(video_ice)?;
    let audio = audio_ice.map(parse_ice_blob).transpose()?;
    let session_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut sdp = String::new();
    push_line(&mut sdp, "v=0");
    push_line(&mut sdp, &format!("o=- {session_id} 2 IN IP4 127.0.0.1"));
    push_line(&mut sdp, "s=-");
    push_line(&mut sdp, "t=0 0");

    push_media(
        &mut sdp,
        "video",
        VIDEO_PT,
        "H264",
        VIDEO_CLOCK,
        None,
        &video,
    );
    if let Some(audio) = &audio {
        push_media(
            &mut sdp,
            "audio",
            AUDIO_PT,
            "opus",
            AUDIO_CLOCK,
            Some(AUDIO_CHANNELS),
            audio,
        );
    }
    Ok(sdp)
}

/// build_offer 的 IceBlob 包装: 调用点少一层 from_sdp 样板.
pub fn build_offer_blob(
    video_ice: &IceBlob,
    audio_ice: Option<&IceBlob>,
) -> Result<IceBlob, SdpError> {
    build_offer(video_ice, audio_ice).map(IceBlob::from_sdp)
}

/// 从对端标准媒体 SDP 中提取 video/audio 各自的 ICE 参数.
/// 至少一路带完整 ICE (ufrag+pwd+候选), 否则报错; 只有一路是正常情况.
pub fn parse_offer(sdp: &str) -> Result<RemoteMedia, SdpError> {
    let (session_attrs, sections) = split_sections(sdp);

    let video = sections
        .iter()
        .find(|section| section.media == "video")
        .map(|section| extract_ice(&section.attrs, &session_attrs, "video"))
        .transpose()?;
    let audio = sections
        .iter()
        .find(|section| section.media == "audio")
        .map(|section| extract_ice(&section.attrs, &session_attrs, "audio"))
        .transpose()?;
    if video.is_none() && audio.is_none() {
        return Err(SdpError::MissingMedia("audio/video"));
    }

    Ok(RemoteMedia {
        video: video.map(|part| ice_only_blob(&part)),
        audio: audio.map(|part| ice_only_blob(&part)),
    })
}

/// 把本端 ICE 参数注入一份标准媒体 SDP (SIP offer/answer 文本).
///
/// 按 `m=video` / `m=audio` 段追加 `a=ice-ufrag` / `a=ice-pwd` /
/// `a=candidate` 行; 传入的 blob 必须有对应媒体段, 否则报错.
/// 文本统一输出 CRLF (SIP SDP 约定), 与 `ice_only_blob` 的 LF 不同.
pub fn append_ice(
    sdp: &str,
    video: Option<&IceBlob>,
    audio: Option<&IceBlob>,
) -> Result<String, SdpError> {
    let video = video.map(parse_ice_blob).transpose()?;
    let audio = audio.map(parse_ice_blob).transpose()?;
    if video.is_none() && audio.is_none() {
        return Err(SdpError::MissingMedia("audio/video"));
    }

    let mut out = String::with_capacity(sdp.len() + 1024);
    let mut current: Option<&str> = None;
    let mut seen_video = false;
    let mut seen_audio = false;
    for raw in sdp.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("m=") {
            // 上一个媒体段结束, 在下一段开始前追加它的 ICE 属性.
            push_section_ice(
                &mut out,
                current,
                &video,
                &audio,
                &mut seen_video,
                &mut seen_audio,
            );
            current = rest.split_whitespace().next();
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    push_section_ice(
        &mut out,
        current,
        &video,
        &audio,
        &mut seen_video,
        &mut seen_audio,
    );
    if video.is_some() && !seen_video {
        return Err(SdpError::MissingMedia("video"));
    }
    if audio.is_some() && !seen_audio {
        return Err(SdpError::MissingMedia("audio"));
    }
    Ok(out)
}

/// 本端某个媒体 (video/audio) 的默认 RTP 端点: 指定网卡上的首个 UDP
/// host 候选. 用于 SIP SDP 的 `m=` 端口 (ICE 失败回退普通 RTP 时复用).
pub fn media_endpoint(blob: &IceBlob, ip: std::net::IpAddr) -> Option<std::net::SocketAddr> {
    blob.as_str().lines().find_map(|raw| {
        let fields: Vec<&str> = raw
            .trim()
            .strip_prefix("a=candidate:")?
            .split_whitespace()
            .collect();
        // foundation component transport priority addr port typ kind
        if fields.len() < 8 || fields[1] != "1" || fields[2] != "UDP" || fields[7] != "host" {
            return None;
        }
        let addr: std::net::IpAddr = fields[4].parse().ok()?;
        if addr != ip {
            return None;
        }
        Some(std::net::SocketAddr::new(addr, fields[5].parse().ok()?))
    })
}

fn push_section_ice(
    out: &mut String,
    media: Option<&str>,
    video: &Option<IcePart>,
    audio: &Option<IcePart>,
    seen_video: &mut bool,
    seen_audio: &mut bool,
) {
    let part = match media {
        Some("video") => {
            *seen_video = true;
            video.as_ref()
        }
        Some("audio") => {
            *seen_audio = true;
            audio.as_ref()
        }
        _ => None,
    };
    let Some(part) = part else { return };
    push_line(out, &format!("a=ice-ufrag:{}", part.ufrag));
    push_line(out, &format!("a=ice-pwd:{}", part.pwd));
    for candidate in &part.candidates {
        push_line(out, candidate);
    }
    push_line(out, "a=end-of-candidates");
}

struct Section {
    media: String,
    attrs: Vec<String>,
}

fn split_sections(sdp: &str) -> (Vec<String>, Vec<Section>) {
    let mut session_attrs = Vec::new();
    let mut sections = Vec::new();
    let mut current: Option<Section> = None;

    for raw in sdp.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            let media = rest
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            current = Some(Section {
                media,
                attrs: Vec::new(),
            });
        } else if line.starts_with("a=") {
            match current.as_mut() {
                Some(section) => section.attrs.push(line.to_string()),
                None => session_attrs.push(line.to_string()),
            }
        }
    }
    if let Some(section) = current {
        sections.push(section);
    }
    (session_attrs, sections)
}

fn extract_ice(
    attrs: &[String],
    session_attrs: &[String],
    what: &'static str,
) -> Result<IcePart, SdpError> {
    let mut part = IcePart::default();
    for line in attrs {
        if let Some(v) = line.strip_prefix("a=ice-ufrag:") {
            part.ufrag = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("a=ice-pwd:") {
            part.pwd = v.trim().to_string();
        } else if line.starts_with("a=candidate:")
            && candidate_component(line).is_none_or(|component| component == 1)
        {
            part.candidates.push(line.clone());
        }
    }
    for line in session_attrs {
        if part.ufrag.is_empty() {
            if let Some(v) = line.strip_prefix("a=ice-ufrag:") {
                part.ufrag = v.trim().to_string();
            }
        }
        if part.pwd.is_empty() {
            if let Some(v) = line.strip_prefix("a=ice-pwd:") {
                part.pwd = v.trim().to_string();
            }
        }
    }
    if !part.is_complete() {
        return Err(SdpError::MissingIce(what));
    }
    if part.candidates.is_empty() {
        return Err(SdpError::MissingCandidate(what));
    }
    Ok(part)
}

fn candidate_component(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("a=candidate:")?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn parse_ice_blob(blob: &IceBlob) -> Result<IcePart, SdpError> {
    let mut part = IcePart::default();
    for raw in blob.as_str().lines() {
        let line = raw.trim();
        if let Some(v) = line.strip_prefix("a=ice-ufrag:") {
            part.ufrag = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("a=ice-pwd:") {
            part.pwd = v.trim().to_string();
        } else if line.starts_with("a=candidate:")
            && candidate_component(line).is_none_or(|component| component == 1)
        {
            part.candidates.push(line.to_string());
        }
    }
    if !part.is_complete() {
        return Err(SdpError::MissingIce("local"));
    }
    if part.candidates.is_empty() {
        return Err(SdpError::MissingCandidate("local"));
    }
    Ok(part)
}

/// 重建 libnice 可直接喂入 `nice_agent_parse_remote_sdp` 的 ICE-only SDP.
///
/// 必须用 `\n` 分行: libnice 只按 `\n` 切分, 且候选行解析不做 trim,
/// CRLF 会让最后一个 token 变成 `host\r`, 类型判断失败.
fn ice_only_blob(part: &IcePart) -> IceBlob {
    // 与 libnice `nice_agent_generate_local_sdp` 的输出形状保持一致:
    // 只有 m=-/c=/a=ice-*/a=candidate, 不带 v=/o=/s=/t= 会话行.
    let (addr, port) = part
        .first_udp_candidate()
        .unwrap_or_else(|| ("0.0.0.0".to_string(), 9));
    let ipver = if addr.contains(':') { "IP6" } else { "IP4" };

    let mut lines = vec![
        format!("m=- {port} ICE/SDP"),
        format!("c=IN {ipver} {addr}"),
        format!("a=ice-ufrag:{}", part.ufrag),
        format!("a=ice-pwd:{}", part.pwd),
    ];
    lines.extend(part.candidates.iter().cloned());
    lines.push(String::new());
    IceBlob::from_sdp(lines.join("\n"))
}

impl IcePart {
    fn first_udp_candidate(&self) -> Option<(String, u16)> {
        self.candidates.iter().find_map(|line| {
            let fields: Vec<&str> = line
                .strip_prefix("a=candidate:")?
                .split_whitespace()
                .collect();
            // foundation component transport priority addr port typ ...
            if fields.len() < 6 || fields[2] != "UDP" {
                return None;
            }
            let addr = fields[4].to_string();
            let port = fields[5].parse().ok()?;
            Some((addr, port))
        })
    }
}

fn push_media(
    sdp: &mut String,
    media: &str,
    pt: u8,
    encoding: &str,
    clock: u32,
    channels: Option<u8>,
    ice: &IcePart,
) {
    push_line(sdp, &format!("m={media} 9 RTP/AVP {pt}"));
    push_line(sdp, "c=IN IP4 0.0.0.0");
    let rtpmap = match channels {
        Some(channels) => format!("a=rtpmap:{pt} {encoding}/{clock}/{channels}"),
        None => format!("a=rtpmap:{pt} {encoding}/{clock}"),
    };
    push_line(sdp, &rtpmap);
    if media == "video" {
        push_line(sdp, &format!("a=fmtp:{pt} packetization-mode=1"));
    }
    push_line(sdp, &format!("a=ice-ufrag:{}", ice.ufrag));
    push_line(sdp, &format!("a=ice-pwd:{}", ice.pwd));
    for candidate in &ice.candidates {
        push_line(sdp, candidate);
    }
    push_line(sdp, "a=end-of-candidates");
}

fn push_line(sdp: &mut String, line: &str) {
    sdp.push_str(line);
    sdp.push_str("\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL_ICE: &str = "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        t=0 0\r\n\
        a=ice-ufrag:abcd\r\n\
        a=ice-pwd:efghEFGHefghEFGHefgh\r\n\
        m=- 9 ICE/SDP\r\n\
        c=IN IP4 0.0.0.0\r\n\
        a=candidate:1 1 UDP 2015363327 192.168.2.109 65025 typ host\r\n\
        a=end-of-candidates\r\n";

    #[test]
    fn builds_media_sections_with_ice() {
        let ice = IceBlob::from_sdp(LOCAL_ICE.into());
        let sdp = build_offer(&ice, Some(&ice)).unwrap();
        assert!(sdp.contains("m=video 9 RTP/AVP 96"));
        assert!(sdp.contains("a=rtpmap:96 H264/90000"));
        assert!(sdp.contains("m=audio 9 RTP/AVP 111"));
        assert!(sdp.contains("a=rtpmap:111 opus/48000/2"));
        assert_eq!(sdp.matches("a=ice-ufrag:abcd").count(), 2);
        assert_eq!(sdp.matches("a=candidate:").count(), 2);
    }

    #[test]
    fn parses_remote_media_ice() {
        let ice = IceBlob::from_sdp(LOCAL_ICE.into());
        let sdp = build_offer(&ice, Some(&ice)).unwrap();
        let remote = parse_offer(&sdp).unwrap();
        let video = remote.video.expect("video section");
        assert!(video.as_str().contains("a=candidate:"));
        // libnice 按 "\n" 切分且不 trim, 重建的 ICE-only SDP 必须无 CR.
        assert!(!video.as_str().contains('\r'));
        let audio = remote.audio.expect("audio section");
        assert!(audio.as_str().contains("a=candidate:"));
        assert!(!audio.as_str().contains('\r'));
    }

    #[test]
    fn injects_ice_into_sip_media_sdp() {
        let ice = IceBlob::from_sdp(LOCAL_ICE.into());
        let with_ice = build_offer(&ice, Some(&ice)).unwrap();
        // 去掉 build_offer 自带的 ICE 行, 模拟一份普通 SIP offer.
        let plain: String = with_ice
            .lines()
            .filter(|l| !l.starts_with("a=ice-") && !l.starts_with("a=candidate:"))
            .map(|l| format!("{l}\r\n"))
            .collect();
        let out = append_ice(&plain, Some(&ice), Some(&ice)).unwrap();
        assert_eq!(out.matches("a=ice-ufrag:abcd").count(), 2);
        assert_eq!(out.matches("a=candidate:").count(), 2);
        // ICE 属性必须落在各自媒体段内 (video 段在 audio 段之前).
        let video_pos = out.find("m=video").unwrap();
        let audio_pos = out.find("m=audio").unwrap();
        let first_ufrag = out.find("a=ice-ufrag:abcd").unwrap();
        let last_ufrag = out.rfind("a=ice-ufrag:abcd").unwrap();
        assert!(video_pos < first_ufrag && first_ufrag < audio_pos);
        assert!(audio_pos < last_ufrag);
        // SIP 文本统一 CRLF, 且不能有 CRLF -> CR 的重复.
        assert!(out.contains("\r\n"));
        assert!(!out.contains("\r\r"));
    }

    #[test]
    fn media_endpoint_picks_udp_host_on_local_ip() {
        let ice = IceBlob::from_sdp(LOCAL_ICE.into());
        let ip = "192.168.2.109".parse().unwrap();
        assert_eq!(
            media_endpoint(&ice, ip),
            Some(std::net::SocketAddr::new(ip, 65025))
        );
        assert_eq!(media_endpoint(&ice, "192.168.2.110".parse().unwrap()), None);
    }

    #[test]
    fn missing_audio_is_allowed() {
        let ice = IceBlob::from_sdp(LOCAL_ICE.into());
        let sdp = build_offer(&ice, None).unwrap();
        assert!(!sdp.contains("m=audio"));
        let remote = parse_offer(&sdp).unwrap();
        assert!(remote.audio.is_none());
    }
}
