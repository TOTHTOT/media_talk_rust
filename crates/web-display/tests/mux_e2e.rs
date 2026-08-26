//! End-to-end test for the fMP4 muxer.
//!
//! Verifies that:
//!   - `push_access_unit` writes ONE moof+mdat per access unit, not per NAL,
//!     and the whole AU is a single trun sample (sample_count == 1).
//!   - Segments carry REAL timing: sample_duration is the RTP delta to the
//!     next frame, tfdt is relative to the first AU. Emission is delayed by
//!     one frame because the duration needs the successor's timestamp.
//!   - The data_offset in trun points past the mdat box header (8 bytes).
//!
//! This test asserts the **semantic** correctness that the unit tests in
//! mux.rs cannot catch — namely that one access unit == one sample.

use bytes::{Bytes, BytesMut};

use web_display::mux::{AvcConfig, Fmp4Muxer};

fn annex_b_nal(nal_byte: u8, payload: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(4 + 1 + payload.len());
    buf.extend_from_slice(&[0, 0, 0, 1]);
    buf.extend_from_slice(&[nal_byte]);
    buf.extend_from_slice(payload);
    buf.freeze()
}

fn read_u32(b: &[u8], pos: usize) -> u32 {
    u32::from_be_bytes([b[pos], b[pos + 1], b[pos + 2], b[pos + 3]])
}

fn find_box(b: &[u8], name: &[u8; 4]) -> Option<usize> {
    b.windows(4).position(|w| w == name)
}

#[test]
fn access_unit_with_4_nals_emits_one_moof_with_one_sample() {
    let mut m = Fmp4Muxer::new();
    let sps = [0x67u8, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0, 0x47, 0xFE, 0xC8];
    let pps = [0x68u8, 0xCE, 0x38, 0x80];
    m.set_avc_config(AvcConfig {
        sps: Bytes::copy_from_slice(&sps),
        pps: Bytes::copy_from_slice(&pps),
    });
    m.set_dimensions(1920, 1080);

    // One access unit: SPS + PPS + IDR + 1 slice NAL (the typical shape
    // for the very first frame of a stream).
    let nalus = vec![
        annex_b_nal(
            0x67,
            &[0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0, 0x47, 0xFE, 0xC8],
        ),
        annex_b_nal(0x68, &[0xCE, 0x38, 0x80]),
        annex_b_nal(0x65, &[0xAA; 64]),
        annex_b_nal(0x41, &[0xBB; 128]),
    ];
    m.push_access_unit(&nalus, 42_000);
    assert_eq!(
        m.segment_count(),
        0,
        "first AU stays pending until the next frame's timestamp"
    );
    // A second AU (any content) supplies the delta that emits the first.
    m.push_access_unit(&[annex_b_nal(0x41, &[0xCC; 16])], 42_000 + 9000);

    let segs = m.take_segments_since(0);
    assert_eq!(segs.len(), 1, "exactly one segment for the first access unit");
    let seg = segs.into_iter().next().unwrap();

    // Expect moof + mdat
    let moof_pos = find_box(&seg, b"moof").expect("moof");
    let mdat_pos = find_box(&seg, b"mdat").expect("mdat");
    assert!(moof_pos < mdat_pos, "moof precedes mdat");
    // moof box header is 8 bytes (4 size + 4 'moof'); the size field sits
    // immediately before the 'moof' tag.
    let moof_size = read_u32(&seg, moof_pos - 4) as usize;
    // mdat box header is also 8 bytes (4 size + 4 'mdat'), so the mdat
    // box itself starts at `mdat_pos - 4`. The moof's last byte is the
    // byte before the mdat box header.
    assert_eq!(
        moof_pos - 4 + moof_size,
        mdat_pos - 4,
        "moof ends right before mdat box"
    );

    // trun body (flags 0x00000301): [0..4) version+flags, [4..8) sample_count,
    // [8..12) data_offset, [12..16) sample_duration, [16..20) sample_size
    let trun_pos = find_box(&seg, b"trun").expect("trun");
    let body = trun_pos + 4; // skip box name
    assert_eq!(read_u32(&seg, body), 0x00000301, "trun flags");
    assert_eq!(
        read_u32(&seg, body + 4),
        1,
        "one trun sample for the WHOLE access unit"
    );

    // tfhd.default-base-is-moof is set, so trun.data_offset is relative
    // to moof start. The sample is at moof_size + 8 (8 = mdat header).
    let data_offset = read_u32(&seg, body + 8);
    assert_eq!(
        data_offset as usize,
        moof_size + 8,
        "trun.data_offset must point to the sample byte inside mdat (moof_size + 8)"
    );

    // Real timing: duration is the RTP delta to the second frame, and the
    // first fragment's decode time is zero relative to the first AU.
    assert_eq!(
        read_u32(&seg, body + 12),
        9000,
        "sample_duration = real RTP delta to the next frame"
    );
    let tfdt_pos = find_box(&seg, b"tfdt").expect("tfdt");
    // tfdt body: [0..4) version+flags, [4..8) baseMediaDecodeTime
    assert_eq!(
        read_u32(&seg, tfdt_pos + 4 + 4),
        0,
        "first fragment starts at t=0"
    );

    // The single sample covers every NAL: each contributes its 4-byte
    // length prefix + NAL body (Annex-B start code stripped).
    let sample_size = read_u32(&seg, body + 16);
    let expected: usize = nalus
        .iter()
        .map(|n| 4 + (n.len() - 4 /* strip Annex-B start code */))
        .sum();
    assert_eq!(sample_size as usize, expected, "sample_size = whole AU");

    // mdat payload is exactly that one sample; the segment is moof + mdat
    // with no trailing bytes.
    let mdat_size = read_u32(&seg, mdat_pos - 4) as usize;
    assert_eq!(mdat_size, 8 + expected, "mdat box size = header (8) + sample");
    assert_eq!(
        seg.len(),
        moof_size + mdat_size,
        "segment length = moof + mdat (no extra bytes)"
    );
}

