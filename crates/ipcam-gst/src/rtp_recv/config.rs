//! RTP 接收器配置.

use std::path::PathBuf;

use ipcam_core::{AudioCodec, VideoCodec};

use crate::GstStreamError;

/// RTP 接收器配置: 音视频合进同一个 mp4.
#[derive(Debug, Clone)]
pub struct RtpRecvConfig {
    /// 输出 mp4 路径. mp4 的 moov 索引只在 EOS 时写入, 必须走
    /// `RtpReceiver::stop()` 收尾, 强杀进程得到的文件播不了
    pub path: PathBuf,
    /// (监听端口, 协商出的 codec); None = 不收视频
    pub video: Option<(u16, VideoCodec)>,
    /// (监听端口, 协商出的 codec); None = 不收音频.
    /// G.711 会解码后转 opus 进 mp4 (mp4 容器不认 G.711 载荷)
    pub audio: Option<(u16, AudioCodec)>,
}

impl RtpRecvConfig {
    /// 验证配置合法性.
    pub fn validate(&self) -> Result<(), GstStreamError> {
        if self.video.is_none() && self.audio.is_none() {
            return Err(GstStreamError::InvalidConfig(
                "at least one of video or audio must be set".into(),
            ));
        }
        let ports = self
            .video
            .map(|(p, _)| ("video", p))
            .into_iter()
            .chain(self.audio.map(|(p, _)| ("audio", p)));
        for (kind, port) in ports {
            if port == 0 {
                return Err(GstStreamError::InvalidConfig(format!(
                    "{kind} port must not be zero"
                )));
            }
        }
        if let (Some((vp, _)), Some((ap, _))) = (self.video, self.audio) {
            if vp == ap {
                return Err(GstStreamError::InvalidConfig(
                    "video and audio ports must be different".into(),
                ));
            }
        }
        Ok(())
    }
}
