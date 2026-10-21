//! Pipeline construction: build_pipeline + pad-added dispatch + reconnect attempt.

use std::sync::Arc;

use gstreamer as gst;
use gstreamer::prelude::*;
use parking_lot::Mutex;

use super::stats::GstStreamHandle;
use super::tap::RawTaps;
use super::track::install_pad_added;
use crate::GstStreamError;
use crate::gstutil::make;

/// Per-pipeline pad-added bookkeeping (recreated on every rebuild).
pub(super) struct TrackState {
    pub(super) video_linked: bool,
    pub(super) audio_linked: bool,
}

impl TrackState {
    pub(super) fn new() -> Self {
        Self {
            video_linked: false,
            audio_linked: false,
        }
    }
}

/// Shared pipeline handle for the session thread to swap in new pipelines
/// after reconnect.  `Arc<Mutex<Option<_>>>` so `try_rebuild_pipeline`
/// can atomically replace the live pipeline while the bus thread reads it.
pub(super) type SharedPipeline = Arc<Mutex<Option<gst::Pipeline>>>;

/// 构建新管线 (rtspsrc + 可选 webrtcsink + pad-added 分发).
/// webrtc=false 时不建 webrtcsink (仅本地 tap 模式).
/// 由 `start()` 首次调用, 每次重连时再调用, 必须可重入.
///
/// # Arguments
///
/// * `cfg`: 流配置 (URI, 凭据, latency, 音频输出模式)
/// * `handle`: 会话句柄, 用于状态转移和错误上报
/// * `taps`: 原始帧回调 (视频/音频解码后 tap sink)
///
/// returns: Result<(gst::Pipeline, Arc<Mutex<TrackState>>), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub(super) fn build_pipeline(
    cfg: &super::config::GstStreamConfig,
    handle: GstStreamHandle,
    taps: RawTaps,
) -> Result<(gst::Pipeline, Arc<Mutex<TrackState>>), GstStreamError> {
    let pipeline = gst::Pipeline::new();
    let src = make("rtspsrc")?;
    src.set_property("location", &cfg.uri);
    src.set_property("latency", cfg.latency_ms);
    src.set_property("drop-on-latency", true);
    // `protocols` is a GstRTSPLowerTrans flags value; avoid pulling in
    // gstreamer-rtsp just for it and go through the string setter.
    src.set_property_from_str("protocols", "tcp");
    if let Some((user, pass)) = &cfg.credentials {
        src.set_property("user-id", user);
        src.set_property("user-pw", pass);
    }

    // webrtc=false = 仅本地 tap 模式 (GUI 监控页): 不建 webrtcsink,
    // 音视频轨只剩 tap/alsa 消费者 (见 track.rs link_video/link_audio)
    let ws = if cfg.webrtc {
        let ws = make("webrtcsink")?;
        // Producer identity on the signalling channel; consumers match on
        // `meta.name`. Keep it unique per session.
        ws.set_property_from_str("meta", &format!("meta,name={}", cfg.stream_name));
        // webrtcsink 连到进程内共享的信令服务器（ensure_signalling_server
        // 起在 8443）——注意不是 signalling-server-host/port，那两个属性
        // 是给"自己跑服务器"模式用的，连别人的服务器要走 signaller 的 uri
        let signaller = ws.property::<gst::glib::Object>("signaller");
        signaller.set_property(
            "uri",
            format!("ws://{}:{}", cfg.signalling_host, cfg.signalling_port),
        );
        Some(ws)
    } else {
        None
    };

    match &ws {
        Some(ws) => pipeline
            .add_many([&src, ws])
            .map_err(|e| GstStreamError::Init(format!("pipeline add rtspsrc/webrtcsink: {e}")))?,
        None => pipeline
            .add(&src)
            .map_err(|e| GstStreamError::Init(format!("pipeline add rtspsrc: {e}")))?,
    }

    let tracks = Arc::new(Mutex::new(TrackState::new()));
    install_pad_added(
        &src,
        &pipeline,
        tracks.clone(),
        cfg.audio_output.clone(),
        ws,
        handle,
        taps,
        cfg.tap_max_width,
    );
    Ok((pipeline, tracks))
}

/// Outcome of one reconnect attempt: either a Playing pipeline+tracks,
/// or a string describing why it failed (for error reporting).
pub(super) enum TryRebuildResult {
    Ok(gst::Pipeline, Arc<Mutex<TrackState>>),
    Err(String),
}

/// 从 `cfg` 构建新管线并设为 Playing 状态.
/// 失败时向 `handle` 报告错误.
///
/// # Arguments
///
/// * `cfg`: 流配置 (同 build_pipeline)
/// * `taps`: 原始帧回调
/// * `handle`: 会话句柄, 用于错误上报
///
/// returns: TryRebuildResult — Ok(pipeline, tracks) 成功, Err(msg) 失败
///
/// # Examples
///
/// ```
///
/// ```
pub(super) fn try_rebuild_pipeline(
    cfg: &super::config::GstStreamConfig,
    taps: &RawTaps,
    handle: &GstStreamHandle,
) -> TryRebuildResult {
    match build_pipeline(cfg, handle.clone(), taps.clone()) {
        Ok((p, t)) => match p.set_state(gst::State::Playing) {
            Ok(_) => TryRebuildResult::Ok(p, t),
            Err(e) => TryRebuildResult::Err(format!("rebuilt pipeline set Playing: {e}")),
        },
        Err(e) => TryRebuildResult::Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::config::GstStreamConfig;
    use crate::ingest::stats::GstStreamHandle;
    use crate::ingest::tap::RawTaps;

    fn has_factory(p: &gst::Pipeline, name: &str) -> bool {
        p.iterate_elements().into_iter().any(|e| {
            e.map(|e| e.factory().map(|f| f.name() == name).unwrap_or(false))
                .unwrap_or(false)
        })
    }

    /// 本地查看模式: webrtc=false 时管线只建 rtspsrc, 不建 webrtcsink;
    /// true (默认) 保持现状两个都有
    #[test]
    fn local_mode_skips_webrtcsink() {
        crate::ensure_init_internal().unwrap();
        // CI 环境可能没有 gst-plugins-bad，webrtcsink 不可用时跳过
        if gst::ElementFactory::find("webrtcsink").is_none() {
            tracing::warn!(
                "webrtcsink not available (gst-plugins-bad not installed), skipping test"
            );
            return;
        }
        let local = GstStreamConfig {
            webrtc: false,
            ..Default::default()
        };
        let (p, _t) = build_pipeline(&local, GstStreamHandle::new(), RawTaps::default())
            .expect("local pipeline builds");
        assert!(has_factory(&p, "rtspsrc"));
        assert!(!has_factory(&p, "webrtcsink"));

        let (p2, _t2) = build_pipeline(
            &GstStreamConfig::default(),
            GstStreamHandle::new(),
            RawTaps::default(),
        )
        .expect("webrtc pipeline builds");
        assert!(has_factory(&p2, "webrtcsink"));
    }
}
