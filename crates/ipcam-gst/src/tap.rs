//! Optional raw-frame taps: tee decoded video/audio out of the streaming
//! pipeline into caller-provided callbacks (for native GUI texture upload,
//! analytics, recording, …). The WebRTC browser path is unaffected.
//!
//! Register taps via [`crate::start_with_taps`]. Callbacks run on GStreamer
//! streaming threads — never touch UI state directly from them; forward the
//! frame over a channel to the UI thread instead.

use std::sync::Arc;

use parking_lot::Mutex;

/// One decoded video frame in tightly packed RGBA (stride may exceed
/// `width * 4` due to alignment — always honour `stride`).
///
/// `data` borrows the GStreamer buffer and is only valid for the duration
/// of the callback; copy it if it must outlive the call.
pub struct RawVideoFrame<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes per row including padding.
    pub stride: usize,
    /// RGBA pixels, `stride * height` bytes.
    pub data: &'a [u8],
}

/// One chunk of decoded audio, interleaved signed 16-bit little-endian
/// (`audio/x-raw,format=S16LE`). `data` is only valid during the callback.
pub struct RawAudioChunk<'a> {
    pub rate: u32,
    pub channels: u32,
    /// Interleaved S16LE samples (`len / 2` samples, `/ channels` per channel).
    pub data: &'a [u8],
}

/// Video tap callback storage (behind Arc+Mutex so a session rebuild can
/// reuse the same sink).
pub type VideoFrameSink = Arc<Mutex<dyn FnMut(RawVideoFrame<'_>) + Send + 'static>>;
/// Audio tap callback storage.
pub type AudioChunkSink = Arc<Mutex<dyn FnMut(RawAudioChunk<'_>) + Send + 'static>>;

/// Optional raw-frame taps attached to a streaming session. Each tap tees
/// an extra decode branch off the pipeline; `None` keeps the pipeline
/// exactly as before (no decode cost).
#[derive(Clone, Default)]
pub struct RawTaps {
    /// Called per decoded video frame (RGBA). Decode happens only when set.
    pub video: Option<VideoFrameSink>,
    /// Called per decoded audio chunk (S16LE). No-op for cameras without
    /// an audio track.
    pub audio: Option<AudioChunkSink>,
}
