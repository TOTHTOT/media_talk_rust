//! GStreamer pipeline implementation.
//!
//! Topology: `rtspsrc` → dynamic pads (`stream_%u`) → per-track
//! `depay ! parse ! webrtcsink(video_%u)`. The encoded stream is
//! republished verbatim — no decode, no re-encode — so the browser does
//! the decoding and the latency budget is the WebRTC jitter buffer
//! (tens of ms) instead of an MSE buffer (seconds). With
//! `AudioOutput::Alsa` the audio branch decodes and plays locally
//! (webrtcsink's `audio_%u` pad only accepts raw/opus, so camera G.711
//! audio is not forwarded to the browser yet).
//!
//! webrtcsink's sink pads are **request pads**: each track requests
//! `video_%u`/`audio_%u` when the rtspsrc pad appears. The element
//! registers as a producer on the process-wide signalling server (see
//! [`crate::ensure_signalling_server`]) under `cfg.stream_name`.
//!
//! The bus/session thread owns the lifecycle: ERROR/EOS/RTSPSrcTimeout
//! tear the pipeline down and rebuild it after exponential backoff
//! (auth failures and exhausted retries go straight to `Failed`); a
//! separate ticker logs stats every 10s while Playing. Both exit via
//! the shared [`StopSignal`] when `stop()` fires. Frame/byte counters
//! come from buffer probes on the parse src pads (there is no appsink
//! anymore).
//!
//! Every `start()` builds an independent pipeline instance; sessions
//! share no mutable state (FR-007).

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use ipcam_core::{AudioCodec, VideoCodec};
use parking_lot::Mutex;
use tracing::{error, info, warn};

use crate::stats::{GstStreamHandle, StopSignal, StreamState, wait_or_stop};
use crate::{AudioOutput, GstStreamConfig, GstStreamError};

/// Per-pipeline pad-added bookkeeping (recreated on every rebuild).
struct TrackState {
    video_linked: bool,
    audio_linked: bool,
}

type SharedPipeline = Arc<Mutex<Option<gst::Pipeline>>>;

pub(crate) fn start(cfg: GstStreamConfig) -> Result<GstStreamHandle, GstStreamError> {
    gst::init().map_err(|e| GstStreamError::Init(format!("gst init: {e}")))?;

    let handle = GstStreamHandle::new();
    let (pipeline, tracks) = build_pipeline(&cfg, handle.clone())?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| GstStreamError::Init(format!("pipeline set Playing: {e}")))?;

    let signal = Arc::new(StopSignal::new());
    let current: SharedPipeline = Arc::new(Mutex::new(Some(pipeline.clone())));
    spawn_session_loop(
        pipeline,
        tracks,
        cfg,
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

/// Create an element, mapping a missing factory/plugin to `Init` with
/// the element name in the message (a missing `rtspsrc` usually means
/// `gstreamer1.0-plugins-good` is not installed on the target).
fn make(name: &str) -> Result<gst::Element, GstStreamError> {
    gst::ElementFactory::make(name)
        .build()
        .map_err(|e| GstStreamError::Init(format!("missing element `{name}`: {e}")))
}

/// Build a fresh pipeline (rtspsrc + webrtcsink + pad-added dispatch).
/// Called by `start()` and again on every reconnect — must stay
/// reentrant.
fn build_pipeline(
    cfg: &GstStreamConfig,
    handle: GstStreamHandle,
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

    let ws = make("webrtcsink")?;
    // Producer identity on the signalling channel; consumers match on
    // `meta.name`. Keep it unique per session.
    ws.set_property_from_str("meta", &format!("meta,name={}", cfg.stream_name));
    // webrtcsink 连到进程内共享的信令服务器（ensure_signalling_server
    // 起在 8443）——注意不是 signalling-server-host/port，那两个属性
    // 是给“自己跑服务器”模式用的，连别人的服务器要走 signaller 的 uri
    let signaller = ws.property::<gst::glib::Object>("signaller");
    signaller.set_property(
        "uri",
        format!("ws://{}:{}", cfg.signalling_host, cfg.signalling_port),
    );

    pipeline
        .add_many([&src, &ws])
        .map_err(|e| GstStreamError::Init(format!("pipeline add rtspsrc/webrtcsink: {e}")))?;

    let tracks = Arc::new(Mutex::new(TrackState {
        video_linked: false,
        audio_linked: false,
    }));
    install_pad_added(
        &src,
        &pipeline,
        tracks.clone(),
        cfg.audio_output.clone(),
        ws,
        handle,
    );
    Ok((pipeline, tracks))
}

/// Dispatch rtspsrc's dynamic `stream_%u` pads by caps
/// (`media` + `encoding-name`, with a static-payload fallback for
/// cameras that omit rtpmap for PCMA/PCMU).
///
/// NOTE: `pad-added` must be connected on the **rtspsrc element** — the
/// dynamic pads belong to it. Connecting on the pipeline (a Bin) never
/// fires for rtspsrc's stream pads (the bin only reports its own ghost
/// pads), which silently leaves every track unlinked.
fn install_pad_added(
    src: &gst::Element,
    pipeline: &gst::Pipeline,
    tracks: Arc<Mutex<TrackState>>,
    audio_output: AudioOutput,
    ws: gst::Element,
    handle: GstStreamHandle,
) {
    let weak = pipeline.downgrade();

    src.connect_pad_added(move |_src, pad| {
        let Some(pipeline) = weak.upgrade() else {
            return;
        };
        let caps = pad
            .current_caps()
            .unwrap_or_else(|| pad.query_caps(None::<&gst::Caps>));
        let Some(s) = caps.structure(0) else {
            warn!(pad = %pad.name(), "pad without caps structure, ignored");
            return;
        };
        match s.get::<&str>("media").unwrap_or("") {
            "video" => {
                if let Err(e) = link_video(&pipeline, pad, s, &tracks, &ws, &handle) {
                    error!(%e, "link_video failed");
                }
            }
            "audio" => {
                if let Err(e) = link_audio(&pipeline, pad, s, &tracks, &handle, &audio_output) {
                    error!(%e, "link_audio failed");
                }
            }
            other => warn!(media = other, "unsupported pad media type, ignored"),
        }
    });
}

/// Attach a buffer probe on `pad` that feeds the session's frame/byte
/// counters (replaces the appsink callbacks of the fMP4 era).
fn install_stats_probe(pad: &gst::Pad, handle: GstStreamHandle, is_audio: bool) {
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        if let Some(gst::PadProbeData::Buffer(ref buffer)) = info.data {
            handle.note_frame(buffer.size() as u64, is_audio);
        }
        gst::PadProbeReturn::Ok
    });
}

