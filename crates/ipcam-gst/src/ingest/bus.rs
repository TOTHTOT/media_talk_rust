//! Bus watch + stats ticker: the two side threads owned by a session.

use std::sync::Arc;

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, error, info};

use super::pipeline::TrackState;
use super::stats::{GstStreamHandle, StopSignal, StreamState, wait_or_stop};

/// Outcome of one bus watch round on the session thread.
pub(super) enum BusOutcome {
    /// `stop()` fired (or the bus vanished): leave without reconnecting.
    Stopped,
    /// Terminal error/EOS/timeout text; the session loop decides
    /// between reconnect and `Failed`.
    StreamError(String),
}

/// 阻塞监听 bus, 直到收到终态消息或 `stop()` 触发.
/// 驱动 `Connecting → Playing` 状态转移, 并在 Playing 时记录"无音频轨道".
///
/// # Arguments
///
/// * `pipeline`: 要监听其 bus 的 GStreamer 管线
/// * `tracks`: Track 状态追踪器 (Playing 时检查音频是否已链接)
/// * `handle`: 会话句柄, 用于状态转移
/// * `signal`: 停止信号 (每 250ms 轮询一次)
/// * `attempt`: 退避次数计数器 (Playing 时重置为 0)
///
/// returns: BusOutcome — Stopped (停止) / StreamError(text) (需重连)
///
/// # Examples
///
/// ```
///
/// ```
pub(super) fn watch_bus(
    pipeline: &gst::Pipeline,
    tracks: &Arc<parking_lot::Mutex<TrackState>>,
    handle: &GstStreamHandle,
    signal: &StopSignal,
    attempt: &mut u32,
) -> BusOutcome {
    let Some(bus) = pipeline.bus() else {
        return BusOutcome::StreamError("pipeline has no bus".into());
    };
    let mut announced_no_audio = false;
    loop {
        if signal.is_stopped() {
            return BusOutcome::Stopped;
        }
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(250)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::StateChanged(sc) => {
                let from_pipeline = msg
                    .src()
                    .is_some_and(|s| s == pipeline.upcast_ref::<gst::Element>());
                if from_pipeline && sc.current() == gst::State::Playing {
                    handle.transition(StreamState::Playing);
                    *attempt = 0; // healthy again: reset the backoff
                    if !announced_no_audio && !tracks.lock().audio_linked {
                        announced_no_audio = true;
                        info!("stream has no audio track");
                    }
                }
            }
            gst::MessageView::Error(err) => {
                let text = format!(
                    "{} ({})",
                    err.error(),
                    err.debug().map(|d| d.to_string()).unwrap_or_default()
                );
                if is_auth_error(&text) {
                    error!(err = %text, "rtsp authentication failed");
                } else {
                    error!(err = %text, "pipeline error");
                }
                return BusOutcome::StreamError(text);
            }
            // Network drops often surface as EOS without an ERROR; treat
            // it as a reconnect trigger (unless it came from stop()).
            gst::MessageView::Eos(_) => {
                return BusOutcome::StreamError("EOS".into());
            }
            _ => {}
        }
    }
}

/// Spawn a background thread that logs stats every 10 s.
pub(super) fn spawn_stats_ticker(handle: GstStreamHandle, signal: Arc<StopSignal>) {
    std::thread::spawn(move || {
        use std::time::Duration;
        while wait_or_stop(&signal, Duration::from_secs(10)) {
            if handle.state() != StreamState::Playing {
                continue;
            }
            let stats = handle.stats();
            debug!(
                video_frames = stats.frames_video,
                audio_frames = stats.frames_audio,
                bytes = stats.bytes,
                "ingest session stats"
            );
        }
    });
}

/// True when the error text contains 401-family RTSP auth failure wording.
pub(super) fn is_auth_error(text: &str) -> bool {
    text.contains("401") || text.contains("Unauthorized") || text.contains("authentication failed")
}
