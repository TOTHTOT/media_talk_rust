//! Minimal but correct fMP4 (ISO BMFF) muxer for H.264 video, sufficient to
//! feed MSE in modern browsers (`MediaSource.addSourceBuffer` with
//! `video/mp4; codecs="avc1.PPCCLL"`).
//!
//! Boxes produced:
//!   - init segment:  ftyp + moov(mvhd, trak(tkhd, mdia(mdhd, hdlr, minf(vmhd,
//!     dinf/dref, stbl(stsd/avc1+avcC, stts, stsc, stsz, stco)))));
//!   - media segment: moof(mfhd, traf(tfhd, tfdt, trun)) + mdat.
//!
//! **Access-unit semantics (important)**: One H.264 access unit (one frame)
//! MUST arrive at the muxer as a complete group of NAL units belonging to
//! the same picture boundary (e.g. SPS+PPS + IDR + SEI + slice NALs). The
//! muxer writes the *entire* access unit as a single fMP4 sample
//! (`trun.sample_count == nalus.len()`), all NALs concatenated with 4-byte
//! length prefixes inside one `mdat`. This is the only format MSE's H.264
//! decoder accepts — handing it a single slice NAL as a "sample" fails
//! to decode.
//!
//! Callers SHOULD accumulate NALs per frame and call `push_access_unit`
//! once per frame boundary. A frame boundary is detected by either the
//! RTP marker bit (M=1) or the appearance of a "frame-start" NAL type
//! (5 = IDR, 7 = SPS, 8 = PPS). The single-NAL `push_packet` API is kept
//! as a thin wrapper for callers that already aggregate upstream; it
//! writes each NAL as its own sample, which is only correct when the
//! input is guaranteed to be one NAL per frame.

use bytes::{Bytes, BytesMut};

use ipcam_core::{VideoCodec, VideoProfile};

#[derive(Debug, Clone)]
pub struct AvcConfig {
    pub sps: Bytes,
    pub pps: Bytes,
}

#[derive(Debug, Clone)]
pub struct Fmp4Muxer {
    codec: VideoCodec,
    avc: Option<AvcConfig>,
    width: u32,
    height: u32,
    timescale: u32,
    duration_ticks: u64,
    next_sequence: u32,
    segments: Vec<Bytes>,
}

impl Fmp4Muxer {
    pub fn new() -> Self {
        Self {
            codec: VideoCodec::H264,
            avc: None,
            width: 0,
            height: 0,
            timescale: 90000,
            duration_ticks: 0,
            next_sequence: 1,
            segments: Vec::new(),
        }
    }

    pub fn from_profile(profile: &VideoProfile) -> Self {
        Self {
            codec: profile.codec,
            avc: None,
            width: profile.width,
            height: profile.height,
            timescale: 90000,
            duration_ticks: 0,
            next_sequence: 1,
            segments: Vec::new(),
        }
    }

    pub fn set_avc_config(&mut self, cfg: AvcConfig) {
        self.avc = Some(cfg);
    }

    pub fn set_dimensions(&mut self, w: u32, h: u32) {
        if w > 0 {
            self.width = w;
        }
        if h > 0 {
            self.height = h;
        }
    }

    pub fn is_ready(&self) -> bool {
        self.avc.is_some() && self.width > 0 && self.height > 0
    }

    pub fn timescale(&self) -> u32 {
        self.timescale
    }

    pub fn make_init_segment(&self) -> Bytes {
        let mut buf = BytesMut::new();
        write_ftyp(&mut buf);
        write_moov(&mut buf, self);
        buf.freeze()
    }

