//! GStreamer pipeline implementation (feature `gst`).
//!
//! Topology: `rtspsrc` → dynamic pads (`stream_%u`) → per-track
//! `depay ! parse ! appsink`. Video AUs are split into single Annex-B
//! NALs and delivered through `on_video`; audio frames are depayloaded
//! and delivered through `on_audio`. With `AudioOutput::Alsa` the audio
//! branch tees off a decode-and-play chain to the ALSA device.
//!
//! The bus/session thread owns the lifecycle: ERROR/EOS/RTSPSrcTimeout
//! tear the pipeline down and rebuild it after exponential backoff
//! (auth failures and exhausted retries go straight to `Failed`); a
//! separate ticker logs stats every 10s while Playing. Both exit via
//! the shared [`StopSignal`] when `stop()` fires.
//!
//! Every `start()` builds an independent pipeline instance; sessions
//! share no mutable state (FR-007).

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use ipcam_core::{AudioCodec, EncodedPacket, VideoCodec, now_micros};
use parking_lot::Mutex;
use tracing::{error, info, warn};

use crate::packet::{
    is_keyframe_h264, is_keyframe_h265, pts_ns_to_rtp_ts90k, pts_ns_to_us, split_au_into_nals,
};
use crate::stats::{GstStreamHandle, StopSignal, StreamState, wait_or_stop};
use crate::{AudioOutput, AudioPacket, GstStreamConfig, GstStreamError};

/// Per-pipeline pad-added bookkeeping (recreated on every rebuild).
struct TrackState {
    video_linked: bool,
    audio_linked: bool,
}

type SharedPipeline = Arc<Mutex<Option<gst::Pipeline>>>;

pub(crate) fn start<V, A>(
    cfg: GstStreamConfig,
    on_video: V,
    on_audio: A,
) -> Result<GstStreamHandle, GstStreamError>
where
    V: FnMut(EncodedPacket) + Send + 'static,
    A: FnMut(AudioPacket) + Send + 'static,
{
    gst::init().map_err(|e| GstStreamError::Init(format!("gst init: {e}")))?;

    let handle = GstStreamHandle::new();
    let on_video = Arc::new(Mutex::new(on_video));
    let on_audio = Arc::new(Mutex::new(on_audio));

    let (pipeline, tracks) =
        build_pipeline(&cfg, on_video.clone(), on_audio.clone(), handle.clone())?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| GstStreamError::Init(format!("pipeline set Playing: {e}")))?;

    let signal = Arc::new(StopSignal::new());
    let current: SharedPipeline = Arc::new(Mutex::new(Some(pipeline.clone())));
    spawn_session_loop(
        pipeline,
        tracks,
        cfg,
        on_video,
        on_audio,
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

/// Build a fresh pipeline (rtspsrc + pad-added dispatch). Called by
/// `start()` and again on every reconnect — must stay reentrant.
fn build_pipeline<V, A>(
    cfg: &GstStreamConfig,
    on_video: Arc<Mutex<V>>,
    on_audio: Arc<Mutex<A>>,
    handle: GstStreamHandle,
) -> Result<(gst::Pipeline, Arc<Mutex<TrackState>>), GstStreamError>
where
    V: FnMut(EncodedPacket) + Send + 'static,
    A: FnMut(AudioPacket) + Send + 'static,
{
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
    pipeline
        .add(&src)
        .map_err(|e| GstStreamError::Init(format!("pipeline add rtspsrc: {e}")))?;

    let tracks = Arc::new(Mutex::new(TrackState {
        video_linked: false,
        audio_linked: false,
    }));
    install_pad_added(
        &src,
        &pipeline,
        tracks.clone(),
        cfg.audio_output.clone(),
        on_video,
        on_audio,
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
fn install_pad_added<V, A>(
    src: &gst::Element,
    pipeline: &gst::Pipeline,
    tracks: Arc<Mutex<TrackState>>,
    audio_output: AudioOutput,
    on_video: Arc<Mutex<V>>,
    on_audio: Arc<Mutex<A>>,
    handle: GstStreamHandle,
) where
    V: FnMut(EncodedPacket) + Send + 'static,
    A: FnMut(AudioPacket) + Send + 'static,
{
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
            "video" => link_video(&pipeline, pad, s, &tracks, &on_video, &handle),
            "audio" => link_audio(
                &pipeline,
                pad,
                s,
                &tracks,
                &on_audio,
                &handle,
                &audio_output,
            ),
            other => warn!(media = other, "unsupported pad media type, ignored"),
        }
    });
}

