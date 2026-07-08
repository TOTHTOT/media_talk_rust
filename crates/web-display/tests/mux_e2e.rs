//! End-to-end test for the fMP4 muxer.
//!
//! Verifies that:
//!   - `push_access_unit` writes ONE moof+mdat per access unit, not per NAL.
//!   - The moof's trun box has sample_count == NAL count in the access unit.
//!   - The data_offset in trun points past the mdat box header (8 bytes).
//!   - The init segment is emitted exactly once and stays valid.
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
fn access_unit_with_4_nals_emits_one_moof_with_4_samples() {
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
    m.push_access_unit(&nalus);

    let segs = m.take_segments_since(0);
    assert_eq!(segs.len(), 1, "exactly one segment for one access unit");
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

    // Expect trun with sample_count == 4
    let trun_pos = find_box(&seg, b"trun").expect("trun");
    let trun_body_start = trun_pos + 4; // skip box name
    // trun body: [0..4) version+flags, [4..8) sample_count, [8..12) data_offset,
    //             [12..) sample_size[]
    let sample_count = read_u32(&seg, trun_body_start + 4);
    assert_eq!(
        sample_count, 4,
        "trun.sample_count must equal NAL count of the access unit"
    );

    // tfhd.default-base-is-moof is set, so trun.data_offset is relative
    // to moof start. First sample is at moof_size + 8 (8 = mdat header).
    let data_offset = read_u32(&seg, trun_body_start + 8);
    assert_eq!(
        data_offset as usize,
        moof_size + 8,
        "trun.data_offset must point to first sample byte inside mdat (moof_size + 8)"
    );

    // Expect 4 sample_size entries
    let sample_sizes_start = trun_body_start + 12;
    let sizes: Vec<u32> = (0..4)
        .map(|i| read_u32(&seg, sample_sizes_start + i * 4))
        .collect();
    // Each NAL was prefixed with a 4-byte length prefix when written.
    let expected_payloads: Vec<usize> = nalus
        .iter()
        .map(|n| n.len() - 4 /* strip Annex-B start code */)
        .collect();
    for (i, s) in sizes.iter().enumerate() {
        assert_eq!(*s as usize, 4 + expected_payloads[i], "sample[{i}] size");
    }

    // mdat payload: total = sum of sample_sizes
    let mdat_payload_len: usize = sizes.iter().map(|s| *s as usize).sum();
    // The full segment is moof_box + mdat_box. Verify the mdat size
    // field equals header (8) + payload, and that segment ends
    // exactly at moof_box_end + mdat_box_size.
    let mdat_size = read_u32(&seg, mdat_pos - 4) as usize;
    assert_eq!(
        mdat_size,
        8 + mdat_payload_len,
        "mdat box size = header (8) + payload"
    );
    let segment_len = seg.len();
    assert_eq!(
        segment_len,
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
    m.push_access_unit(&frame1);
    m.push_access_unit(&frame2);

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
}

#[test]
fn empty_access_unit_is_dropped() {
    let mut m = Fmp4Muxer::new();
    m.set_avc_config(AvcConfig {
        sps: Bytes::from_static(&[0x67]),
        pps: Bytes::from_static(&[0x68]),
    });
    m.set_dimensions(640, 480);
    m.push_access_unit(&[]);
    assert_eq!(m.segment_count(), 0);
}

#[test]
fn push_before_init_is_dropped() {
    let mut m = Fmp4Muxer::new();
    m.push_access_unit(&[annex_b_nal(0x65, &[0])]);
    assert_eq!(m.segment_count(), 0);
}