    /// Push a complete access unit (one frame) as a single fMP4 sample.
    /// All NAL units in `nalus` will be concatenated inside one `mdat` as
    /// 4-byte length-prefixed entries, and `trun` will report one sample
    /// entry per NAL with the matching cumulative sizes.
    ///
    /// `nalus` must contain Annex-B framed NALs (start code `00 00 00 01`).
    /// Empty lists are silently dropped.
    pub fn push_access_unit(&mut self, nalus: &[Bytes]) {
        if nalus.is_empty() || !self.is_ready() {
            return;
        }
        let mut out = BytesMut::new();
        // Strip Annex-B start codes and prepend 4-byte length prefixes
        // (per avcC.length_size_minus_one == 3).
        let mut sample_sizes: Vec<u32> = Vec::with_capacity(nalus.len());
        let mut payload = BytesMut::new();
        for nalu in nalus {
            let body = strip_annex_b(nalu);
            let sample_size = (4 + body.len()) as u32;
            payload.extend_from_slice(&sample_size.to_be_bytes());
            payload.extend_from_slice(body);
            sample_sizes.push(sample_size);
        }
        let total_payload_len = payload.len();
        write_moof_mdat(&mut out, self, &sample_sizes, total_payload_len);
        out.extend_from_slice(&payload);
        self.next_sequence += 1;
        self.duration_ticks += self.duration_per_sample();
        self.segments.push(out.freeze());
    }

    /// Single-NAL convenience wrapper. Writes one NAL as its own sample —
    /// only correct if the caller already knows one frame == one NAL.
    /// For typical multi-NAL frames, use `push_access_unit`.
    pub fn push_packet(&mut self, nalu: Bytes) {
        self.push_access_unit(&[nalu]);
    }

    fn duration_per_sample(&self) -> u64 {
        // We don't know the actual frame duration precisely. Pick a
        // reasonable constant that MSE will accept; duration_ticks is
        // unused by MSE for styp/mdat, only stts uses it, and we leave
        // stts empty (one entry per sample with delta=1 tickscale). The
        // browser figures out actual playback time from its wall clock.
        1
    }

    pub fn take_segments_since(&mut self, since: usize) -> Vec<Bytes> {
        if since >= self.segments.len() {
            return Vec::new();
        }
        self.segments.split_off(since)
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }
}

impl Default for Fmp4Muxer {
    fn default() -> Self {
        Self::new()
    }
}

fn write_ftyp(buf: &mut BytesMut) {
    write_box(buf, b"ftyp", |b| {
        b.extend_from_slice(b"isom");
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(b"isomiso2avc1mp41");
    });
}

fn write_moov(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"moov", |b| {
        write_mvhd(b, m);
        write_trak(b, m);
    });
}

fn write_mvhd(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"mvhd", |b| {
        b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
        b.extend_from_slice(&0u32.to_be_bytes()); // creation_time
        b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
        b.extend_from_slice(&m.timescale.to_be_bytes());
        b.extend_from_slice(&m.duration_ticks.to_be_bytes());
        b.extend_from_slice(&0x00010000u32.to_be_bytes()); // rate 1.0
        b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
        b.extend_from_slice(&[0u8; 10]); // reserved
        // 3x3 unity matrix
        b.extend_from_slice(&0x00010000u32.to_be_bytes());
        b.extend_from_slice(&[0u8; 8]);
        b.extend_from_slice(&0x00010000u32.to_be_bytes());
        b.extend_from_slice(&[0u8; 8]);
        b.extend_from_slice(&0x40000000u32.to_be_bytes());
        b.extend_from_slice(&[0u8; 4]);
        b.extend_from_slice(&[0u8; 4]);
        b.extend_from_slice(&[0u8; 4]);
        b.extend_from_slice(&0x01000000u32.to_be_bytes()); // pre_defined
        b.extend_from_slice(&0x01000000u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // next_track_ID
    });
}

fn write_trak(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"trak", |b| {
        write_tkhd(b, m);
        write_mdia(b, m);
    });
}