/// Link `rtph264depay ! h264parse ! webrtcsink.video_%u` (or the H.265
/// equivalents) onto an rtspsrc video pad. Only the first video track
/// is consumed; additional ones are logged and ignored.
fn link_video(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    s: &gst::StructureRef,
    tracks: &Arc<Mutex<TrackState>>,
    ws: &gst::Element,
    handle: &GstStreamHandle,
) -> Result<(), GstStreamError> {
    let encoding = s.get::<&str>("encoding-name").unwrap_or("");
    let codec = VideoCodec::from_name(encoding);
    let (depay_name, parse_name) = match codec {
        VideoCodec::H264 => ("rtph264depay", "h264parse"),
        VideoCodec::H265 => ("rtph265depay", "h265parse"),
        other => {
            return Err(GstStreamError::Link(format!(
                "unsupported video codec: {encoding} ({other:?}), track ignored"
            )));
        }
    };
    {
        let mut t = tracks.lock();
        if t.video_linked {
            return Err(GstStreamError::Link(format!(
                "additional video track ignored (only the first is consumed): {encoding}"
            )));
        }
        t.video_linked = true;
    }

    let depay = make(depay_name)?; // RTP 包 → 编码码流（Annex-B NAL）
    let parse = make(parse_name)?; // 组帧 + 提供 codec_data 给下游
    pipeline
        .add_many([&depay, &parse])
        .map_err(|e| GstStreamError::Link(format!("failed to add video branch: {e}")))?;
    gst::Element::link_many([&depay, &parse])
        .map_err(|e| GstStreamError::Link(format!("failed to link video branch: {e}")))?;

    for elem in [&depay, &parse] {
        if let Err(e) = elem.sync_state_with_parent() {
            warn!(element = %elem.name(), %e, "sync state with parent failed");
        }
    }
    let depay_sink = depay
        .static_pad("sink")
        .ok_or(GstStreamError::Link(format!(
            "depay element `{depay_name}` has no sink pad"
        )))?;
    pad.link(&depay_sink).map_err(|e| {
        GstStreamError::Link(format!("failed to link rtspsrc pad to video branch: {e}"))
    })?;

    // webrtcsink 的 sink pad 是 request pad，用到时才申请
    let ws_pad = ws
        .request_pad_simple("video_%u")
        .ok_or(GstStreamError::Link(
            "webrtcsink request pad video_%u failed".into(),
        ))?;
    let parse_src = parse.static_pad("src").ok_or(GstStreamError::Link(format!(
        "parse element `{parse_name}` has no src pad"
    )))?;
    parse_src
        .link(&ws_pad)
        .map_err(|e| GstStreamError::Link(format!("failed to link parse to webrtcsink: {e}")))?;
    install_stats_probe(&parse_src, handle.clone(), false);

    info!(codec = ?codec, "video track linked");
    Ok(())
}

