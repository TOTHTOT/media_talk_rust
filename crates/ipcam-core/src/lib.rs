use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod error;
pub mod sink;

pub use error::{CoreError, CoreResult};
pub use sink::{AudioSink, VideoSink};

pub type DeviceId = Uuid;
pub type SessionId = Uuid;

pub type PtsMicros = i64;
pub type RtpTimestamp = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
    Unknown,
}

impl VideoCodec {
    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_uppercase().as_str() {
            "H264" | "AVC" | "AVC1" => Self::H264,
            "H265" | "HEVC" => Self::H265,
            "AV1" => Self::Av1,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Aac,
    Opus,
    G711A,
    G711U,
    Unknown,
}

impl AudioCodec {
    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_uppercase().as_str() {
            "AAC" | "MPEG4-GENERIC" => Self::Aac,
            "OPUS" => Self::Opus,
            "PCMA" => Self::G711A,
            "PCMU" => Self::G711U,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoProfile {
    pub profile_id: String,
    pub uri: Option<String>,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub id: DeviceId,
    pub address: String,
    pub xaddr: Option<String>,
    pub scopes: Vec<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub auth_status: AuthStatus,
    pub profiles: Vec<VideoProfile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStatus {
    Unknown,
    Anonymous,
    Valid,
    InvalidCredentials,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodedPacket {
    pub codec: VideoCodec,
    pub data: Bytes,
    pub rtp_ts: RtpTimestamp,
    pub arrival_us: i64,
    pub is_keyframe: bool,
    /// RTP marker bit (RFC 3550): set on the last packet of an access
    /// unit. Consumers that aggregate NALs into access units SHOULD
    /// flush their buffer when this is true.
    #[serde(default)]
    pub marker: bool,
}

#[derive(Debug)]
pub struct DecodedFrame {
    pub handle: DecodedHandle,
    pub pts: PtsMicros,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Nv12,
    I420,
    Rgba,
    Bgra,
}

#[derive(Debug)]
pub struct DecodedHandle {
    inner: Box<dyn FrameBackend>,
}

impl DecodedHandle {
    pub fn new(inner: Box<dyn FrameBackend>) -> Self {
        Self { inner }
    }

    pub fn ptr(&self) -> *const u8 {
        self.inner.ptr()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn release(self) {
        self.inner.release();
    }
}

pub trait FrameBackend: std::fmt::Debug + Send {
    fn ptr(&self) -> *const u8;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool;
    fn release(self: Box<Self>);
}

impl FrameBackend for Vec<u8> {
    fn ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn len(&self) -> usize {
        self.len()
    }

    fn is_empty(&self) -> bool {
        self.is_empty()
    }

    fn release(self: Box<Self>) {
        drop(self);
    }
}

#[async_trait]
pub trait Decoder: Send + Sync {
    fn codec(&self) -> VideoCodec;

    async fn submit(&self, packet: EncodedPacket) -> CoreResult<Option<DecodedFrame>>;

    fn set_recovery_strategy(&mut self, strategy: RecoveryStrategy);

    async fn reset(&self) -> CoreResult<()>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStrategy {
    None,
    DropFrames,
    #[default]
    ResetOnError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub device_id: DeviceId,
    pub profile_id: String,
    pub state: SessionState,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Creating,
    Ready,
    Stalled,
    Ended,
}

pub fn now_micros() -> i64 {
    chrono::Utc::now().timestamp_micros()
}

pub fn duration_micros(d: Duration) -> i64 {
    d.as_micros() as i64
}

/// Cumulative NAL-unit statistics over a window of received
/// `EncodedPacket`s. Built up by a callback from
/// `ipcam_rtsp::RtspClient::play_loop`; used by `media_talk probe` to
/// summarise a stream without rendering it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NalStats {
    /// Number of frame boundaries observed (= RTP marker=1 packets).
    pub access_units: u64,
    /// Number of NAL units whose type byte (after Annex-B start code)
    /// is 5 (IDR). Per-NAL count, not per-access-unit.
    pub idr_count: u64,
    /// First SPS seen (NAL type 7), as RBSP without the start code.
    pub sps: Option<Vec<u8>>,
    /// First PPS seen (NAL type 8), as RBSP without the start code.
    pub pps: Option<Vec<u8>>,
    /// NAL type byte (lower 5 bits) → count.
    pub nal_by_type: BTreeMap<u8, u64>,
    /// Total NAL payload bytes (excluding start codes).
    pub bytes_total: u64,
    /// Wall-clock duration covered by these stats.
    #[serde(with = "duration_micros_compat")]
    pub elapsed: Duration,
    /// Elapsed from play-loop start to the first IDR, if any.
    #[serde(default, with = "duration_micros_opt_compat")]
    pub time_to_first_idr: Option<Duration>,
}

/// Read the NAL unit type from an H.264 Annex-B-framed NAL. The NAL is
/// expected to start with `00 00 00 01` followed by a header byte whose
/// lower 5 bits hold the type. Returns `None` if the buffer is too
/// short to contain a start code plus header.
pub fn classify_h264_nal(data: &[u8]) -> Option<u8> {
    if data.len() < 5 {
        return None;
    }
    Some(data[4] & 0x1F)
}

mod duration_micros_compat {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_micros() as u64)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let us = u64::deserialize(d)?;
        Ok(Duration::from_micros(us))
    }
}

mod duration_micros_opt_compat {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(dur) => s.serialize_some(&(dur.as_micros() as u64)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let v: Option<u64> = Option::deserialize(d)?;
        Ok(v.map(Duration::from_micros))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_idr_nal() {
        // 00 00 00 01 65 ... → NAL type 5 (IDR slice)
        let nalu = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB];
        assert_eq!(classify_h264_nal(&nalu), Some(5));
    }

    #[test]
    fn classify_sps_nal() {
        // 00 00 00 01 67 ... → NAL type 7 (SPS)
        let nalu = [0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0];
        assert_eq!(classify_h264_nal(&nalu), Some(7));
    }

    #[test]
    fn classify_rejects_short_buffer() {
        assert_eq!(classify_h264_nal(&[0, 0, 0, 1, 0x65]), Some(5));
        assert_eq!(classify_h264_nal(&[0, 0, 0, 1]), None);
        assert_eq!(classify_h264_nal(&[]), None);
    }

    #[test]
    fn nal_stats_default_is_empty() {
        let s = NalStats::default();
        assert_eq!(s.access_units, 0);
        assert_eq!(s.idr_count, 0);
        assert!(s.sps.is_none());
        assert!(s.pps.is_none());
        assert!(s.nal_by_type.is_empty());
        assert_eq!(s.bytes_total, 0);
        assert!(s.time_to_first_idr.is_none());
    }
}
