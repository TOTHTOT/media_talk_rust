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
use gstreamer_app::AppSinkCallbacks;
use ipcam_core::{AudioCodec, EncodedPacket, VideoCodec, now_micros};
use parking_lot::Mutex;
use tracing::{error, info, warn};

use crate::packet::{pts_ns_to_rtp_ts90k, pts_ns_to_us};
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
        info!(s = %s, "current pad capacity");
        match s.get::<&str>("media").unwrap_or("") {
            "video" => {
                if let Err(e) = link_video(&pipeline, pad, s, &tracks, &on_video, &handle) {
                    error!(%e, "link_video failed");
                }
            }
            "audio" => {
                if let Err(e) = link_audio(
                    &pipeline,
                    pad,
                    s,
                    &tracks,
                    &on_audio,
                    &handle,
                    &audio_output,
                ) {
                    error!(%e, "link_audio failed");
                }
            }
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
) -> Result<(), GstStreamError>
where
    V: FnMut(EncodedPacket) + Send + 'static,
{
    let encoding = s.get::<&str>("encoding-name").unwrap_or("");
    let codec = VideoCodec::from_name(encoding);
    let (depay_name, parse_name, caps_name) = match codec {
        VideoCodec::H264 => ("rtph264depay", "h264parse", "video/x-h264"),
        VideoCodec::H265 => ("rtph265depay", "h265parse", "video/x-h265"),
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

    let depay = make(depay_name)?; // 将输入的 rtp 包处理成 Annex-B NAL 然后丢给 parse
    let parse = make(parse_name)?; // 将 depay 的结果组合成完整的 au, 最后丢给 appsink的回调
    // byte-stream (Annex-B) + alignment=au matches the downstream
    // Fmp4Muxer input contract carried over from play_loop.
    let caps = gst::Caps::builder(caps_name)
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build();
    let appsink = gst_app::AppSink::builder().caps(&caps).build();
    // 0.24: emit-signals isn't on AppSinkBuilder; set it post-build via property.
    appsink.set_property("emit-signals", true);

    pipeline
        .add_many([&depay, &parse, appsink.upcast_ref()])
        .map_err(|e| {
            GstStreamError::Link(format!("failed to add video branch to pipeline: {e}"))
        })?;
    gst::Element::link_many([&depay, &parse, appsink.upcast_ref()])
        .map_err(|e| GstStreamError::Link(format!("failed to link video branch: {e}")))?;

    // 对其 element 初始状态, 其实也可以不用, 目前还没 play 都是 null 状态
    for elem in [&depay, &parse, appsink.upcast_ref()] {
        if let Err(e) = elem.sync_state_with_parent() {
            warn!(element = %elem.name(), %e, "sync state with parent failed");
        }
    }
    // 提取 depay 的sink_pad然后与rtsp的src_pad连接
    let depay_sink = depay
        .static_pad("sink")
        .ok_or(GstStreamError::Link(format!(
            "depay element `{depay_name}` has no sink pad"
        )))?;
    pad.link(&depay_sink).map_err(|e| {
        GstStreamError::Link(format!("failed to link rtspsrc pad to video branch: {e}"))
    })?;

    appsink.set_callbacks(link_video_callback(codec, on_video.clone(), handle.clone()));
    info!(codec = ?codec, "video track linked");
    Ok(())
}


///
///
/// Builds [`AppSinkCallbacks`] for the video branch.
///
/// The callback is invoked by GStreamer's appsink each time a complete
/// access unit (one frame, `alignment=au`) is available. It extracts the
/// raw Annex-B NAL data and PTS from the buffer, wraps them in an
/// [`EncodedPacket`], and forwards it through `cb`. [`GstStreamHandle`]
/// is used only for byte-count bookkeeping.
///
/// # Arguments
///
/// * `codec`: video codec (H.264 or H.265), copied into every packet.
/// * `cb`: wrapped callback invoked once per complete access unit.
/// * `h`: stream handle used for byte-count accounting.
///
/// returns: AppSinkCallbacks
fn link_video_callback<V>(
    codec: VideoCodec,
    cb: Arc<Mutex<V>>,
    h: GstStreamHandle,
) -> AppSinkCallbacks
where
    V: FnMut(EncodedPacket) + Send + 'static,
{
    AppSinkCallbacks::builder()
        .new_sample(move |sink| {
            let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
            let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let rtp_ts = pts_ns_to_rtp_ts90k(buffer.pts().map_or(0, |t| t.nseconds()));
            // alignment=au on the appsink caps guarantees one buffer ==
            // one complete access unit (Annex-B). Deliver it whole —
            // splitting into per-NAL packets would only force the
            // consumer to reassemble what is already assembled here.
            h.note_frame(map.len() as u64, false);
            // GStreamer marks non-keyframe buffers DELTA_UNIT.
            let is_keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
            cb.lock()(EncodedPacket {
                codec,
                data: Bytes::copy_from_slice(map.as_slice()),
                rtp_ts,
                arrival_us: now_micros(),
                is_keyframe,
                // Every packet carries a complete AU, so it is always
                // the "last packet of the access unit".
                marker: true,
            });
            Ok(gst::FlowSuccess::Ok)
        })
        .build()
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
) -> Result<(), GstStreamError>
where
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
        return Err(GstStreamError::Link(format!(
            "unsupported audio codec: encoding={encoding}, payload={payload:?}, track ignored"
        )));
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
    let appsink = gst_app::AppSink::builder().build();
    appsink.set_property("emit-signals", true);

    match audio_output {
        AudioOutput::Disabled => {
            assemble(pipeline, pad, &[&depay, appsink.upcast_ref()])?;
            depay
                .link(appsink.upcast_ref::<gst::Element>())
                .map_err(|e| GstStreamError::Link(format!("failed to link audio branch: {e}")))?;
        }
        AudioOutput::Alsa { device } => {
            link_audio_with_playback(pipeline, pad, &depay, &appsink, codec, device)?;
        }
    }

    let cb = on_audio.clone();
    let h = handle.clone();
    appsink.set_callbacks(
        AppSinkCallbacks::builder()
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
    Ok(())
}

/// Add `elems` to the pipeline, sync their state with the parent and
/// link the rtspsrc pad to the first element's sink pad. Shared by the
/// plain and the tee'd audio branch.
fn assemble(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    elems: &[&gst::Element],
) -> Result<(), GstStreamError> {
    if let Err(e) = pipeline.add_many(elems.iter().copied()) {
        return Err(GstStreamError::Link(format!(
            "failed to add audio branch to pipeline: {e}"
        )));
    }
    for elem in elems {
        if let Err(e) = elem.sync_state_with_parent() {
            warn!(element = %elem.name(), %e, "sync state with parent failed");
        }
    }
    let Some(first_sink) = elems[0].static_pad("sink") else {
        return Err(GstStreamError::Link(
            "first audio branch element has no sink pad".into(),
        ));
    };
    if let Err(e) = pad.link(&first_sink) {
        return Err(GstStreamError::Link(format!(
            "failed to link rtspsrc pad to audio branch: {e}"
        )));
    }
    Ok(())
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
) -> Result<(), GstStreamError> {
    let decode_names = decode_chain_names(codec);
    if decode_names.is_empty() {
        return Err(GstStreamError::Link(format!(
            "no decode chain for codec: {codec:?}, playing back nothing"
        )));
    }

    let mut names = vec!["tee", "queue", "queue"];
    names.extend_from_slice(decode_names);
    names.extend(["audioconvert", "audioresample", "alsasink"]);
    let elems: Vec<gst::Element> =
        match names.iter().map(|n| make(n)).collect::<Result<Vec<_>, _>>() {
            Ok(v) => v,
            Err(e) => {
                return Err(GstStreamError::Link(format!(
                    "audio playback branch elements unavailable: {e}"
                )));
            }
        };
    let tee = &elems[0];
    let queue_cb = &elems[1];
    let queue_play = &elems[2];
    let Some(alsasink) = elems.last() else {
        return Err(GstStreamError::Link(
            "playback branch missing alsasink".into(),
        ));
    };
    alsasink.set_property("device", device);

    // Static links: depay→tee, queue_cb→appsink, and the playback chain
    // queue_play→decode…→alsasink.
    if let Err(e) = depay.link(tee) {
        return Err(GstStreamError::Link(format!(
            "failed to link depay to tee: {e}"
        )));
    }
    if let Err(e) = queue_cb.link(appsink.upcast_ref::<gst::Element>()) {
        return Err(GstStreamError::Link(format!(
            "failed to link callback queue to appsink: {e}"
        )));
    }
    let play_chain: Vec<&gst::Element> = elems[2..].iter().collect();
    if let Err(e) = gst::Element::link_many(play_chain) {
        return Err(GstStreamError::Link(format!(
            "failed to link audio playback chain: {e}"
        )));
    }

    // Request tee src pads and connect both outputs.
    for (branch, queue) in [("callback", queue_cb), ("playback", queue_play)] {
        let Some(tee_src) = tee.request_pad_simple("src_%u") else {
            return Err(GstStreamError::Link(format!(
                "tee request pad src_%u failed for {branch} branch"
            )));
        };
        let Some(queue_sink) = queue.static_pad("sink") else {
            return Err(GstStreamError::Link(format!(
                "queue has no sink pad for {branch} branch"
            )));
        };
        if let Err(e) = tee_src.link(&queue_sink) {
            return Err(GstStreamError::Link(format!(
                "failed to link tee src pad ({branch}): {e}"
            )));
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