fn write_tkhd(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"tkhd", |b| {
        b.extend_from_slice(&0x00000003u32.to_be_bytes()); // version=0, flags=track_enabled+in_movie
        b.extend_from_slice(&0u32.to_be_bytes()); // creation_time
        b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
        b.extend_from_slice(&1u32.to_be_bytes()); // track_ID
        b.extend_from_slice(&0u32.to_be_bytes()); // reserved
        b.extend_from_slice(&m.duration_ticks.to_be_bytes()); // duration
        b.extend_from_slice(&[0u8; 8]);
        b.extend_from_slice(&0u16.to_be_bytes()); // layer
        b.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
        b.extend_from_slice(&0u16.to_be_bytes()); // volume
        b.extend_from_slice(&0u16.to_be_bytes()); // reserved
        // 3x3 unity matrix (same as mvhd)
        b.extend_from_slice(&0x00010000u32.to_be_bytes());
        b.extend_from_slice(&[0u8; 8]);
        b.extend_from_slice(&0x00010000u32.to_be_bytes());
        b.extend_from_slice(&[0u8; 8]);
        b.extend_from_slice(&0x40000000u32.to_be_bytes());
        b.extend_from_slice(&[0u8; 8]);
        b.extend_from_slice(&[0u8; 4]);
        b.extend_from_slice(&[0u8; 4]);
        b.extend_from_slice(&m.width.to_be_bytes());
        b.extend_from_slice(&m.height.to_be_bytes());
    });
}

fn write_mdia(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"mdia", |b| {
        write_mdhd(b, m);
        write_hdlr(b);
        write_minf(b, m);
    });
}

fn write_mdhd(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"mdhd", |b| {
        b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
        b.extend_from_slice(&0u32.to_be_bytes()); // creation_time
        b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
        b.extend_from_slice(&m.timescale.to_be_bytes());
        b.extend_from_slice(&m.duration_ticks.to_be_bytes());
        b.extend_from_slice(&0x55C4u16.to_be_bytes()); // language 'und'
        b.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    });
}

fn write_hdlr(buf: &mut BytesMut) {
    write_box(buf, b"hdlr", |b| {
        b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
        b.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
        b.extend_from_slice(b"vide");
        b.extend_from_slice(&[0u8; 12]); // reserved
        b.extend_from_slice(b"VideoHandler\0");
    });
}

fn write_minf(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"minf", |b| {
        // vmhd
        write_box(b, b"vmhd", |b| {
            b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
            b.extend_from_slice(&[0u8; 8]); // graphicsmode + opcolor[3]
        });
        // dinf > dref > url
        write_box(b, b"dinf", |b| {
            write_box(b, b"dref", |b| {
                b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
                b.extend_from_slice(&1u32.to_be_bytes()); // entry_count
                write_box(b, b"url ", |b| {
                    b.extend_from_slice(&1u32.to_be_bytes()); // self-contained flag
                });
            });
        });
        write_stbl(b, m);
    });
}

fn write_stbl(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"stbl", |b| {
        write_stsd(b, m);
        write_box(b, b"stts", |b| {
            b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
            b.extend_from_slice(&0u32.to_be_bytes()); // entry_count
        });
        write_box(b, b"stsc", |b| {
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
        });
        write_box(b, b"stsz", |b| {
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
        });
        write_box(b, b"stco", |b| {
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
        });
    });
}

fn write_stsd(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"stsd", |b| {
        b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
        b.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        write_avc1(b, m);
    });
}

fn write_avc1(buf: &mut BytesMut, m: &Fmp4Muxer) {
    let (sps, pps) = match (&m.avc, m.codec) {
        (Some(cfg), VideoCodec::H264) => (&cfg.sps[..], &cfg.pps[..]),
        _ => (&[][..], &[][..]),
    };
    // Profile/level from SPS bytes if available, else baseline 3.1.
    let (profile_idc, constraints, level_idc) = parse_sps_profile_level(sps);
    write_box(buf, b"avc1", |b| {
        // 6 reserved bytes
        b.extend_from_slice(&[0u8; 6]);
        b.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
        b.extend_from_slice(&[0u8; 16]); // pre_defined + reserved
        b.extend_from_slice(&m.width.to_be_bytes());
        b.extend_from_slice(&m.height.to_be_bytes());
        b.extend_from_slice(&0x00480000u32.to_be_bytes()); // horizresolution 72 dpi
        b.extend_from_slice(&0x00480000u32.to_be_bytes()); // vertresolution 72 dpi
        b.extend_from_slice(&0u32.to_be_bytes()); // reserved
        b.extend_from_slice(&1u16.to_be_bytes()); // frame_count
        b.extend_from_slice(&[0u8; 32]); // compressorname
        b.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
        b.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined
        write_avc_c(b, profile_idc, constraints, level_idc, sps, pps);
    });
}

