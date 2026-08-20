//! Encoded-frame helpers: the `AudioPacket` type plus pure functions
//! for Annex-B NAL splitting, PTS conversion and keyframe detection.

use bytes::Bytes;
use ipcam_core::AudioCodec;

/// One encoded audio frame (AAC raw or a G.711 sample block).
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub codec: AudioCodec,
    pub data: Bytes,
    /// Presentation timestamp in microseconds.
    pub pts_us: i64,
    /// Sample rate in Hz (G.711 is always 8000).
    pub rate: u32,
    /// Channel count (G.711 is always 1).
    pub channels: u32,
}

/// Split an Annex-B access unit into individual NAL units.
///
/// Accepts mixed 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start
/// codes. Each returned NAL keeps its start-code prefix (downstream
/// consumers require `data` to begin with a start code). Empty segments
/// and leading zero bytes before the first start code are ignored, and
/// zero-payload NALs (two start codes back to back) are dropped.
pub fn split_au_into_nals(data: &[u8]) -> Vec<Bytes> {
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (offset, start-code len)
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                starts.push((i, 3));
                i += 3;
                continue;
            }
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                starts.push((i, 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }

    starts
        .iter()
        .enumerate()
        .filter_map(|(idx, &(pos, code_len))| {
            let end = starts.get(idx + 1).map(|&(p, _)| p).unwrap_or(data.len());
            // Drop NALs with no payload beyond the start code.
            (end > pos + code_len).then(|| Bytes::copy_from_slice(&data[pos..end]))
        })
        .collect()
}

/// Convert a GStreamer PTS (nanoseconds) to a 90 kHz RTP timestamp.
/// Wraps at u32 as RTP timestamps do.
pub fn pts_ns_to_rtp_ts90k(ns: u64) -> u32 {
    (ns as u128 * 90_000 / 1_000_000_000) as u32
}

/// Convert a GStreamer PTS (nanoseconds) to microseconds.
pub fn pts_ns_to_us(ns: u64) -> i64 {
    (ns / 1_000) as i64
}

/// First NAL header byte, skipping a leading Annex-B start code if present.
fn nal_header(nal: &[u8]) -> Option<u8> {
    if nal.starts_with(&[0x00, 0x00, 0x00, 0x01]) {
        nal.get(4).copied()
    } else if nal.starts_with(&[0x00, 0x00, 0x01]) {
        nal.get(3).copied()
    } else {
        nal.first().copied()
    }
}

/// H.264: keyframe iff `nal_unit_type == 5` (IDR slice).
pub fn is_keyframe_h264(nal: &[u8]) -> bool {
    nal_header(nal).is_some_and(|h| h & 0x1F == 5)
}

/// H.265: keyframe iff `nal_unit_type` ∈ {19, 20} (IDR_W_RADL / IDR_N_LP).
pub fn is_keyframe_h265(nal: &[u8]) -> bool {
    nal_header(nal).is_some_and(|h| matches!((h >> 1) & 0x3F, 19 | 20))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_single_nal_four_byte_start_code() {
        let au = [0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84];
        let nals = split_au_into_nals(&au);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].as_ref(), &au);
    }

    #[test]
    fn split_mixed_three_and_four_byte_start_codes() {
        let au = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, // SPS (4-byte)
            0x00, 0x00, 0x01, 0x68, 0xCE, // PPS (3-byte)
            0x00, 0x00, 0x00, 0x01, 0x65, 0x88, // IDR (4-byte)
        ];
        let nals = split_au_into_nals(&au);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0].as_ref(), &au[0..6]);
        assert_eq!(nals[1].as_ref(), &au[6..11]);
        assert_eq!(nals[2].as_ref(), &au[11..]);
        // every NAL keeps its start-code prefix
        for nal in &nals {
            assert!(nal.starts_with(&[0, 0, 1]) || nal.starts_with(&[0, 0, 0, 1]));
            assert!(nal.len() >= 5);
        }
    }

    #[test]
    fn split_without_start_code_returns_empty() {
        assert!(split_au_into_nals(&[0x65, 0x88, 0x84]).is_empty());
        assert!(split_au_into_nals(&[]).is_empty());
    }

    #[test]
    fn split_ignores_leading_zeros_and_empty_segments() {
        let au = [0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x65, 0x88];
        let nals = split_au_into_nals(&au);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].as_ref(), &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88]);

        // back-to-back start codes: zero-payload NAL is dropped
        let au = [0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x65, 0x88];
        let nals = split_au_into_nals(&au);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].as_ref(), &[0x00, 0x00, 0x01, 0x65, 0x88]);
    }

    #[test]
    fn pts_conversion_known_values() {
        assert_eq!(pts_ns_to_rtp_ts90k(1_000_000_000), 90_000);
        assert_eq!(pts_ns_to_us(1_000_000_000), 1_000_000);
        assert_eq!(pts_ns_to_rtp_ts90k(0), 0);
        // u32 wrap: 47_721_858_844_445 ns * 90000 / 1e9 = 2^32 exactly → 0
        assert_eq!(pts_ns_to_rtp_ts90k(47_721_858_844_445), 0);
        // one ns earlier is u32::MAX
        assert_eq!(pts_ns_to_rtp_ts90k(47_721_858_844_444), u32::MAX);
    }

    #[test]
    fn h264_keyframe_detection() {
        // IDR (type 5) with 4-byte start code
        assert!(is_keyframe_h264(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x88]));
        // IDR with 3-byte start code
        assert!(is_keyframe_h264(&[0x00, 0x00, 0x01, 0x65, 0x88]));
        // non-IDR slice (type 1)
        assert!(!is_keyframe_h264(&[0x00, 0x00, 0x00, 0x01, 0x41, 0x9A]));
        // SPS (type 7) is not a keyframe
        assert!(!is_keyframe_h264(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x42]));
        assert!(!is_keyframe_h264(&[]));
    }

    #[test]
    fn h265_keyframe_detection() {
        // nal[0] = 0x26 → type (0x26 >> 1) & 0x3F = 19 (IDR_W_RADL)
        assert!(is_keyframe_h265(&[
            0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xAF
        ]));
        // nal[0] = 0x28 → type 20 (IDR_N_LP)
        assert!(is_keyframe_h265(&[0x00, 0x00, 0x01, 0x28, 0x01]));
        // nal[0] = 0x02 → type 1 (TRAIL_R) is not a keyframe
        assert!(!is_keyframe_h265(&[0x00, 0x00, 0x00, 0x01, 0x02, 0x01]));
        // nal[0] = 0x42 → type 33 (SPS) is not a keyframe
        assert!(!is_keyframe_h265(&[0x00, 0x00, 0x00, 0x01, 0x42, 0x01]));
        assert!(!is_keyframe_h265(&[]));
    }
}
