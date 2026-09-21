//! RTP 接收器配置.

use std::path::PathBuf;

use ipcam_core::{AudioCodec, VideoCodec};

use crate::GstStreamError;

/// RTP 接收器配置.
#[derive(Debug, Clone)]
pub struct RtpRecvConfig {
    /// 视频保存路径, None = 不保存视频.
    pub video_path: Option<PathBuf>,
    /// 音频保存路径, None = 不保存音频.
    pub audio_path: Option<PathBuf>,
    /// 监听视频 RTP 的端口.
    pub video_port: u16,
    /// 监听音频 RTP 的端口.
    pub audio_port: u16,
    /// 视频 codec, 与 offer/answer 协商结果一致.
    pub video_codec: VideoCodec,
    /// 音频 codec, 与 offer/answer 协商结果一致.
    pub audio_codec: AudioCodec,
}

impl RtpRecvConfig {
    /// 验证配置合法性.
    pub fn validate(&self) -> Result<(), GstStreamError> {
        if self.video_port == 0 {
            return Err(GstStreamError::InvalidConfig(
                "video_port must not be zero".into(),
            ));
        }
        if self.audio_port == 0 {
            return Err(GstStreamError::InvalidConfig(
                "audio_port must not be zero".into(),
            ));
        }
        if self.video_port == self.audio_port {
            return Err(GstStreamError::InvalidConfig(
                "video_port and audio_port must be different".into(),
            ));
        }
        if self.video_path.is_none() && self.audio_path.is_none() {
            return Err(GstStreamError::InvalidConfig(
                "at least one of video_path or audio_path must be set".into(),
            ));
        }
        Ok(())
    }
}