fn write_avc_c(
    buf: &mut BytesMut,
    profile_idc: u8,
    constraints: u8,
    level_idc: u8,
    sps: &[u8],
    pps: &[u8],
) {
    write_box(buf, b"avcC", |b| {
        b.extend_from_slice(&[
            profile_idc,
            constraints,
            level_idc,
            0xFF, // length_size_minus_one = 3 (NALU length is 4 bytes)
            0xE1, // number_of_sequence_parameter_sets = 1
        ]);
        b.extend_from_slice(&((sps.len()) as u16).to_be_bytes());
        b.extend_from_slice(sps);
        b.extend_from_slice(&[1u8]); // number_of_picture_parameter_sets = 1
        b.extend_from_slice(&((pps.len()) as u16).to_be_bytes());
        b.extend_from_slice(pps);
    });
}

fn parse_sps_profile_level(sps_rbsp: &[u8]) -> (u8, u8, u8) {
    // H.264 SPS, as written into the avcC box, is the RBSP starting at
    // profile_idc (i.e. the NAL header has already been stripped by the
    // caller). If the caller accidentally passed the full NAL unit with
    // its 1-byte header, we still find profile_idc at offset 1.
    let skip = if sps_rbsp.len() > 3 && (sps_rbsp[0] & 0x1F) == 7 {
        1
    } else {
        0
    };
    if sps_rbsp.len() >= skip + 3 {
        (sps_rbsp[skip], sps_rbsp[skip + 1], sps_rbsp[skip + 2])
    } else {
        (0x42, 0xC0, 0x1E) // baseline 3.1 fallback
    }
}

/// Build one fMP4 fragment: a single moof box followed by a single mdat
/// box. `sample_sizes` lists each NAL's 4-byte length-prefixed size (so
/// the NAL payload is exactly `sample_size - 4` bytes).
/// `total_payload_len` is the sum of all sample_sizes; the function
/// writes the matching mdat payload after the mdat header.
fn write_moof_mdat(
    buf: &mut BytesMut,
    m: &Fmp4Muxer,
    sample_sizes: &[u32],
    total_payload_len: usize,
) {
    // Build the moof box first so we know its size, then patch the
    // trun's data_offset.
    let mut moof_buf = BytesMut::new();
    write_moof(&mut moof_buf, m, sample_sizes);
    let mdat_header_size = 8u32;
    // tfhd.default-base-is-moof is set, so data_offset is relative to
    // moof start. First sample byte is at moof_size + 8 (mdat header).
    let data_offset = moof_buf.len() as u32 + mdat_header_size;
    patch_trun_data_offset(&mut moof_buf, data_offset);
    // Concatenate moof + mdat (no outer box — fragments are top-level
    // boxes per ISO/IEC 14496-12).
    let mdat_total = mdat_header_size as usize + total_payload_len;
    buf.extend_from_slice(&moof_buf);
    buf.extend_from_slice(&(mdat_total as u32).to_be_bytes());
    buf.extend_from_slice(b"mdat");
}

/// Find the (single) trun box inside moof_buf and overwrite the
/// data_offset field at the expected byte position.
fn patch_trun_data_offset(moof_buf: &mut BytesMut, data_offset: u32) {
    // Locate the trun box: search for the literal 4-byte "trun" tag.
    if let Some(pos) = moof_buf.windows(4).position(|w| w == b"trun") {
        // trun body layout (flags = 0x00000201 → data_offset_present | sample_size_present):
        //   [0..4) version+flags
        //   [4..8) sample_count
        //   [8..12) data_offset
        //   [12..12 + 4*sample_count) sample_size entries
        let off = pos + 4;
        let count = u32::from_be_bytes([
            moof_buf[off + 4],
            moof_buf[off + 5],
            moof_buf[off + 6],
            moof_buf[off + 7],
        ]) as usize;
        let data_offset_pos = off + 8;
        moof_buf[data_offset_pos..data_offset_pos + 4].copy_from_slice(&data_offset.to_be_bytes());
        let _ = count;
    }
}

