//! Track linking: video / audio pad-added handlers and all their helpers.

use std::sync::Arc;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use parking_lot::Mutex;
use tracing::{debug, error, info, warn};

use super::config::AudioOutput;
use super::pipeline::TrackState;
use super::stats::GstStreamHandle;
use super::tap::{AudioChunkSink, RawAudioChunk, RawTaps, RawVideoFrame, VideoFrameSink};
use crate::GstStreamError;
use crate::gstutil::{add_and_sync, leaky_queue, link_chain, make, static_pad};
use ipcam_core::{AudioCodec, VideoCodec};

/// 分发 rtspsrc 动态 `stream_%u` pad, 按 caps (media + encoding-name) 路由.
/// 相机省略 rtpmap 时用静态 pt 兜底 (RFC 3551).
///
/// 注意: `pad-added` 必须连在 **rtspsrc 元件**上, 不是 pipeline 上,
/// 否则永远收不到 rtspsrc 的 stream pad 事件.
///
/// # Arguments
///
/// * `src`: rtspsrc 元件 (pad-added 事件源)
/// * `pipeline`: GStreamer 管线 (用于添加元件)
/// * `tracks`: Track 状态追踪器 (标记 video/audio 已链接)
/// * `audio_output`: 音频输出模式 (Alsa / Disabled)
/// * `ws`: webrtcsink 元件 (接收链接好的轨道)
/// * `handle`: 会话句柄, 用于错误上报
/// * `taps`: 原始帧回调 (视频/音频 tap sink)
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
pub(super) fn install_pad_added(
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

/// 将 `rtph264depay ! h264parse ! webrtcsink.video_%u` (或 H.265 版)
/// 链接到 rtspsrc 视频 pad. 只消费第一个视频轨道, 多余的记录并忽略.
/// 有 video tap 时在 parse 后插入 tee: WebRTC 直通, 另一路解码为 RGBA.
///
/// # Arguments
///
/// * `pipeline`: GStreamer 管线 (用于添加元件)
/// * `pad`: rtspsrc 动态视频 pad
/// * `s`: pad 的 caps structure (提取 encoding-name)
/// * `tracks`: Track 状态追踪器 (标记 video 已链接)
/// * `ws`: webrtcsink 元件 (请求 video_%u pad 接收轨道)
/// * `handle`: 会话句柄 (挂载统计 probe)
/// * `tap`: 视频帧回调 (Option, 有值时插入 tee + 解码支路)
///
/// returns: Result<(), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub(super) fn link_video(
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

/// Map an audio caps `encoding-name` to (depay element name, codec).
/// 相机省略 rtpmap 时用静态 pt 兜底 (RFC 3551 允许); 编码名/pt 映射
/// 在 ipcam-core, 这里只补 gst 的 depay 元件名
fn audio_codec(encoding: &str, payload: Option<i32>) -> Option<(&'static str, AudioCodec)> {
    let mut codec = AudioCodec::from_name(encoding);
    if matches!(codec, AudioCodec::Unknown) {
        codec = payload
            .and_then(|p| u8::try_from(p).ok())
            .map(AudioCodec::from_static_pt)
            .unwrap_or(AudioCodec::Unknown);
    }
    let depay = match codec {
        AudioCodec::G711A => "rtppcmadepay",
        AudioCodec::G711U => "rtppcmudepay",
        AudioCodec::Aac => "rtpmp4adepay",
        _ => return None,
    };
    Some((depay, codec))
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

/// 链接音频分支. webrtcsink 的 `audio_%u` pad 只接受 raw/opus,
/// 而相机发 G.711/AAC, 因此音频路径需要转码: `depay ! decode ! tee`,
/// 一路 tee 输出给浏览器 (opus), 另一路 (仅 AudioOutput::Alsa) 给本地播放.
///
/// # Arguments
///
/// * `pipeline`: GStreamer 管线 (用于添加元件)
/// * `pad`: rtspsrc 动态音频 pad
/// * `s`: pad 的 caps structure (提取 encoding-name 和 payload)
/// * `tracks`: Track 状态追踪器 (标记 audio 已链接)
/// * `ws`: webrtcsink 元件 (请求 audio_%u pad 接收轨道)
/// * `handle`: 会话句柄 (挂载统计 probe)
/// * `audio_output`: 音频输出模式 (Alsa 本地播放 / Disabled)
/// * `tap`: 音频块回调 (Option, 有值时插入原始帧 tap 支路)
///
/// returns: Result<(), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
#[allow(clippy::too_many_arguments)]
pub(super) fn link_audio(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    s: &gst::StructureRef,
    tracks: &Arc<Mutex<TrackState>>,
    ws: &gst::Element,
    handle: &GstStreamHandle,
    audio_output: &AudioOutput,
    tap: &Option<AudioChunkSink>,
) -> Result<(), GstStreamError> {
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
