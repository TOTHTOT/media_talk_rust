//! RTP 接收器配置.

use std::path::PathBuf;

use ipcam_core::{AudioCodec, VideoCodec};

use crate::GstStreamError;

/// RTP 接收器配置: 收进的两路媒体可存盘/本地播放, 单开或同时开.
#[derive(Debug, Clone)]
pub struct RtpRecvConfig {
    /// Some = 音视频合并存 mp4 到该路径. mp4 的 moov 索引只在 EOS 时
    /// 写入, 必须走 `RtpReceiver::stop()` 收尾, 强杀进程得到的文件播不了
    pub path: Option<PathBuf>,
    /// true = 本地播放: 音频解码后进 autoaudiosink (扬声器), 视频解码后
    /// 进 autovideosink (屏幕); 和 path 可同时开 (tee 分叉), 是全双工
    /// 对讲的扬声器通路. 注意没接 AEC, 同机麦克风回传时建议插耳机
    pub playback: bool,
    /// (监听端口, 协商出的 codec); None = 不收视频
    pub video: Option<(u16, VideoCodec)>,
    /// (监听端口, 协商出的 codec); None = 不收音频.
    /// 存盘时 G.711 会解码后转 opus 进 mp4 (mp4 容器不认 G.711 载荷)
    pub audio: Option<(u16, AudioCodec)>,
}

impl RtpRecvConfig {
    /// 验证配置合法性.
    pub fn validate(&self) -> Result<(), GstStreamError> {
        if self.path.is_none() && !self.playback {
            return Err(GstStreamError::InvalidConfig(
                "either path (record) or playback must be set".into(),
            ));
        }
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