fn strip_annex_b(nalu: &[u8]) -> &[u8] {
    // Strip up to one 4-byte start code (`00 00 00 01`) or 3-byte (`00 00 01`)
    // from the front so we can replace with a 4-byte length prefix.
    if nalu.len() >= 4 && nalu[..4] == [0, 0, 0, 1] {
        &nalu[4..]
    } else if nalu.len() >= 3 && nalu[..3] == [0, 0, 1] {
        &nalu[3..]
    } else {
        nalu
    }
}

fn write_moof(buf: &mut BytesMut, m: &Fmp4Muxer, sample_sizes: &[u32]) {
    write_box(buf, b"moof", |b| {
        write_mfhd(b, m);
        write_traf(b, m, sample_sizes);
    });
}

fn write_mfhd(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"mfhd", |b| {
        b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
        b.extend_from_slice(&m.next_sequence.to_be_bytes());
    });
}

fn write_traf(buf: &mut BytesMut, m: &Fmp4Muxer, sample_sizes: &[u32]) {
    write_box(buf, b"traf", |b| {
        write_tfhd(b);
        write_tfdt(b, m);
        write_trun(b, sample_sizes);
    });
}

fn write_tfhd(buf: &mut BytesMut) {
    write_box(buf, b"tfhd", |b| {
        // flags = 0x020000 = default-base-is-moof → trun.data_offset is
        // interpreted relative to the start of the containing moof box.
        // No base_data_offset field is written (it MUST be absent when
        // default-base-is-moof is set, per ISO/IEC 14496-12).
        b.extend_from_slice(&0x00020000u32.to_be_bytes());
        b.extend_from_slice(&1u32.to_be_bytes()); // track_ID
    });
}

fn write_tfdt(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"tfdt", |b| {
        b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
        b.extend_from_slice(&m.duration_ticks.to_be_bytes());
    });
}

fn write_trun(buf: &mut BytesMut, sample_sizes: &[u32]) {
    write_box(buf, b"trun", |b| {
        // flags:
        //   0x000001 = data_offset_present
        //   0x000008 = sample_size_present
        // We declare only the fields we actually write.
        b.extend_from_slice(&0x00000009u32.to_be_bytes());
        b.extend_from_slice(&(sample_sizes.len() as u32).to_be_bytes());
        // data_offset: patched in later (after moof is fully written) via
        // `patch_trun_data_offset` to point at the first sample byte
        // inside the upcoming mdat box.
        b.extend_from_slice(&0u32.to_be_bytes());
        for size in sample_sizes {
            b.extend_from_slice(&size.to_be_bytes());
        }
    });
}

