//! RTP packet parsing + H.264 de-packetization (RFC 6184).
//!
//! Implements just enough of the RFCs for an ONVIF RTSP source:
//!
//! - RFC 3550 RTP header (no padding, no CSRC list, optional one-byte
//!   extension header).
//! - RFC 6184 §5.6 H.264 packetization:
//!   * Single NAL unit (Type 1..23) – emitted verbatim.
//!   * STAP-A (Type 24) – aggregated NALs split out individually.
//!   * FU-A (Type 28) – fragmented NALs reassembled across packets.
//!
//! STAP-B, MTAP and FU-B are uncommon in ONVIF camera streams and are
//! silently skipped. H.265 (RFC 7798) and audio codecs are not yet
//! wired through; audio RTP packets are returned as empty.

use bytes::{Bytes, BytesMut};

const ANNEX_B_START: [u8; 4] = [0, 0, 0, 1];

#[derive(Debug, Clone, Copy)]
pub struct RtpHeader {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload_offset: usize,
}

pub fn parse_rtp_header(buf: &[u8]) -> Option<RtpHeader> {
    if buf.len() < 12 {
        return None;
    }
    let byte0 = buf[0];
    let version = (byte0 >> 6) & 0x3;
    if version != 2 {
        return None;
    }
    let padding = (byte0 & 0x20) != 0;
    let extension = (byte0 & 0x10) != 0;
    let cc = (byte0 & 0x0F) as usize;

    let byte1 = buf[1];
    let marker = (byte1 & 0x80) != 0;
    let payload_type = byte1 & 0x7F;
    let sequence = u16::from_be_bytes([buf[2], buf[3]]);
    let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);

    let mut offset = 12 + cc * 4;
    if extension {
        if buf.len() < offset + 4 {
            return None;
        }
        let ext_words = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
        offset += 4 + ext_words * 4;
    }
    if padding {
        // Final byte of payload is the length of the padding (including
        // itself). Only meaningful if the payload is interpretable.
        if offset < buf.len() {
            let pad = buf[buf.len() - 1] as usize;
            if pad <= buf.len() - offset {
                // valid; payload length is computed by callers using buf.len()
                let _ = pad;
            }
        }
    }
    if offset > buf.len() {
        return None;
    }
    Some(RtpHeader {
        marker,
        payload_type,
        sequence,
        timestamp,
        ssrc,
        payload_offset: offset,
    })
}

pub struct H264Depacketizer {
    fua_buffer: BytesMut,
    fua_nri: u8,
    fua_type: u8,
    fua_have_start: bool,
    fua_have_middle: bool,
}

impl H264Depacketizer {
    pub fn new() -> Self {
        Self {
            fua_buffer: BytesMut::new(),
            fua_nri: 0,
            fua_type: 0,
            fua_have_start: false,
            fua_have_middle: false,
        }
    }

    /// Push one RTP payload. Returns any complete Annex-B NAL units
    /// produced (each with `00 00 00 01` prefix). FU-A fragments are
    /// held until the E bit is seen.
    pub fn push(&mut self, payload: &[u8]) -> Vec<Bytes> {
        let mut out = Vec::new();
        if payload.is_empty() {
            return out;
        }
        let nal_byte = payload[0];
        let nal_type = nal_byte & 0x1F;
        match nal_type {
            1..=23 => {
                self.flush_partial_fua();
                let mut buf = BytesMut::with_capacity(ANNEX_B_START.len() + payload.len());
                buf.extend_from_slice(&ANNEX_B_START);
                buf.extend_from_slice(payload);
                out.push(buf.freeze());
            }
            24 => {
                // STAP-A: F/NRI/Type=24, then u16 size + NAL, repeated.
                self.flush_partial_fua();
                let mut i = 1;
                while i + 2 <= payload.len() {
                    let nalu_size = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
                    i += 2;
                    if i + nalu_size > payload.len() {
                        break;
                    }
                    let nalu = &payload[i..i + nalu_size];
                    let mut buf = BytesMut::with_capacity(ANNEX_B_START.len() + nalu_size);
                    buf.extend_from_slice(&ANNEX_B_START);
                    buf.extend_from_slice(nalu);
                    out.push(buf.freeze());
                    i += nalu_size;
                }
            }
            28 => {
                // FU-A: F/NRI/Type=28, FU-header (1 byte), then payload.
                if payload.len() < 2 {
                    return out;
                }
                let fu_header = payload[1];
                let s = (fu_header & 0x80) != 0;
                let e = (fu_header & 0x40) != 0;
                let fu_type = fu_header & 0x1F;
                let nri = nal_byte & 0xE0;
                let _ = (s, fu_type);
                if s {
                    if self.fua_have_start && self.fua_buffer.is_empty() {
                        // stray S=1 right after a previous S=1 with no E.
                        // Keep the older one (defensive).
                    } else {
                        self.fua_nri = nri;
                        self.fua_type = fu_type;
                        self.fua_buffer.clear();
                        // reconstructed header = (nri | fu_type)
                        self.fua_buffer.extend_from_slice(&[nri | fu_type]);
                        self.fua_have_start = true;
                        self.fua_have_middle = false;
                    }
                }
                if !self.fua_have_start {
                    return out;
                }
                self.fua_buffer.extend_from_slice(&payload[2..]);
                self.fua_have_middle = true;
                if e {
                    let mut buf =
                        BytesMut::with_capacity(ANNEX_B_START.len() + self.fua_buffer.len());
                    buf.extend_from_slice(&ANNEX_B_START);
                    buf.extend_from_slice(&self.fua_buffer);
                    out.push(buf.freeze());
                    self.fua_buffer.clear();
                    self.fua_have_start = false;
                    self.fua_have_middle = false;
                }
            }
            _ => {
                // STAP-B (25), MTAP16 (26), MTAP24 (27), FU-B (29) are
                // rare in ONVIF and unsupported here. Drop silently.
                self.flush_partial_fua();
            }
        }
        out
    }