/// Link `rtph264depay ! h264parse ! appsink` (or the H.265 equivalents)
/// onto an rtspsrc video pad. Only the first video track is consumed;
/// additional ones are logged and ignored.
fn link_video<V>(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    s: &gst::StructureRef,
    tracks: &Arc<Mutex<TrackState>>,
    on_video: &Arc<Mutex<V>>,
    handle: &GstStreamHandle,
) where
    V: FnMut(EncodedPacket) + Send + 'static,
{
    let encoding = s.get::<&str>("encoding-name").unwrap_or("");
    let codec = VideoCodec::from_name(encoding);
    let (depay_name, parse_name, caps_name) = match codec {
        VideoCodec::H264 => ("rtph264depay", "h264parse", "video/x-h264"),
        VideoCodec::H265 => ("rtph265depay", "h265parse", "video/x-h265"),
        other => {
            warn!(encoding, codec = ?other, "unsupported video codec, track ignored");
            return;
        }
    };
    {
        let mut t = tracks.lock();
        if t.video_linked {
            warn!(
                encoding,
                "additional video track ignored (only the first is consumed)"
            );
            return;
        }
        t.video_linked = true;
    }

    let (depay, parse) = match (make(depay_name), make(parse_name)) {
        (Ok(d), Ok(p)) => (d, p),
        (d, p) => {
            error!(depay = ?d.err(), parse = ?p.err(), "video branch elements unavailable");
            return;
        }
    };
    // byte-stream (Annex-B) + alignment=au matches the downstream
    // Fmp4Muxer input contract carried over from play_loop.
    let caps = gst::Caps::builder(caps_name)
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build();
    let appsink = gst_app::AppSink::builder().caps(&caps).build();
    // 0.24: emit-signals isn't on AppSinkBuilder; set it post-build via property.
    appsink.set_property("emit-signals", true);

    if let Err(e) = pipeline.add_many([&depay, &parse, appsink.upcast_ref()]) {
        error!(%e, "failed to add video branch to pipeline");
        return;
    }
    if let Err(e) = gst::Element::link_many([&depay, &parse, appsink.upcast_ref()]) {
        error!(%e, "failed to link video branch");
        return;
    }
    for elem in [&depay, &parse, appsink.upcast_ref()] {
        if let Err(e) = elem.sync_state_with_parent() {
            warn!(element = %elem.name(), %e, "sync state with parent failed");
        }
    }
    let Some(depay_sink) = depay.static_pad("sink") else {
        error!(depay = depay_name, "depay element has no sink pad");
        return;
    };
    if let Err(e) = pad.link(&depay_sink) {
        error!(%e, "failed to link rtspsrc pad to video branch");
        return;
    }

    let cb = on_video.clone();
    let h = handle.clone();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let rtp_ts = pts_ns_to_rtp_ts90k(buffer.pts().map_or(0, |t| t.nseconds()));
                let nals = split_au_into_nals(map.as_slice());
                let last = nals.len().saturating_sub(1);
                let mut out = cb.lock();
                for (i, nal) in nals.into_iter().enumerate() {
                    let is_keyframe = match codec {
                        VideoCodec::H264 => is_keyframe_h264(&nal),
                        VideoCodec::H265 => is_keyframe_h265(&nal),
                        _ => false,
                    };
                    h.note_frame(nal.len() as u64, false);
                    out(EncodedPacket {
                        codec,
                        data: nal,
                        rtp_ts,
                        arrival_us: now_micros(),
                        is_keyframe,
                        marker: i == last,
                    });
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    info!(codec = ?codec, "video track linked");
}

/// Decode-chain element names between the tee and `audioconvert` for
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

/// Link the audio branch: `rtppcmadepay`/`rtppcmudepay`/`rtpmp4adepay`
/// → appsink delivering `AudioPacket` via `on_audio`. With
/// `AudioOutput::Alsa` a `tee` sits behind the depay element and a
/// decode → audioconvert → audioresample → alsasink chain plays the
/// stream on the target's speaker.
fn link_audio<A>(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    s: &gst::StructureRef,
    tracks: &Arc<Mutex<TrackState>>,
    on_audio: &Arc<Mutex<A>>,
    handle: &GstStreamHandle,
    audio_output: &AudioOutput,
) where
    A: FnMut(AudioPacket) + Send + 'static,
{
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
        warn!(encoding, ?payload, "unsupported audio codec, track ignored");
        return;
    };
    {
        let mut t = tracks.lock();
        if t.audio_linked {
            warn!(depay = depay_name, "additional audio track ignored");
            return;
        }
        t.audio_linked = true;
    }

    let depay = match make(depay_name) {
        Ok(d) => d,
        Err(e) => {
            error!(%e, "audio depay element unavailable");
            return;
        }
    };
    let appsink = gst_app::AppSink::builder().build();
    appsink.set_property("emit-signals", true);

    match audio_output {
        AudioOutput::Disabled => {
            if !assemble(pipeline, pad, &[&depay, appsink.upcast_ref()]) {
                return;
            }
            if let Err(e) = depay.link(appsink.upcast_ref::<gst::Element>()) {
                error!(%e, "failed to link audio branch");
                return;
            }
        }
        AudioOutput::Alsa { device } => {
            if !link_audio_with_playback(pipeline, pad, &depay, &appsink, codec, device) {
                return;
            }
        }
    }

    let cb = on_audio.clone();
    let h = handle.clone();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let (rate, channels) = match codec {
                    // G.711 over RTP is always 8 kHz mono.
                    AudioCodec::G711A | AudioCodec::G711U => (8000, 1),
                    _ => match sample.caps().and_then(|c| c.structure(0)) {
                        Some(st) => (
                            st.get::<i32>("rate").unwrap_or(0) as u32,
                            st.get::<i32>("channels").unwrap_or(0) as u32,
                        ),
                        None => {
                            warn!(codec = ?codec, "audio sample without caps; rate/channels set to 0");
                            (0, 0)
                        }
                    },
                };
                let pkt = AudioPacket {
                    codec,
                    data: Bytes::copy_from_slice(map.as_slice()),
                    pts_us: pts_ns_to_us(buffer.pts().map_or(0, |t| t.nseconds())),
                    rate,
                    channels,
                };
                h.note_frame(pkt.data.len() as u64, true);
                cb.lock()(pkt);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    info!(depay = depay_name, codec = ?codec, "audio track linked");
}

