//! RTP 接收器配置.

use std::path::PathBuf;

use ipcam_core::{AudioCodec, VideoCodec};

use crate::GstStreamError;

/// RTP 接收器配置: 收进的两路媒体可存盘/本地播放, 单开或同时开.
#[derive(Clone)]
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
    /// 视频帧 tap: Some 时视频解码后经 appsink 吐 RGBA 裸帧给回调
    /// (GUI 纹理通路), 取代 autovideosink 播放分支; 不影响音频 playback
    /// 和存盘. 回调跑在 GStreamer 流线程, 别在里面碰 UI 状态
    pub video_tap: Option<crate::VideoFrameSink>,
}

/// VideoFrameSink 是回调闭包没有 Debug, 手写 impl 只标个占位
impl std::fmt::Debug for RtpRecvConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtpRecvConfig")
            .field("path", &self.path)
            .field("playback", &self.playback)
            .field("video", &self.video)
            .field("audio", &self.audio)
            .field("video_tap", &self.video_tap.as_ref().map(|_| "..."))
            .finish()
    }
}

impl RtpRecvConfig {
    /// 验证配置合法性.
    pub fn validate(&self) -> Result<(), GstStreamError> {
        if self.path.is_none() && !self.playback && self.video_tap.is_none() {
            return Err(GstStreamError::InvalidConfig(
                "one of path (record), playback or video_tap must be set".into(),
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