/// Decode-chain element names between the depay and `audioconvert` for
/// `AudioOutput::Alsa` playback (`avdec_aac` comes from gst-libav —
/// needs `gstreamer1.0-libav` on the target).
fn decode_chain_names(codec: AudioCodec) -> &'static [&'static str] {
    match codec {
        AudioCodec::G711A => &["alawdec"],
        AudioCodec::G711U => &["mulawdec"],
        AudioCodec::Aac => &["aacparse", "avdec_aac"],
        _ => &[],
    }
}

/// Link the audio branch. webrtcsink's `audio_%u` pad only accepts
/// raw/opus, so camera audio (G.711/AAC) is NOT forwarded to the
/// browser; the branch exists solely for `AudioOutput::Alsa` local
/// playback (`depay ! decode ! audioconvert ! audioresample ! alsasink`).
/// With `AudioOutput::Disabled` the pad is left unlinked.
fn link_audio(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    s: &gst::StructureRef,
    tracks: &Arc<Mutex<TrackState>>,
    handle: &GstStreamHandle,
    audio_output: &AudioOutput,
) -> Result<(), GstStreamError> {
    let encoding = s.get::<&str>("encoding-name").unwrap_or("");
    let payload = s.get::<i32>("payload").ok();
    let codec = match encoding.to_ascii_uppercase().as_str() {
        "PCMA" => Some(("rtppcmadepay", AudioCodec::G711A)),
        "PCMU" => Some(("rtppcmudepay", AudioCodec::G711U)),
        "MP4A-LATM" => Some(("rtpmp4adepay", AudioCodec::Aac)),
        // Static RTP payload types: some cameras omit rtpmap entirely.
        _ => match payload {
            Some(8) => Some(("rtppcmadepay", AudioCodec::G711A)),
            Some(0) => Some(("rtppcmudepay", AudioCodec::G711U)),
            _ => None,
        },
    };
    let Some((depay_name, codec)) = codec else {
        return Err(GstStreamError::Link(format!(
            "unsupported audio codec: encoding={encoding}, payload={payload:?}, track ignored"
        )));
    };

    let AudioOutput::Alsa { device } = audio_output else {
        info!(depay = depay_name, codec = ?codec, "audio track ignored (no local playback requested)");
        return Ok(());
    };
    {
        let mut t = tracks.lock();
        if t.audio_linked {
            return Err(GstStreamError::Link(format!(
                "additional audio track ignored: {depay_name}"
            )));
        }
        t.audio_linked = true;
    }

    let depay = make(depay_name)?;
    let decode_names = decode_chain_names(codec);
    if decode_names.is_empty() {
        return Err(GstStreamError::Link(format!(
            "no decode chain for codec: {codec:?}, playing back nothing"
        )));
    }
    let mut names = vec![depay_name];
    names.extend_from_slice(decode_names);
    names.extend(["audioconvert", "audioresample", "alsasink"]);
    let elems: Vec<gst::Element> = names
        .iter()
        .map(|n| make(n))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            GstStreamError::Link(format!("audio playback branch elements unavailable: {e}"))
        })?;
    let Some(alsasink) = elems.last() else {
        return Err(GstStreamError::Link(
            "playback branch missing alsasink".into(),
        ));
    };
    alsasink.set_property("device", device);

    let refs: Vec<&gst::Element> = elems.iter().collect();
    gst::Element::link_many(refs.clone())
        .map_err(|e| GstStreamError::Link(format!("failed to link audio playback chain: {e}")))?;

    pipeline
        .add_many(refs.clone())
        .map_err(|e| GstStreamError::Link(format!("failed to add audio branch: {e}")))?;
    for elem in &elems {
        if let Err(e) = elem.sync_state_with_parent() {
            warn!(element = %elem.name(), %e, "sync state with parent failed");
        }
    }
    let depay_sink = depay.static_pad("sink").ok_or(GstStreamError::Link(
        "audio depay element has no sink pad".into(),
    ))?;
    pad.link(&depay_sink).map_err(|e| {
        GstStreamError::Link(format!("failed to link rtspsrc pad to audio branch: {e}"))
    })?;

    // 统计：数 depay 输出的编码帧（解码后样本计数意义不大）
    if let Some(depay_src) = depay.static_pad("src") {
        install_stats_probe(&depay_src, handle.clone(), true);
    }
    info!(depay = depay_name, codec = ?codec, "audio playback branch linked");
    Ok(())
}

