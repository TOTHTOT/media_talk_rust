//! SIP + ICE 媒体会话: 持有 nicesink/nicesrc 管线与两个独立 agent.
//!
//! 顺序纪律: 先停媒体 (pipeline Null, nicesink/nicesrc 停用), 再关 agent.
//! `Drop` 统一执行该顺序, 避免任何退出路径上 agent 先于媒体元件销毁.

use std::time::Duration;

use ipcam_gst::{
    AudioRecv, IceMediaConfig, IceMediaHandle, RawTaps, TrackSource, VideoFrameSink,
    start_ice_media,
};
use ipcam_ice::IceAgent;
use tracing::warn;

use crate::session::types::GuiSipConfig;

/// 一路 ICE 通话媒体: 视频 agent + 音频 agent + 收发管线.
pub struct IceCallMedia {
    video_agent: Option<IceAgent>,
    audio_agent: Option<IceAgent>,
    handle: IceMediaHandle,
}

impl IceCallMedia {
    /// agent 必须已连通并喂入对端 ICE; 本函数只负责起收发管线.
    pub fn start(
        video_agent: IceAgent,
        audio_agent: IceAgent,
        cfg: &GuiSipConfig,
        tap: &VideoFrameSink,
    ) -> Result<Self, ipcam_gst::GstStreamError> {
        let video_src = match &cfg.video_src {
            TrackSource::Mic => None,
            src => Some(src.clone()),
        };
        let audio_send = matches!(&cfg.audio_src, TrackSource::Mic);
        if !audio_send {
            warn!("ICE media: audio source is not the mic, audio send disabled");
        }
        let handle = match start_ice_media(&IceMediaConfig {
            video_agent: Some(&video_agent),
            audio_agent: Some(&audio_agent),
            video_src,
            audio_send,
            audio_recv: ice_audio_recv(),
            max_width: cfg.max_width,
            taps: RawTaps {
                video: Some(tap.clone()),
                audio: None,
            },
        }) {
            Ok(handle) => handle,
            Err(e) => {
                // 起管线失败: 两个 agent 还在这里, 必须显式 close 再返回.
                video_agent.close();
                audio_agent.close();
                return Err(e);
            }
        };
        Ok(Self {
            video_agent: Some(video_agent),
            audio_agent: Some(audio_agent),
            handle,
        })
    }

    /// 等两路媒体都 Connected. 管线必须先启动 (nicesink/nicesrc 会接管
    /// component 的 socket I/O, 部分平台/libnice 版本不开管线连通检查不推进).
    pub async fn wait_connected(&self, timeout: Duration) -> Result<(), ipcam_ice::IceError> {
        let video = self.video_agent.as_ref().expect("video ICE agent alive");
        let audio = self.audio_agent.as_ref().expect("audio ICE agent alive");
        let (video, audio) =
            tokio::join!(video.wait_connected(timeout), audio.wait_connected(timeout),);
        video?;
        audio?;
        Ok(())
    }
}

impl Drop for IceCallMedia {
    fn drop(&mut self) {
        // 先停媒体, 再关 agent (顺序纪律).
        self.handle.stop();
        if let Some(video) = self.video_agent.take() {
            video.close();
        }
        if let Some(audio) = self.audio_agent.take() {
            audio.close();
        }
    }
}

/// 音频接收方式: Linux 板载扬声器 / 其他平台仅 tap.
pub(crate) fn ice_audio_recv() -> AudioRecv {
    #[cfg(target_os = "linux")]
    {
        AudioRecv::Alsa("default".into())
    }
    #[cfg(not(target_os = "linux"))]
    {
        AudioRecv::TapOnly
    }
}