#[test]
fn two_access_units_emit_two_segments() {
    let mut m = Fmp4Muxer::new();
    m.set_avc_config(AvcConfig {
        sps: Bytes::from_static(&[0x67, 0x42]),
        pps: Bytes::from_static(&[0x68, 0xCE]),
    });
    m.set_dimensions(320, 240);

    let frame1 = vec![annex_b_nal(0x65, &[1; 32])];
    let frame2 = vec![annex_b_nal(0x41, &[2; 32])];
    let frame3 = vec![annex_b_nal(0x41, &[3; 32])];
    // 3 frames → 2 segments; the last frame stays pending its successor.
    m.push_access_unit(&frame1, 0);
    m.push_access_unit(&frame2, 9000);
    m.push_access_unit(&frame3, 18000);

    assert_eq!(m.segment_count(), 2);
    let mut segs = m.take_segments_since(0);
    let s1 = segs.remove(0);
    let s2 = segs.remove(0);
    let trun1_count = {
        let p = find_box(&s1, b"trun").unwrap() + 4;
        read_u32(&s1, p + 4)
    };
    let trun2_count = {
        let p = find_box(&s2, b"trun").unwrap() + 4;
        read_u32(&s2, p + 4)
    };
    assert_eq!(trun1_count, 1);
    assert_eq!(trun2_count, 1);

    // Sequence numbers should be 1 then 2.
    let seq1 = {
        let p = find_box(&s1, b"mfhd").unwrap() + 4 + 4;
        read_u32(&s1, p)
    };
    let seq2 = {
        let p = find_box(&s2, b"mfhd").unwrap() + 4 + 4;
        read_u32(&s2, p)
    };
    assert_eq!(seq1, 1);
    assert_eq!(seq2, 2);

    // Second segment: tfdt = 9000 (second frame's decode time), and its
    // sample_duration = 9000 (delta to the third frame).
    let tfdt2 = {
        let p = find_box(&s2, b"tfdt").unwrap() + 4;
        read_u32(&s2, p + 4)
    };
    assert_eq!(tfdt2, 9000);
    let dur2 = {
        let p = find_box(&s2, b"trun").unwrap() + 4;
        read_u32(&s2, p + 12)
    };
    assert_eq!(dur2, 9000);
}

#[test]
fn empty_access_unit_is_dropped() {
    let mut m = Fmp4Muxer::new();
    m.set_avc_config(AvcConfig {
        sps: Bytes::from_static(&[0x67]),
        pps: Bytes::from_static(&[0x68]),
    });
    m.set_dimensions(640, 480);
    m.push_access_unit(&[], 0);
    assert_eq!(m.segment_count(), 0);
}

#[test]
fn push_before_init_is_dropped() {
    let mut m = Fmp4Muxer::new();
    m.push_access_unit(&[annex_b_nal(0x65, &[0])], 0);
    assert_eq!(m.segment_count(), 0);
}
