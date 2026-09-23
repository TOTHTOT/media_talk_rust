//! RTP 发送配置: 每路媒体从哪来 (TrackSource), 发到哪 (RtpDest/AudioDest).

use std::net::SocketAddr;

use ipcam_core::AudioCodec;

use super::source::TrackSource;

/// 一路媒体的发送目标: 对端收包地址 + 对端 answer 里协商出的 pt
#[derive(Debug, Clone, Copy)]
pub struct RtpDest {
    pub addr: SocketAddr,
    pub payload_type: u8,
    /// 对端解码能力的限宽: Some(w) 时源超宽会降采样到 w 以内
    /// (保持宽高比), None = 不限制 (不插 videoscale/capsfilter).
    /// 门口机给 Some(640), 能力未知的对端先按 640 保守发
    pub max_width: Option<u32>,
}

/// 从 SDP rtpmap 的编码名解析出我们支持发送的编码.
/// None = 不支持发这种 (SIP 对讲场景只发 G.711 两兄弟)
pub fn sendable_audio_codec(name: &str) -> Option<AudioCodec> {
    match AudioCodec::from_name(name) {
        c @ (AudioCodec::G711A | AudioCodec::G711U) => Some(c),
        _ => None,
    }
}

/// 音频路的发送目标: 地址 + pt + answer 协商出的编码.
/// 编码必须取 answer 里的值 -- 对端收窄到 PCMU 我们还发 PCMA,
/// 就是标签和内容都对不上的错包 (RFC 3264)
#[derive(Debug, Clone, Copy)]
pub struct AudioDest {
    pub addr: SocketAddr,
    pub payload_type: u8,
    pub codec: AudioCodec,
}

#[derive(Debug, Clone)]
pub struct RtpSendConfig {
    /// None = 不发这路 (对端没接这路媒体时)
    pub audio: Option<(TrackSource, AudioDest)>,
    pub video: Option<(TrackSource, RtpDest)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SDP rtpmap 编码名解析: 大小写不敏感, 未知编码返回 None
    #[test]
    fn audio_codec_from_codec_name() {
        assert_eq!(sendable_audio_codec("PCMU"), Some(AudioCodec::G711U));
        assert_eq!(sendable_audio_codec("pcmu"), Some(AudioCodec::G711U));
        assert_eq!(sendable_audio_codec("PcMu"), Some(AudioCodec::G711U));
        assert_eq!(sendable_audio_codec("PCMA"), Some(AudioCodec::G711A));
        assert_eq!(sendable_audio_codec("pcma"), Some(AudioCodec::G711A));
        assert_eq!(sendable_audio_codec("opus"), None);
    }
}