/// Outcome of one bus watch round on the session thread.
enum BusOutcome {
    /// `stop()` fired (or the bus vanished): leave without reconnecting.
    Stopped,
    /// Terminal error/EOS/timeout text; the session loop decides
    /// between reconnect and `Failed`.
    StreamError(String),
}

/// Session thread: watches the bus, and on stream failure rebuilds the
/// whole pipeline after exponential backoff (R5). Auth failures (401
/// wording) and exhausted `max_attempts` end the session as `Failed`;
/// `stop()` interrupts backoff immediately.
fn spawn_session_loop(
    pipeline: gst::Pipeline,
    tracks: Arc<Mutex<TrackState>>,
    cfg: GstStreamConfig,
    handle: GstStreamHandle,
    current: SharedPipeline,
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

            // Reconnect attempts: Reconnecting → backoff → Connecting →
            // rebuild → Playing; exhausted retries or stop end the loop.
            loop {
                if cfg.reconnect.exhausted(attempt) {
                    error!(attempt, "reconnect attempts exhausted");
                    handle.transition(StreamState::Failed);
                    break 'outer;
                }
                handle.transition(StreamState::Reconnecting);
                let delay = cfg.reconnect.delay_for(attempt);
                attempt += 1;
                info!(?delay, attempt, "reconnecting after backoff");
                if !wait_or_stop(&signal, delay) {
                    break 'outer;
                }
                handle.transition(StreamState::Connecting);
                match build_pipeline(&cfg, handle.clone()) {
                    Ok((p, t)) => match p.set_state(gst::State::Playing) {
                        Ok(_) => {
                            *current.lock() = Some(p.clone());
                            pipeline = p;
                            tracks = t;
                            continue 'outer;
                        }
                        Err(e) => {
                            handle.note_error(format!("rebuilt pipeline set Playing: {e}"));
                        }
                    },
                    Err(e) => {
                        handle.note_error(e.to_string());
                    }
                }
            }
        }
        let _ = pipeline.set_state(gst::State::Null);
        *current.lock() = None;
    });
}

/// Block on the bus until a terminal message arrives or `stop()` fires.
/// Drives `Connecting → Playing` and the "no audio track" note.
fn watch_bus(
    pipeline: &gst::Pipeline,
    tracks: &Arc<Mutex<TrackState>>,
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
            gst::MessageView::Eos(..) => {
                info!("end of stream");
                return BusOutcome::StreamError("eos: end of stream".into());
            }
            gst::MessageView::Element(el) => {
                let is_rtsp_timeout = el
                    .structure()
                    .is_some_and(|s| s.name() == "GstRTSPSrcTimeout");
                if is_rtsp_timeout {
                    return BusOutcome::StreamError("rtspsrc timeout".into());
                }
            }
            _ => {}
        }
    }
}

/// T025: while Playing, log one structured stats line every 10s.
/// Exits with the session via the shared stop signal.
fn spawn_stats_ticker(handle: GstStreamHandle, signal: Arc<StopSignal>) {
    thread::spawn(move || {
        while wait_or_stop(&signal, Duration::from_secs(10)) {
            if handle.state() != StreamState::Playing {
                continue;
            }
            let s = handle.stats();
            info!(
                frames_video = s.frames_video,
                frames_audio = s.frames_audio,
                bytes = s.bytes,
                reconnects = s.reconnects,
                last_error = ?s.last_error,
                "stream stats"
            );
        }
    });
}

fn is_auth_error(text: &str) -> bool {
    text.contains("401") || text.contains("Unauthorized") || text.contains("Not Authorized")
}