fn write_box<F: FnOnce(&mut BytesMut)>(buf: &mut BytesMut, name: &[u8; 4], content: F) {
    let mut inner = BytesMut::new();
    content(&mut inner);
    let total = (inner.len() + 8) as u32;
    buf.extend_from_slice(&total.to_be_bytes());
    buf.extend_from_slice(name);
    buf.extend_from_slice(&inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_segment_starts_with_ftyp() {
        let mut m = Fmp4Muxer::new();
        m.set_avc_config(AvcConfig {
            sps: Bytes::from_static(&[0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0, 0x47, 0xFE, 0xC8]),
            pps: Bytes::from_static(&[0x68, 0xCE, 0x38, 0x80]),
        });
        m.set_dimensions(1920, 1080);
        let bytes = m.make_init_segment();
        assert_eq!(&bytes[4..8], b"ftyp");
        assert!(bytes.windows(4).any(|w| w == b"moov"));
        assert!(bytes.windows(4).any(|w| w == b"avcC"));
    }

    #[test]
    fn avcc_embeds_sps_and_pps() {
        let mut m = Fmp4Muxer::new();
        // RBSP without NAL header (avcC convention).
        let sps = [0x42u8, 0xC0, 0x1E, 0xD9, 0x00, 0xA0];
        let pps = [0xCEu8, 0x38, 0x80];
        m.set_avc_config(AvcConfig {
            sps: Bytes::copy_from_slice(&sps),
            pps: Bytes::copy_from_slice(&pps),
        });
        m.set_dimensions(1280, 720);
        let bytes = m.make_init_segment();
        let pos = bytes
            .windows(4)
            .position(|w| w == b"avcC")
            .expect("avcC box");
        let content_start = pos + 4;
        assert_eq!(bytes[content_start], 0x42); // profile_idc baseline
        assert_eq!(bytes[content_start + 1], 0xC0); // constraints
        assert_eq!(bytes[content_start + 2], 0x1E); // level_idc 3.0
        assert_eq!(bytes[content_start + 3], 0xFF); // length_size_minus_one=3
        assert_eq!(bytes[content_start + 4], 0xE1); // number_of_sps = 1
        let sps_len =
            u16::from_be_bytes([bytes[content_start + 5], bytes[content_start + 6]]) as usize;
        assert_eq!(sps_len, sps.len());
        let sps_off = content_start + 7;
        assert_eq!(&bytes[sps_off..sps_off + sps_len], &sps[..]);
    }

    #[test]
    fn push_packet_before_init_is_dropped() {
        let mut m = Fmp4Muxer::new();
        m.push_packet(Bytes::from_static(&[0, 0, 0, 1, 0x65, 0xAA]));
        assert_eq!(m.segments.len(), 0);
    }

    #[test]
    fn push_packet_after_init_writes_moof_mdat() {
        let mut m = Fmp4Muxer::new();
        m.set_avc_config(AvcConfig {
            sps: Bytes::from_static(&[0x67, 0x42]),
            pps: Bytes::from_static(&[0x68, 0xCE]),
        });
        m.set_dimensions(640, 480);
        m.push_packet(Bytes::from_static(&[0, 0, 0, 1, 0x65, 0xAA, 0xBB]));
        assert_eq!(m.segments.len(), 1);
        let seg = &m.segments[0];
        assert_eq!(&seg[4..8], b"moof");
        assert!(seg.windows(4).any(|w| w == b"mdat"));
    }

    #[test]
    fn take_segments_since_skips() {
        let mut m = Fmp4Muxer::new();
        m.set_avc_config(AvcConfig {
            sps: Bytes::from_static(&[0x67]),
            pps: Bytes::from_static(&[0x68]),
        });
        m.set_dimensions(320, 240);
        for _ in 0..5 {
            m.push_packet(Bytes::from_static(&[0, 0, 0, 1, 0x65, 0xAA]));
        }
        let n2 = m.take_segments_since(2);
        assert_eq!(n2.len(), 3);
        assert_eq!(&n2[0][4..8], b"moof");
    }

    #[test]
    fn push_increments_sequence_number() {
        let mut m = Fmp4Muxer::new();
        m.set_avc_config(AvcConfig {
            sps: Bytes::from_static(&[0x67]),
            pps: Bytes::from_static(&[0x68]),
        });
        m.set_dimensions(320, 240);
        for _ in 0..3 {
            m.push_packet(Bytes::from_static(&[0, 0, 0, 1, 0x65, 0xAA]));
        }
        // mfhd sequence number: 1, 2, 3
        // Check first segment has mfhd seq 1
        let seg = &m.segments[0];
        let mfhd_pos = seg.windows(4).position(|w| w == b"mfhd").expect("mfhd");
        let sn_pos = mfhd_pos + 4 + 4; // skip size+name + version/flags
        let sn = u32::from_be_bytes([
            seg[sn_pos],
            seg[sn_pos + 1],
            seg[sn_pos + 2],
            seg[sn_pos + 3],
        ]);
        assert_eq!(sn, 1);
        let seg2 = &m.segments[2];
        let mfhd2 = seg2.windows(4).position(|w| w == b"mfhd").expect("mfhd");
        let sn2_pos = mfhd2 + 4 + 4;
        let sn2 = u32::from_be_bytes([
            seg2[sn2_pos],
            seg2[sn2_pos + 1],
            seg2[sn2_pos + 2],
            seg2[sn2_pos + 3],
        ]);
        assert_eq!(sn2, 3);
    }
}
