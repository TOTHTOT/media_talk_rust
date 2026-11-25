//! SIP + ICE 媒体会话: 持有 nicesink/nicesrc 管线与可选视频 agent.
//!
//! 顺序纪律: 先停媒体 (pipeline Null, nicesink/nicesrc 停用), 再关 agent.
//! `Drop` 统一执行该顺序, 避免任何退出路径上 agent 先于媒体元件销毁.

use std::time::Duration;

use ipcam_gst::{
    AudioRecv, IceCodecs, IceMediaConfig, IceMediaHandle, RawTaps, VideoFrameSink, start_ice_media,
};
use ipcam_ice::IceAgent;
use tracing::warn;

use crate::session::types::GuiSipConfig;

/// 一路 ICE 通话媒体: 音频 agent 必选, 视频 agent 可选.
pub struct IceCallMedia {
    video_agent: Option<IceAgent>,
    audio_agent: Option<IceAgent>,
    handle: IceMediaHandle,
}

impl IceCallMedia {
    /// agent 必须已连通并喂入对端 ICE; 本函数只负责起收发管线.
    pub fn start(
        video_agent: Option<IceAgent>,
        audio_agent: IceAgent,
        codecs: IceCodecs,
        cfg: &GuiSipConfig,
        tap: &VideoFrameSink,
    ) -> Result<Self, ipcam_gst::GstStreamError> {
        let video_src = if video_agent.is_some() {
            let src = cfg.video_src.as_video_source();
            if src.is_none() {
                warn!("video source is a mic, video send disabled");
            }
            src
        } else {
            None
        };
        let audio_src = cfg.audio_src.as_audio_source();
        if audio_src.is_none() {
            warn!("ICE media: audio source is a camera, audio send disabled");
        }
        let video_tap = video_agent.is_some().then(|| tap.clone());
        let handle = match start_ice_media(&IceMediaConfig {
            video_agent: video_agent.as_ref(),
            audio_agent: Some(&audio_agent),
            video_src,
            audio_src,
            audio_recv: ice_audio_recv(),
            codecs,
            max_width: cfg.max_width,
            taps: RawTaps {
                video: video_tap,
                audio: None,
            },
        }) {
            Ok(handle) => handle,
            Err(e) => {
                // 起管线失败: 两个 agent 还在这里, 必须显式 close 再返回.
                if let Some(video) = video_agent {
                    video.close();
                }
                audio_agent.close();
                return Err(e);
            }
        };
        Ok(Self {
            video_agent,
            audio_agent: Some(audio_agent),
            handle,
        })
    }

    /// 诊断快照: 音频/视频各自的 ICE 状态与选中路径.
    pub async fn debug_state(
        &self,
    ) -> (
        Option<ipcam_ice::IceState>,
        Option<ipcam_ice::SelectedPair>,
        Option<ipcam_ice::IceState>,
        Option<ipcam_ice::SelectedPair>,
    ) {
        let audio = self.audio_agent.as_ref();
        let video = self.video_agent.as_ref();
        let audio_state = audio.map(|agent| *agent.state_rx().borrow());
        let audio_pair = match audio {
            Some(agent) => agent.selected_pair(1).await,
            None => None,
        };
        let video_state = video.map(|agent| *agent.state_rx().borrow());
        let video_pair = match video {
            Some(agent) => agent.selected_pair(1).await,
            None => None,
        };
        (audio_state, audio_pair, video_state, video_pair)
    }

    /// 等已启用的媒体路 Connected. 管线必须先启动 (nicesink/nicesrc 会
    /// 接管 component 的 socket I/O, 部分平台/libnice 版本不开管线连通检查不推进).
    pub async fn wait_connected(&self, timeout: Duration) -> Result<(), ipcam_ice::IceError> {
        let audio = self.audio_agent.as_ref().expect("audio ICE agent alive");
        match self.video_agent.as_ref() {
            Some(video) => {
                let (audio, video) =
                    tokio::join!(audio.wait_connected(timeout), video.wait_connected(timeout),);
                audio?;
                video?;
            }
            None => audio.wait_connected(timeout).await?,
        }
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

/// 音频接收方式: Linux 板载 ALSA / 其他平台走系统默认输出.
pub(crate) fn ice_audio_recv() -> AudioRecv {
    #[cfg(target_os = "linux")]
    {
        AudioRecv::Alsa("default".into())
    }
    #[cfg(not(target_os = "linux"))]
    {
        AudioRecv::Auto
    }
}
