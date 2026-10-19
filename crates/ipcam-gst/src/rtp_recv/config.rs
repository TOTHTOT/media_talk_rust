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
    /// tap 分支的显示限宽: Some(w) 时解码后先 videoscale 降采样到 w
    /// 以内 (保持宽高比) 再吐给回调; None = 不限制 (不插 videoscale).
    /// 仅 tap 分支生效, 不影响存盘.
    ///
    /// 为什么要缩: tap 吐的帧最终只显示在屏幕上 640 宽左右的 widget
    /// 里, 但缩放前每一级都要按全尺寸买单 — 720p@25fps 的 RGBA ≈
    /// 92MB/s, 要过 appsink → GUI 的 memcpy 和纹理上传两道, 其中 ~75%
    /// 的像素搬过去只为被立刻缩掉 (ARM 板子上这是帧率杀手). 在 gst
    /// 里解码完就缩 (videoscale), 后面的 memcpy/上传全按小尺寸走,
    /// 带宽降到 ~23MB/s; 比在 GUI 层缩省得多 — 越早缩越省
    pub tap_max_width: Option<u32>,
    /// 视频路共享 UDP socket: Some 时 udpsrc 直接用它收 (忽略 video 里
    /// 的端口配置), 发送器可用同一 socket 发 — 对称 RTP, 见
    /// [`crate::bind_udp_socket`]. None = udpsrc 自建 socket (旧行为)
    pub video_socket: Option<crate::SharedUdpSocket>,
    /// 音频路共享 UDP socket, 同 `video_socket`
    pub audio_socket: Option<crate::SharedUdpSocket>,
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
            .field("tap_max_width", &self.tap_max_width)
            .field("video_socket", &self.video_socket.is_some())
            .field("audio_socket", &self.audio_socket.is_some())
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
