use ipcam_core::{AudioCodec, VideoCodec};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SdpError {
    #[error("missing session description: {0}")]
    MissingField(&'static str),
    #[error("malformed SDP line")]
    Malformed,
}

#[derive(Debug, Clone, Default)]
pub struct SdpSession {
    pub origin: String,
    pub session_name: String,
    pub media: Vec<SdpMedia>,
}

#[derive(Debug, Clone, Default)]
pub struct SdpMedia {
    pub kind: String,
    pub port: u16,
    pub proto: String,
    pub payload_types: Vec<u8>,
    pub attributes: Vec<(String, String)>,
    pub clock_rate: Option<u32>,
    pub video_codec: Option<VideoCodec>,
    pub audio_codec: Option<AudioCodec>,
}

impl SdpMedia {
    pub fn video_codec(&self) -> Option<VideoCodec> {
        if let Some(c) = self.video_codec {
            return Some(c);
        }
        for (k, v) in &self.attributes {
            if k == "rtpmap" {
                if v.to_ascii_uppercase().contains("H264/") {
                    return Some(VideoCodec::H264);
                }
                if v.to_ascii_uppercase().contains("H265/")
                    || v.to_ascii_uppercase().contains("HEVC/")
                {
                    return Some(VideoCodec::H265);
                }
            }
        }
        None
    }

    pub fn audio_codec(&self) -> Option<AudioCodec> {
        if let Some(c) = self.audio_codec {
            return Some(c);
        }
        for (k, v) in &self.attributes {
            if k == "rtpmap" {
                let upper = v.to_ascii_uppercase();
                if upper.contains("MPEG4-GENERIC/") {
                    return Some(AudioCodec::Aac);
                }
                if upper.contains("OPUS/") {
                    return Some(AudioCodec::Opus);
                }
                if upper.contains("PCMA/") {
                    return Some(AudioCodec::G711A);
                }
                if upper.contains("PCMU/") {
                    return Some(AudioCodec::G711U);
                }
            }
        }
        None
    }
}

impl SdpSession {
    pub fn video_codec(&self) -> Option<VideoCodec> {
        self.media
            .iter()
            .filter(|m| m.kind == "video")
            .find_map(|m| m.video_codec())
    }

    pub fn audio_codec(&self) -> Option<AudioCodec> {
        self.media
            .iter()
            .filter(|m| m.kind == "audio")
            .find_map(|m| m.audio_codec())
    }
}

pub fn parse_sdp(input: &str) -> Result<SdpSession, SdpError> {
    let mut s = SdpSession::default();
    let mut current: Option<SdpMedia> = None;

    for raw in input.lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        let (kind, value) = line.split_once('=').ok_or(SdpError::Malformed)?;
        match kind {
            "o" => s.origin = value.to_string(),
            "s" => s.session_name = value.to_string(),
            "m" => {
                if let Some(m) = current.take() {
                    s.media.push(m);
                }
                let mut parts = value.split_whitespace();
                let mk = parts.next().unwrap_or("").to_string();
                let port: u16 = parts.next().unwrap_or("0").parse().unwrap_or(0);
                let proto = parts.next().unwrap_or("").to_string();
                let pts: Vec<u8> = parts.filter_map(|p| p.parse().ok()).collect();
                current = Some(SdpMedia {
                    kind: mk,
                    port,
                    proto,
                    payload_types: pts,
                    attributes: Vec::new(),
                    clock_rate: None,
                    video_codec: None,
                    audio_codec: None,
                });
            }
            "a" => {
                let target = current.get_or_insert_with(SdpMedia::default);
                let (k, v) = match value.split_once(':') {
                    Some((a, b)) => (a.to_string(), b.to_string()),
                    None => (value.to_string(), String::new()),
                };
                if k == "rtpmap" {
                    if let Some((pt_part, rest)) = v.split_once(' ') {
                        let codec_str = rest.split('/').next().unwrap_or("").to_string();
                        let clock = rest.split('/').nth(1).and_then(|s| s.parse().ok());
                        target.clock_rate = clock;
                        if target.kind == "video" {
                            target.video_codec = Some(VideoCodec::from_name(&codec_str));
                        } else if target.kind == "audio" {
                            target.audio_codec = Some(AudioCodec::from_name(&codec_str));
                        }
                        let _ = pt_part;
                    }
                } else if k == "fmtp"
                    && v.contains("sprop-parameter-sets=")
                    && target.kind == "video"
                    && target.video_codec.is_none()
                {
                    target.video_codec = Some(VideoCodec::H264);
                }
                target.attributes.push((k, v));
            }
            _ => {}
        }
    }
    if let Some(m) = current.take() {
        s.media.push(m);
    }
    if s.origin.is_empty() {
        return Err(SdpError::MissingField("o="));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_h264_sdp() {
        let sdp = "v=0\r\n\
o=- 0 0 IN IP4 192.168.1.10\r\n\
s=Test\r\n\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 profile-level-id=42E01E;sprop-parameter-sets=Z0LAHtkA\r\n\
m=audio 0 RTP/AVP 97\r\n\
a=rtpmap:97 mpeg4-generic/16000/2\r\n";
        let s = parse_sdp(sdp).expect("parse");
        assert_eq!(s.media.len(), 2);
        assert_eq!(s.video_codec(), Some(VideoCodec::H264));
        assert_eq!(s.audio_codec(), Some(AudioCodec::Aac));
    }

    #[test]
    fn parses_h265_sdp() {
        let sdp = "v=0\r\no=- 0 0 IN IP4 10.0.0.1\r\ns=H\r\nm=video 0 RTP/AVP 100\r\na=rtpmap:100 H265/90000\r\n";
        let s = parse_sdp(sdp).expect("parse");
        assert_eq!(s.video_codec(), Some(VideoCodec::H265));
    }

    #[test]
    fn parses_empty_session_fails() {
        assert!(parse_sdp("v=0\r\n").is_err());
    }
}