    /// Drop any incomplete FU-A buffer (used on stream teardown or
    /// significant timeline gap).
    pub fn flush_partial_fua(&mut self) {
        self.fua_buffer.clear();
        self.fua_have_start = false;
        self.fua_have_middle = false;
    }
}

impl Default for H264Depacketizer {
    fn default() -> Self {
        Self::new()
    }
}

/// True if the NAL unit byte's type field equals 5 (Coded slice of an
/// IDR picture in H.264).
pub fn is_h264_keyframe(nal_byte_after_start_code: u8) -> bool {
    nal_byte_after_start_code & 0x1F == 5
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp_wrap(payload: &[u8], seq: u16, ts: u32, marker: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + payload.len());
        // V=2, P=0, X=0, CC=0
        out.push(0x80);
        // M=marker, PT=96
        out.push(if marker { 0xE0 } else { 0x60 });
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&ts.to_be_bytes());
        out.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn parse_rtp_header_basic() {
        let buf = rtp_wrap(&[0x67, 0x42], 1, 0x0A, false);
        let h = parse_rtp_header(&buf).expect("rtp");
        assert_eq!(h.payload_type, 96);
        assert_eq!(h.sequence, 1);
        assert_eq!(h.timestamp, 0x0A);
        assert_eq!(h.ssrc, 0xAABBCCDD);
        assert!(!h.marker);
        assert_eq!(h.payload_offset, 12);
    }

    #[test]
    fn parse_rtp_header_with_marker() {
        let buf = rtp_wrap(&[0x65], 5, 100, true);
        let h = parse_rtp_header(&buf).unwrap();
        assert!(h.marker);
    }

    #[test]
    fn parse_rtp_header_rejects_non_v2() {
        let buf = [0x00, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(parse_rtp_header(&buf).is_none());
    }

    #[test]
    fn single_nal_emits_with_start_code() {
        let mut d = H264Depacketizer::new();
        let nalu = [0x67, 0x42, 0xC0, 0x1E];
        let out = d.push(&nalu);
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0][..4], &[0, 0, 0, 1]);
        assert_eq!(&out[0][4..], &nalu[..]);
    }

    #[test]
    fn fu_a_round_trip() {
        let mut d = H264Depacketizer::new();
        // FU indicator: F=0 NRI=3 Type=28  -> 0x7C
        // FU header:   S=1 E=0 R=0 Type=5  -> 0x85
        let s1 = [0x7C, 0x85, 0xAA, 0xAA];
        let mid = [0x7C, 0x05, 0xBB, 0xBB];
        // FU header E=1 -> 0x45
        let end = [0x7C, 0x45, 0xCC, 0xCC];
        assert!(d.push(&s1).is_empty());
        assert!(d.push(&mid).is_empty());
        let out = d.push(&end);
        assert_eq!(out.len(), 1);
        // Reconstructed header: (0x7C & 0xE0) | 5 = 0x65 (IDR NRI=3)
        assert_eq!(&out[0][..4], &[0, 0, 0, 1]);
        assert_eq!(out[0][4], 0x65);
        assert_eq!(&out[0][5..], &[0xAA, 0xAA, 0xBB, 0xBB, 0xCC, 0xCC]);
    }

    #[test]
    fn fu_a_dropped_on_teardown() {
        let mut d = H264Depacketizer::new();
        // Start a fragment, then never send E.
        d.push(&[0x7C, 0x85, 0xAA]);
        d.flush_partial_fua();
        // Next packet must be ignored until another S.
        assert!(d.push(&[0x7C, 0x45, 0xBB]).is_empty());
        // Now a fresh S+E
        let out = d.push(&[0x7C, 0xC5, 0xCC]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn stap_a_emits_each_nal() {
        let mut d = H264Depacketizer::new();
        // STAP-A indicator: F=0 NRI=3 Type=24 -> 0x78
        // Then u16 size, NAL, repeat.
        let p = [
            0x78, // STAP-A header
            0x00, 0x03, 0x67, 0x42, 0xC0, // SPS
            0x00, 0x02, 0x68, 0xCE, // PPS
        ];
        let out = d.push(&p);
        assert_eq!(out.len(), 2);
        assert_eq!(&out[0][4..], &[0x67, 0x42, 0xC0][..]);
        assert_eq!(&out[1][4..], &[0x68, 0xCE][..]);
    }

    #[test]
    fn is_keyframe_detects_idr() {
        assert!(is_h264_keyframe(0x65));
        assert!(!is_h264_keyframe(0x67)); // SPS
        assert!(!is_h264_keyframe(0x68)); // PPS
        assert!(!is_h264_keyframe(0x41)); // non-IDR slice
    }
}
