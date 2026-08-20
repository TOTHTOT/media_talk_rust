//! GStreamer-based RTSP A/V ingest engine (replaces `ipcam-rtsp` `play_loop`).
//!
//! Pulls an RTSP stream from an IP camera and delivers encoded frames
//! through callbacks: video as [`ipcam_core::EncodedPacket`] (Annex-B,
//! one NAL per packet) and audio as [`AudioPacket`]. Session state and
//! counters are observable through [`GstStreamHandle`].
//!
//! The GStreamer pipeline itself lives behind the `gst` feature
//! (requires GStreamer + pkg-config on the build host). Without it the
//! crate still compiles — [`start`] validates the config and then
//! returns [`GstStreamError::Init`].

use thiserror::Error;

pub mod config;
pub mod packet;
pub mod stats;

#[cfg(feature = "gst")]
mod pipeline;

pub use config::{AudioOutput, GstStreamConfig, ReconnectPolicy};
pub use packet::AudioPacket;
pub use stats::{GstStreamHandle, StreamState, StreamStats};

use ipcam_core::EncodedPacket;

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
}

/// Validate a stream configuration without touching the network.
pub fn validate(cfg: &GstStreamConfig) -> Result<(), GstStreamError> {
    config::validate(cfg)
}

/// Start a streaming session: build the pipeline and enter the
/// background event loop. Frames are delivered on GStreamer streaming
/// threads, so the callbacks must be lightweight (clone the `Bytes`
/// and hand off; never block).
pub fn start<V, A>(
    cfg: GstStreamConfig,
    on_video: V,
    on_audio: A,
) -> Result<GstStreamHandle, GstStreamError>
where
    V: FnMut(EncodedPacket) + Send + 'static,
    A: FnMut(AudioPacket) + Send + 'static,
{
    validate(&cfg)?;
    #[cfg(feature = "gst")]
    {
        pipeline::start(cfg, on_video, on_audio)
    }
    #[cfg(not(feature = "gst"))]
    {
        let _ = (on_video, on_audio);
        Err(GstStreamError::Init(
            "compiled without `gst` feature".into(),
        ))
    }
}
