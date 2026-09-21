//! GStreamer pipeline implementation.
//!
//! Topology: `rtspsrc` → dynamic pads (`stream_%u`) → per-track
//! `depay ! parse ! webrtcsink(video_%u)`. The encoded video is
//! republished verbatim — no decode, no re-encode — so the browser does
//! the decoding and the latency budget is the WebRTC jitter buffer
//! (tens of ms) instead of an MSE buffer (seconds). Audio transcodes
//! (G.711/AAC → Opus) through a tee because webrtcsink's `audio_%u` pad
//! only accepts raw/opus; the same tee optionally feeds a local
//! decode-and-play branch to ALSA.
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
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use ipcam_core::{AudioCodec, VideoCodec};
use parking_lot::Mutex;
use tracing::{debug, error, info, warn};

pub(crate) mod config;
pub(crate) mod stats;
pub(crate) mod tap;

use crate::GstStreamError;
use crate::gstutil::{add_and_sync, leaky_queue, link_chain, make, static_pad};
use config::{AudioOutput, GstStreamConfig};
use stats::{GstStreamHandle, StopSignal, StreamState, wait_or_stop};
use tap::{AudioChunkSink, RawAudioChunk, RawTaps, RawVideoFrame, VideoFrameSink};

/// Per-pipeline pad-added bookkeeping (recreated on every rebuild).
struct TrackState {
    video_linked: bool,
    audio_linked: bool,
}

type SharedPipeline = Arc<Mutex<Option<gst::Pipeline>>>;

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
    let current: SharedPipeline = Arc::new(Mutex::new(Some(pipeline.clone())));
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