/// Add `elems` to the pipeline, sync their state with the parent and
/// link the rtspsrc pad to the first element's sink pad. Shared by the
/// plain and the tee'd audio branch.
fn assemble(pipeline: &gst::Pipeline, pad: &gst::Pad, elems: &[&gst::Element]) -> bool {
    if let Err(e) = pipeline.add_many(elems.iter().copied()) {
        error!(%e, "failed to add audio branch to pipeline");
        return false;
    }
    for elem in elems {
        if let Err(e) = elem.sync_state_with_parent() {
            warn!(element = %elem.name(), %e, "sync state with parent failed");
        }
    }
    let Some(first_sink) = elems[0].static_pad("sink") else {
        error!("first audio branch element has no sink pad");
        return false;
    };
    if let Err(e) = pad.link(&first_sink) {
        error!(%e, "failed to link rtspsrc pad to audio branch");
        return false;
    }
    true
}

/// `depay ! tee`, one tee output to the appsink queue, the other to
/// `decode ! audioconvert ! audioresample ! alsasink`. Tee src pads are
/// request pads (decodebin example pattern).
fn link_audio_with_playback(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    depay: &gst::Element,
    appsink: &gst_app::AppSink,
    codec: AudioCodec,
    device: &str,
) -> bool {
    let decode_names = decode_chain_names(codec);
    if decode_names.is_empty() {
        warn!(codec = ?codec, "no decode chain for codec, playing back nothing");
        return false;
    }

    let mut names = vec!["tee", "queue", "queue"];
    names.extend_from_slice(decode_names);
    names.extend(["audioconvert", "audioresample", "alsasink"]);
    let elems: Vec<gst::Element> =
        match names.iter().map(|n| make(n)).collect::<Result<Vec<_>, _>>() {
            Ok(v) => v,
            Err(e) => {
                error!(%e, "audio playback branch elements unavailable");
                return false;
            }
        };
    let tee = &elems[0];
    let queue_cb = &elems[1];
    let queue_play = &elems[2];
    let Some(alsasink) = elems.last() else {
        error!("playback branch missing alsasink");
        return false;
    };
    alsasink.set_property("device", device);

    // Static links: depay→tee, queue_cb→appsink, and the playback chain
    // queue_play→decode…→alsasink.
    if let Err(e) = depay.link(tee) {
        error!(%e, "failed to link depay to tee");
        return false;
    }
    if let Err(e) = queue_cb.link(appsink.upcast_ref::<gst::Element>()) {
        error!(%e, "failed to link callback queue to appsink");
        return false;
    }
    let play_chain: Vec<&gst::Element> = elems[2..].iter().collect();
    if let Err(e) = gst::Element::link_many(play_chain) {
        error!(%e, "failed to link audio playback chain");
        return false;
    }

    // Request tee src pads and connect both outputs.
    for (branch, queue) in [("callback", queue_cb), ("playback", queue_play)] {
        let Some(tee_src) = tee.request_pad_simple("src_%u") else {
            error!("tee request pad src_%u failed");
            return false;
        };
        let Some(queue_sink) = queue.static_pad("sink") else {
            error!(branch, "queue has no sink pad");
            return false;
        };
        if let Err(e) = tee_src.link(&queue_sink) {
            error!(branch, %e, "failed to link tee src pad");
            return false;
        }
    }

    let mut all: Vec<&gst::Element> = vec![depay];
    all.extend(elems.iter());
    all.push(appsink.upcast_ref());
    assemble(pipeline, pad, &all)
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
// reason: one argument per session-owned resource; bundling them into
// a struct would just reshuffle the same list.
#[allow(clippy::too_many_arguments)]
fn spawn_session_loop<V, A>(
    pipeline: gst::Pipeline,
    tracks: Arc<Mutex<TrackState>>,
    cfg: GstStreamConfig,
    on_video: Arc<Mutex<V>>,
    on_audio: Arc<Mutex<A>>,
    handle: GstStreamHandle,
    current: SharedPipeline,
    signal: Arc<StopSignal>,
) where
    V: FnMut(EncodedPacket) + Send + 'static,
    A: FnMut(AudioPacket) + Send + 'static,
{
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
                match build_pipeline(&cfg, on_video.clone(), on_audio.clone(), handle.clone()) {
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
