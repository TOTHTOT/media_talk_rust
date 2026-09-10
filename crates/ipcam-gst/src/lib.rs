//! GStreamer-based RTSP → WebRTC republishing engine.
//!
//! Pulls an RTSP stream from an IP camera and republishes it to browsers
//! through `webrtcsink` (rswebrtc): the pipeline is
//! `rtspsrc → depay → parse → webrtcsink`, and every session registers
//! itself as a named producer on the process-wide signalling server
//! started by [`ensure_signalling_server`]. Session state and counters
//! are observable through [`GstStreamHandle`].
//!
//! The GStreamer pipeline requires GStreamer + pkg-config on the build
//! host; the bindings are an unconditional dependency of this crate.

use std::sync::OnceLock;

use gstreamer as gst;
use gstreamer::prelude::*;
use thiserror::Error;

pub mod config;
pub mod stats;
pub mod tap;

mod pipeline;

pub use config::{AudioOutput, GstStreamConfig, ReconnectPolicy};
pub use stats::{GstStreamHandle, StreamState, StreamStats};
pub use tap::{AudioChunkSink, RawAudioChunk, RawTaps, RawVideoFrame, VideoFrameSink};

#[derive(Debug, Error)]
pub enum GstStreamError {
    /// Static configuration error, reported by [`validate`].
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    /// GStreamer init / missing element (e.g. `rtspsrc` from plugins-good).
    #[error("init failed: {0}")]
    Init(String),
    /// Connection failure (auth failures included; the message carries
    /// 401/Unauthorized wording to stay compatible with `probe`).
    #[error("connect failed: {0}")]
    Connect(String),
    /// Runtime bus ERROR during playback.
    #[error("stream error: {0}")]
    Stream(String),
    /// Video/audio branch link failure.
    #[error("link error: {0}")]
    Link(String),
}

/// Validate a stream configuration without touching the network.
pub fn validate(cfg: &GstStreamConfig) -> Result<(), GstStreamError> {
    config::validate(cfg)
}

/// Anchor pipeline hosting the process-wide WebRTC signalling server.
static SIGNALLING_ANCHOR: OnceLock<gst::Pipeline> = OnceLock::new();

/// Start the shared WebRTC signalling server (default 0.0.0.0:8443) if
/// it is not running yet. The server lives on an anchor `webrtcsink`
/// held in a static for the process lifetime; every streaming session
/// then connects to it as a producer via `signalling_host/port`.
/// Idempotent — safe to call from every server startup.
pub fn ensure_signalling_server() -> Result<(), GstStreamError> {
    gst::init().map_err(|e| GstStreamError::Init(format!("gst init: {e}")))?;
    if SIGNALLING_ANCHOR.get().is_some() {
        return Ok(());
    }
    let pipeline = gst::Pipeline::new();
    let ws = gst::ElementFactory::make("webrtcsink")
        .build()
        .map_err(|e| GstStreamError::Init(format!("missing element `webrtcsink`: {e}")))?;
    ws.set_property("run-signalling-server", true);
    ws.set_property_from_str("meta", "meta,name=signalling-anchor");
    pipeline
        .add(&ws)
        .map_err(|e| GstStreamError::Init(format!("anchor pipeline add webrtcsink: {e}")))?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| GstStreamError::Init(format!("anchor pipeline set Playing: {e}")))?;
    // Lost a race with a concurrent caller: tear the duplicate down.
    if SIGNALLING_ANCHOR.set(pipeline.clone()).is_err() {
        let _ = pipeline.set_state(gst::State::Null);
    }
    Ok(())
}

/// Start a streaming session: build the RTSP → webrtcsink pipeline and
/// enter the background event loop. The stream becomes visible to WebRTC
/// consumers under `cfg.stream_name` once the pipeline reaches Playing.
pub fn start(cfg: GstStreamConfig) -> Result<GstStreamHandle, GstStreamError> {
    start_with_taps(cfg, RawTaps::default())
}

/// Like [`start`], additionally teeing decoded frames into `taps`
/// (native GUI / analytics). Each set tap adds a decode branch; with no
/// taps this is identical to [`start`].
pub fn start_with_taps(
    cfg: GstStreamConfig,
    taps: RawTaps,
) -> Result<GstStreamHandle, GstStreamError> {
    validate(&cfg)?;
    pipeline::start(cfg, taps)
}