/// Build a fresh pipeline (rtspsrc + webrtcsink + pad-added dispatch).
/// Called by `start()` and again on every reconnect — must stay
/// reentrant.
fn build_pipeline(
    cfg: &GstStreamConfig,
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
        taps,
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
    taps: RawTaps,
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
                if let Err(e) = link_video(&pipeline, pad, s, &tracks, &ws, &handle, &taps.video) {
                    error!(%e, "link_video failed");
                }
            }
            "audio" => {
                if let Err(e) = link_audio(
                    &pipeline,
                    pad,
                    s,
                    &tracks,
                    &ws,
                    &handle,
                    &audio_output,
                    &taps.audio,
                ) {
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

/// Resolve the RTP depay/parse element names for a video caps
/// `encoding-name`, plus the libav decoder used by the raw tap branch;
/// webrtcsink can only consume H.264/H.265.
fn video_chain_names(
    encoding: &str,
) -> Result<(VideoCodec, &'static str, &'static str, &'static str), GstStreamError> {
    match VideoCodec::from_name(encoding) {
        VideoCodec::H264 => Ok((VideoCodec::H264, "rtph264depay", "h264parse", "avdec_h264")),
        VideoCodec::H265 => Ok((VideoCodec::H265, "rtph265depay", "h265parse", "avdec_h265")),
        other => Err(GstStreamError::Link(format!(
            "unsupported video codec: {encoding} ({other:?}), track ignored"
        ))),
    }
}

/// Claim the single video/audio slot in `tracks`; additional tracks of
/// the same kind are rejected with an explanatory Link error.
fn claim_track(
    tracks: &Arc<Mutex<TrackState>>,
    is_audio: bool,
    desc: &str,
) -> Result<(), GstStreamError> {
    let mut t = tracks.lock();
    let slot = if is_audio {
        &mut t.audio_linked
    } else {
        &mut t.video_linked
    };
    if *slot {
        let kind = if is_audio { "audio" } else { "video" };
        return Err(GstStreamError::Link(format!(
            "additional {kind} track ignored (only the first is consumed): {desc}"
        )));
    }
    *slot = true;
    Ok(())
}

/// Request a sink pad from webrtcsink (`video_%u` / `audio_%u`) —
/// 用到时才申请，这是 webrtcsink 接收音视频的唯一入口.
fn request_ws_pad(ws: &gst::Element, template: &str) -> Result<gst::Pad, GstStreamError> {
    ws.request_pad_simple(template)
        .ok_or(GstStreamError::Link(format!(
            "webrtcsink request pad {template} failed"
        )))
}

/// Link the rtspsrc stream pad to the head element of a branch.
fn link_rtsp_pad(pad: &gst::Pad, head: &gst::Element) -> Result<(), GstStreamError> {
    let sink = static_pad(head, "sink")?;
    pad.link(&sink)
        .map_err(|e| GstStreamError::Link(format!("failed to link rtspsrc pad to branch: {e}")))?;
    Ok(())
}

/// Link `rtph264depay ! h264parse ! webrtcsink.video_%u` (or the H.265
/// equivalents) onto an rtspsrc video pad. Only the first video track
/// is consumed; additional ones are logged and ignored. With a video
/// `tap`, a tee is inserted after parse: the WebRTC branch is untouched,
/// the second branch decodes to RGBA into an appsink callback.
fn link_video(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    s: &gst::StructureRef,
    tracks: &Arc<Mutex<TrackState>>,
    ws: &gst::Element,
    handle: &GstStreamHandle,
    tap: &Option<VideoFrameSink>,
) -> Result<(), GstStreamError> {
    let encoding = s.get::<&str>("encoding-name").unwrap_or("");
    debug!(encoding = encoding, "linking video");
    let (codec, depay_name, parse_name, decoder_name) = video_chain_names(encoding)?;
    claim_track(tracks, false, encoding)?;

    let depay = make(depay_name)?; // RTP 包 → 编码码流（Annex-B NAL）
    let parse = make(parse_name)?; // 组帧 + 提供 codec_data 给下游
    // 有需要视频输出到别的分支就要插入tee分流
    let tap_branch = match tap {
        Some(sink) => Some(build_video_tap_branch(decoder_name, sink.clone())?),
        None => None,
    };

    // 必须先入管道对齐状态、再链接：孤儿态（未入 bin、pad 未激活）
    // 建好的链接会在 add_many/sync_state_with_parent 时被丢掉，
    // 首帧 push 即 not-linked（ingest example 曾因此 100% 断流）。
    let mut elems = vec![depay.clone(), parse.clone()];
    if let Some(b) = &tap_branch {
        elems.push(b.tee.clone());
        elems.extend(b.gui_chain.iter().cloned());
    }
    let refs: Vec<&gst::Element> = elems.iter().collect();
    add_and_sync(pipeline, &refs)?;
    link_chain(&[&depay, &parse], "video branch")?;
    link_rtsp_pad(pad, &depay)?;
    let ws_pad = request_ws_pad(ws, "video_%u")?;
    let parse_src = static_pad(&parse, "src")?;
    match &tap_branch {
        // parse → tee，一路直推 webrtcsink（不加 queue，保持原有低延迟路径），
        // 另一路 queue(leaky) → decode → videoconvert → appsink
        Some(b) => {
            parse_src
                .link(&static_pad(&b.tee, "sink")?)
                .map_err(|e| GstStreamError::Link(format!("failed to link parse to tee: {e}")))?;
            link_tee_to_pad(&b.tee, &ws_pad)?;
            link_tee_branch(&b.tee, &b.gui_chain[0])?;
            let gui_refs: Vec<&gst::Element> = b.gui_chain.iter().collect();
            link_chain(&gui_refs, "video tap branch")?;
        }
        None => {
            parse_src.link(&ws_pad).map_err(|e| {
                GstStreamError::Link(format!("failed to link parse to webrtcsink: {e}"))
            })?;
        }
    }
    install_stats_probe(&parse_src, handle.clone(), false);

    info!(codec = ?codec, tap = tap_branch.is_some(), "video track linked");
    Ok(())
}

/// Elements of the optional video tap branch: a tee right after parse,
/// and `queue(leaky) → decoder → videoconvert → appsink(RGBA)` for the
/// raw-frame callback. The WebRTC branch links straight off the tee.
struct VideoTapBranch {
    tee: gst::Element,
    /// queue → decoder → videoconvert → appsink (linked in this order).
    gui_chain: Vec<gst::Element>,
}

fn build_video_tap_branch(
    decoder_name: &str,
    sink: VideoFrameSink,
) -> Result<VideoTapBranch, GstStreamError> {
    let tee = make("tee")?;
    let gui_queue = leaky_queue()?;
    let decoder = make(decoder_name)?;
    if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        return Err(GstStreamError::Link("not supported aarch64".into()));
    }
    let conv = make("videoconvert")?;
    let appsink = build_video_appsink(sink);
    Ok(VideoTapBranch {
        tee,
        gui_chain: vec![gui_queue, decoder, conv, appsink],
    })
}

/// appsink delivering decoded RGBA frames into the tap callback.
/// `drop + max-buffers=2`: a slow consumer drops frames instead of
/// accumulating latency/memory.
fn build_video_appsink(sink: VideoFrameSink) -> gst::Element {
    let appsink = gst_app::AppSink::builder()
        .caps(
            &gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .build(),
        )
        .max_buffers(2u32)
        .drop(true)
        .sync(false)
        .build();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink_el| {
                let sample = sink_el.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                let info = gst_video::VideoInfo::from_caps(caps)
                    .map_err(|_| gst::FlowError::NotNegotiated)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?.to_owned();
                let frame = gst_video::VideoFrame::from_buffer_readable(buffer, &info)
                    .map_err(|_| gst::FlowError::Error)?;
                let data = frame.plane_data(0).map_err(|_| gst::FlowError::Error)?;
                sink.lock()(RawVideoFrame {
                    width: info.width(),
                    height: info.height(),
                    stride: info.stride()[0] as usize,
                    data,
                });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    appsink.upcast()
}

/// appsink delivering decoded S16LE audio chunks into the tap callback.
fn build_audio_appsink(sink: AudioChunkSink) -> gst::Element {
    let appsink = gst_app::AppSink::builder()
        .caps(
            &gst::Caps::builder("audio/x-raw")
                .field("format", "S16LE")
                .build(),
        )
        .max_buffers(8u32)
        .drop(true)
        .sync(false)
        .build();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink_el| {
                let sample = sink_el.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                let s = caps.structure(0).ok_or(gst::FlowError::NotNegotiated)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                sink.lock()(RawAudioChunk {
                    rate: s.get::<i32>("rate").unwrap_or(0) as u32,
                    channels: s.get::<i32>("channels").unwrap_or(0) as u32,
                    data: map.as_slice(),
                });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    appsink.upcast()
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

/// Map an audio caps `encoding-name` (+ static-payload fallback for
/// cameras that omit rtpmap) to (depay element name, codec).
fn audio_codec(encoding: &str, payload: Option<i32>) -> Option<(&'static str, AudioCodec)> {
    match encoding.to_ascii_uppercase().as_str() {
        "PCMA" => Some(("rtppcmadepay", AudioCodec::G711A)),
        "PCMU" => Some(("rtppcmudepay", AudioCodec::G711U)),
        "MP4A-LATM" => Some(("rtpmp4adepay", AudioCodec::Aac)),
        _ => match payload {
            Some(8) => Some(("rtppcmadepay", AudioCodec::G711A)),
            Some(0) => Some(("rtppcmudepay", AudioCodec::G711U)),
            _ => None,
        },
    }
}

/// Create the shared decode segment `depay → decode… → tee`
/// (`avdec_aac` comes from gst-libav — needs `gstreamer1.0-libav` on
/// the target). Elements are NOT linked here; linking happens in
/// `link_audio` after everything is in the pipeline (linking orphan
/// elements gets silently dropped on `add_many`, see link_video).
/// The returned Vec always ends with the tee.
fn build_decode_elements(
    depay_name: &str,
    decode_names: &[&str],
) -> Result<Vec<gst::Element>, GstStreamError> {
    let mut names = vec![depay_name];
    names.extend_from_slice(decode_names);
    names.push("tee");
    build_elements(&names)
}

/// Browser branch elements: `queue → audioconvert → audioresample →
/// opusenc` (unlinked; linked after add in `link_audio`).
fn build_web_audio_elements() -> Result<Vec<gst::Element>, GstStreamError> {
    build_elements(&["queue", "audioconvert", "audioresample", "opusenc"])
}

/// Local playback branch elements: `queue → audioconvert →
/// audioresample → alsasink` (unlinked; linked after add).
fn build_alsa_elements(device: &str) -> Result<Vec<gst::Element>, GstStreamError> {
    let elems = build_elements(&["queue", "audioconvert", "audioresample", "alsasink"])?;
    elems
        .last()
        .expect("branch is non-empty")
        .set_property("device", device);
    Ok(elems)
}

/// Link the audio branch. webrtcsink's `audio_%u` pad only accepts
/// raw/opus, while cameras send G.711/AAC — so unlike video (pass-through)
/// the audio path transcodes: `depay ! decode ! tee`, one tee output to
/// the browser branch (opus), the other (only with `AudioOutput::Alsa`)
/// to local playback.
#[allow(clippy::too_many_arguments)]
fn link_audio(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    s: &gst::StructureRef,
    tracks: &Arc<Mutex<TrackState>>,
    ws: &gst::Element,
    handle: &GstStreamHandle,
    audio_output: &AudioOutput,
    tap: &Option<AudioChunkSink>,
) -> Result<(), GstStreamError> {
    // 获取编码类型后面根据类型选择对应 codec 节点
    let encoding = s.get::<&str>("encoding-name").unwrap_or("");
    let payload = s.get::<i32>("payload").ok();
    let Some((depay_name, codec)) = audio_codec(encoding, payload) else {
        return Err(GstStreamError::Link(format!(
            "unsupported audio codec: encoding={encoding}, payload={payload:?}, track ignored"
        )));
    };
    claim_track(tracks, true, depay_name)?;

    let decode_names = decode_chain_names(codec);
    if decode_names.is_empty() {
        return Err(GstStreamError::Link(format!(
            "no decode chain for codec: {codec:?}, dropping audio track"
        )));
    }

    let chain = build_decode_elements(depay_name, decode_names)?;
    let tee = chain.last().expect("chain always ends with tee").clone();

    let web = build_web_audio_elements()?;
    let alsa = match audio_output {
        AudioOutput::Alsa { device } => Some(build_alsa_elements(device)?),
        AudioOutput::Disabled => None,
    };
    // 原始帧 tap 支路：tee → queue(leaky) → audioconvert → audioresample
    // → appsink(S16LE)，解码后 PCM 直接进回调
    let tap_chain = match tap {
        Some(sink) => {
            let mut elems = build_elements(&["audioconvert", "audioresample"])?;
            let mut full = vec![leaky_queue()?];
            full.append(&mut elems);
            full.push(build_audio_appsink(sink.clone()));
            Some(full)
        }
        None => None,
    };

    // 全部元件先入管道并对齐状态，再做任何链接（孤儿态链接会被
    // add_many 丢掉，见 link_video 注释）
    let mut all: Vec<&gst::Element> = chain.iter().collect();
    all.extend(web.iter());
    if let Some(alsa_elems) = &alsa {
        all.extend(alsa_elems.iter());
    }
    if let Some(tap_elems) = &tap_chain {
        all.extend(tap_elems.iter());
    }
    add_and_sync(pipeline, &all)?;

    let chain_refs: Vec<&gst::Element> = chain.iter().collect();
    link_chain(&chain_refs, "audio decode chain")?;
    let web_refs: Vec<&gst::Element> = web.iter().collect();
    link_chain(&web_refs, "web audio branch")?;
    let ws_pad = request_ws_pad(ws, "audio_%u")?;
    static_pad(web.last().expect("branch is non-empty"), "src")?
        .link(&ws_pad)
        .map_err(|e| GstStreamError::Link(format!("failed to link opusenc to webrtcsink: {e}")))?;
    link_tee_branch(&tee, &web[0])?;
    if let Some(alsa_elems) = &alsa {
        let alsa_refs: Vec<&gst::Element> = alsa_elems.iter().collect();
        link_chain(&alsa_refs, "audio playback chain")?;
        link_tee_branch(&tee, &alsa_elems[0])?;
    }
    if let Some(tap_elems) = &tap_chain {
        let tap_refs: Vec<&gst::Element> = tap_elems.iter().collect();
        link_chain(&tap_refs, "audio tap branch")?;
        link_tee_branch(&tee, &tap_elems[0])?;
    }
    link_rtsp_pad(pad, &chain[0])?;

    // 统计：数 depay 输出的编码帧（解码后样本计数意义不大）
    if let Some(depay_src) = chain[0].static_pad("src") {
        install_stats_probe(&depay_src, handle.clone(), true);
    }
    info!(depay = depay_name, codec = ?codec, alsa = alsa.is_some(), tap = tap_chain.is_some(), "audio track linked (opus → webrtcsink)");
    Ok(())
}

/// Build a list of elements by factory name.
fn build_elements(names: &[&str]) -> Result<Vec<gst::Element>, GstStreamError> {
    names
        .iter()
        .map(|n| make(n))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| GstStreamError::Link(format!("audio branch elements unavailable: {e}")))
}

/// Request one tee src pad and link it to `first`'s sink pad (the first
/// element of a branch chain, normally a queue).
fn link_tee_branch(tee: &gst::Element, first: &gst::Element) -> Result<(), GstStreamError> {
    let sink = first.static_pad("sink").ok_or(GstStreamError::Link(
        "branch head element has no sink pad".into(),
    ))?;
    link_tee_to_pad(tee, &sink)
}

/// Request one tee src pad and link it to an arbitrary target pad —
/// used when the branch head has request pads (e.g. webrtcsink's
/// `video_%u`) instead of a static "sink" pad.
fn link_tee_to_pad(tee: &gst::Element, target: &gst::Pad) -> Result<(), GstStreamError> {
    let tee_src = tee
        .request_pad_simple("src_%u")
        .ok_or(GstStreamError::Link("tee request pad src_%u failed".into()))?;
    tee_src
        .link(target)
        .map_err(|e| GstStreamError::Link(format!("failed to link tee src pad: {e}")))?;
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
    taps: RawTaps,
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
                match build_pipeline(&cfg, handle.clone(), taps.clone()) {
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
