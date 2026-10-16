//! Session lifecycle: `start()` entry + session thread that watches the
//! bus and reconnects on failure.
//!
//! The actual pipeline construction lives in [`super::pipeline`],
//! track linking in [`super::track`], and bus watching in [`super::bus`].

use std::sync::Arc;
use std::thread;

use gstreamer as gst;
use gstreamer::prelude::*;
use parking_lot::Mutex;
use tracing::{error, info};

use super::bus::{BusOutcome, is_auth_error, spawn_stats_ticker, watch_bus as bus_watch};
use super::config::GstStreamConfig;
use super::pipeline::{TrackState, build_pipeline, try_rebuild_pipeline};
use super::stats::{GstStreamHandle, StopSignal, StreamState, wait_or_stop};
use super::tap::RawTaps;
use crate::GstStreamError;

/// Reconnect loop outcome: either a rebuilt+Playing pipeline, or session end.
enum ReconnectOutcome {
    Reconnected(gst::Pipeline, Arc<Mutex<TrackState>>),
    Failed,
}

/// 入口: 构建初始管线, 启动会话线程, 返回句柄供调用方控制生命周期.
///
/// # Arguments
///
/// * `cfg`: 流配置 (URI, 凭据, latency, 音频输出模式)
/// * `taps`: 原始帧回调 (视频/音频解码后 tap sink)
///
/// returns: Result<GstStreamHandle, GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub(crate) fn start(
    cfg: GstStreamConfig,
    taps: RawTaps,
) -> Result<GstStreamHandle, GstStreamError> {
    gst::init().map_err(|e| GstStreamError::Init(format!("gst init: {e}")))?;

    let handle = GstStreamHandle::new();
    let (pipeline, tracks) = build_pipeline(&cfg, handle.clone(), taps.clone())?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| GstStreamError::Init(format!("pipeline set Playing: {e}")))?;

    let signal = Arc::new(StopSignal::new());
    let current: super::pipeline::SharedPipeline = Arc::new(Mutex::new(Some(pipeline.clone())));
    spawn_session_loop(
        pipeline,
        tracks,
        cfg,
        taps,
        handle.clone(),
        current.clone(),
        signal.clone(),
    );
    spawn_stats_ticker(handle.clone(), signal.clone());

    let signal_for_stop = signal.clone();
    let current_for_stop = current.clone();
    handle.set_stop_hook(move || {
        signal_for_stop.stop();
        if let Some(p) = current_for_stop.lock().as_ref() {
            let _ = p.set_state(gst::State::Null);
        }
    });

    Ok(handle)
}

/// 会话线程: 监控 bus, 出错后指数退避重连.
/// 认证失败和重试耗尽直接转 `Failed`; `stop()` 立即中断退避.
///
/// # Arguments
///
/// * `pipeline`: 初始 GStreamer 管线
/// * `tracks`: 初始 track 状态追踪器
/// * `cfg`: 流配置 (用于重连参数)
/// * `taps`: 原始帧回调
/// * `handle`: 会话句柄 (状态/错误上报)
/// * `current`: 共享管线槽 (重连成功时交换)
/// * `signal`: 停止信号, 用于优雅关闭
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
fn spawn_session_loop(
    pipeline: gst::Pipeline,
    tracks: Arc<Mutex<TrackState>>,
    cfg: GstStreamConfig,
    taps: RawTaps,
    handle: GstStreamHandle,
    current: super::pipeline::SharedPipeline,
    signal: Arc<StopSignal>,
) {
    thread::spawn(move || {
        let mut pipeline = pipeline;
        let mut tracks = tracks;
        let mut attempt = 0u32;

        'outer: loop {
            let err_text = match watch_bus(&pipeline, &tracks, &handle, &signal, &mut attempt) {
                BusOutcome::Stopped => break 'outer,
                BusOutcome::StreamError(t) => t,
            };
            handle.note_error(err_text.clone());
            if is_auth_error(&err_text) {
                error!(err = %err_text, "authentication failed, not retrying");
                handle.transition(StreamState::Failed);
                break 'outer;
            }
            let _ = pipeline.set_state(gst::State::Null);

            match reconnect_session(&cfg, &taps, &handle, &signal, &mut attempt, &current) {
                ReconnectOutcome::Reconnected(p, t) => {
                    pipeline = p;
                    tracks = t;
                }
                ReconnectOutcome::Failed => break 'outer,
            }
        }

        let _ = pipeline.set_state(gst::State::Null);
        *current.lock() = None;
    });
}

/// Block on the bus until a terminal message arrives or `stop()` fires.
fn watch_bus(
    pipeline: &gst::Pipeline,
    tracks: &Arc<parking_lot::Mutex<TrackState>>,
    handle: &GstStreamHandle,
    signal: &StopSignal,
    attempt: &mut u32,
) -> BusOutcome {
    bus_watch(pipeline, tracks, handle, signal, attempt)
}

/// 重连循环: 退避 → 重建管线 → 设 Playing → 更新共享状态,
/// 或报告错误并重试. 返回 `Reconnected` 让调用方替换管线/tracks
/// 并继续外层 bus 监控循环.
///
/// # Arguments
///
/// * `cfg`: 流配置
/// * `taps`: 原始帧回调
/// * `handle`: 会话句柄 (状态/错误上报)
/// * `signal`: 停止信号 (中断退避等待)
/// * `attempt`: 当前重连次数 (每轮递增)
/// * `current`: 共享管线槽 (成功后原子更新)
///
/// returns: ReconnectOutcome — Reconnected(新管线, 新tracks) 或 Failed
///
/// # Examples
///
/// ```
///
/// ```
fn reconnect_session(
    cfg: &GstStreamConfig,
    taps: &RawTaps,
    handle: &GstStreamHandle,
    signal: &Arc<StopSignal>,
    attempt: &mut u32,
    current: &super::pipeline::SharedPipeline,
) -> ReconnectOutcome {
    loop {
        if cfg.reconnect.exhausted(*attempt) {
            error!(attempt, "reconnect attempts exhausted");
            handle.transition(StreamState::Failed);
            return ReconnectOutcome::Failed;
        }
        handle.transition(StreamState::Reconnecting);
        let delay = cfg.reconnect.delay_for(*attempt);
        *attempt += 1;
        info!(?delay, attempt, "reconnecting after backoff");
        if !wait_or_stop(signal, delay) {
            return ReconnectOutcome::Failed;
        }
        handle.transition(StreamState::Connecting);
        match try_rebuild_pipeline(cfg, taps, handle) {
            super::pipeline::TryRebuildResult::Ok(p, t) => {
                *current.lock() = Some(p.clone());
                return ReconnectOutcome::Reconnected(p, t);
            }
            super::pipeline::TryRebuildResult::Err(msg) => {
                handle.note_error(msg);
            }
        }
    }
}
