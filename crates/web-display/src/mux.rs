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
//! muxer writes the *entire* access unit as ONE fMP4 sample: all NALs
//! concatenated with 4-byte length prefixes inside one `mdat`, referenced
//! by a single trun entry (`trun.sample_count == 1`). This is the only
//! format MSE's H.264 decoder accepts — handing it a single slice NAL as
//! a "sample" fails to decode.
//!
//! **Timestamps**: each access unit carries the RTP timestamp (90 kHz) of
//! its first packet. The muxer holds each AU as *pending* until the next
//! one arrives, so the sample gets its REAL duration (the inter-frame RTP
//! delta) and `tfdt` gets the real decode time relative to the first AU.
//! That is a deliberate one-frame delay (~90 ms at 11 fps) in exchange for
//! a correct MSE timeline — feeding a constant 1-tick duration makes the
//! browser "play" hours of video in milliseconds and never render.
//!
//! Callers SHOULD accumulate NALs per frame and call `push_access_unit`
//! once per frame boundary. A frame boundary is detected by either the
//! RTP marker bit (M=1) or the appearance of a "frame-start" NAL type
//! (5 = IDR, 7 = SPS, 8 = PPS). The single-NAL `push_packet` API is kept
//! as a thin wrapper for callers that already aggregate upstream.

use bytes::{Bytes, BytesMut};

use ipcam_core::{VideoCodec, VideoProfile};

#[derive(Debug, Clone)]
pub struct AvcConfig {
    pub sps: Bytes,
    pub pps: Bytes,
}

/// A completed access unit waiting for the NEXT one's RTP timestamp, so
/// its true sample duration can be written into the trun box.
#[derive(Debug, Clone)]
struct PendingAu {
    rtp_ts: u32,
    /// All NALs of the frame, each prefixed with its 4-byte length
    /// (the avcC / ISO-IEC 14496-15 in-sample format).
    payload: BytesMut,
}

#[derive(Debug, Clone)]
pub struct Fmp4Muxer {
    codec: VideoCodec,
    avc: Option<AvcConfig>,
    width: u32,
    height: u32,
    timescale: u32,
    /// RTP timestamp of the first access unit ever pushed; tfdt values
    /// are relative to it so the MSE timeline starts at zero.
    first_ts: Option<u32>,
    pending: Option<PendingAu>,
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
            first_ts: None,
            pending: None,
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
            first_ts: None,
            pending: None,
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
    /// All NAL units in `nalus` are concatenated inside one `mdat` as
    /// 4-byte length-prefixed entries, referenced by a single trun
    /// sample entry.
    ///
    /// `nalus` must contain Annex-B framed NALs (start code `00 00 00 01`).
    /// `rtp_ts` is the RTP timestamp (90 kHz) of the frame's first packet.
    /// Empty lists are silently dropped.
    ///
    /// The AU is NOT emitted immediately: it becomes a segment only when
    /// the NEXT access unit arrives, because the real sample duration is
    /// the RTP timestamp delta between consecutive frames.
    pub fn push_access_unit(&mut self, nalus: &[Bytes], rtp_ts: u32) {
        if nalus.is_empty() || !self.is_ready() {
            return;
        }
        // Strip Annex-B start codes and prepend 4-byte length prefixes
        // (per avcC.length_size_minus_one == 3). The in-sample length
        // prefix counts only the NAL bytes that follow it, NOT the 4
        // prefix bytes themselves (ISO/IEC 14496-15).
        let mut payload = BytesMut::new();
        for nalu in nalus {
            let body = strip_annex_b(nalu);
            payload.extend_from_slice(&(body.len() as u32).to_be_bytes());
            payload.extend_from_slice(body);
        }
        let first_ts = *self.first_ts.get_or_insert(rtp_ts);
        if let Some(prev) = self.pending.take() {
            // Real duration = RTP delta to this frame. Guard against a
            // camera sending duplicate timestamps — zero-duration samples
            // confuse MSE's buffered-range bookkeeping.
            let duration = rtp_ts.wrapping_sub(prev.rtp_ts).max(1);
            let base_ts = prev.rtp_ts.wrapping_sub(first_ts);
            let sample_size = prev.payload.len() as u32;
            let mut out = BytesMut::new();
            write_moof_mdat(&mut out, self, sample_size, duration, base_ts);
            out.extend_from_slice(&prev.payload);
            self.next_sequence += 1;
            self.segments.push(out.freeze());
        }
        self.pending = Some(PendingAu { rtp_ts, payload });
    }

    /// Single-NAL convenience wrapper — only correct if the caller
    /// already knows one frame == one NAL. Carries no real timestamp, so
    /// every sample gets the 1-tick fallback duration; for typical
    /// multi-NAL frames use `push_access_unit`.
    pub fn push_packet(&mut self, nalu: Bytes) {
        self.push_access_unit(&[nalu], 0);
    }

    /// Take all segments from index `since` onward, REMOVING them from
    /// the internal buffer (split_off). Callers must treat the returned
    /// segments as consumed: the next call sees a buffer that starts
    /// again at index 0.
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
        write_mvex(b);
    });
}

/// mvex/trex is mandatory for fragmented MP4: it is what associates
/// moof fragments (tfhd track_ID) with the track declared in trak, and
/// supplies the default sample metadata when trun omits those fields.
/// Without it both ffmpeg and browser MSE reject every fragment.
fn write_mvex(buf: &mut BytesMut) {
    write_box(buf, b"mvex", |b| {
        write_box(b, b"trex", |b| {
            b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
            b.extend_from_slice(&1u32.to_be_bytes()); // track_ID
            b.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
            b.extend_from_slice(&0u32.to_be_bytes()); // default_sample_duration
            b.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size
            b.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags
        });
    });
}

fn write_mvhd(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"mvhd", |b| {
        b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
        b.extend_from_slice(&0u32.to_be_bytes()); // creation_time
        b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
        b.extend_from_slice(&m.timescale.to_be_bytes());
        // fMP4 movie duration is unknown at init time; version-0 mvhd
        // carries a u32, and 0 is the conventional "no duration" value.
        b.extend_from_slice(&0u32.to_be_bytes()); // duration
        b.extend_from_slice(&0x00010000u32.to_be_bytes()); // rate 1.0
        b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
        b.extend_from_slice(&[0u8; 10]); // reserved
        write_unity_matrix(b);
        b.extend_from_slice(&[0u8; 24]); // pre_defined[6]
        b.extend_from_slice(&2u32.to_be_bytes()); // next_track_ID
    });
}

/// ISO/IEC 14496-12 3x3 unity matrix: 9 u32 values
/// `{0x10000,0,0, 0,0x10000,0, 0,0,0x40000000}` (16.16 / 2.30 fixed).
fn write_unity_matrix(b: &mut BytesMut) {
    for v in [
        0x00010000u32,
        0,
        0, //
        0,
        0x00010000,
        0, //
        0,
        0,
        0x40000000,
    ] {
        b.extend_from_slice(&v.to_be_bytes());
    }
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
        b.extend_from_slice(&0u32.to_be_bytes()); // duration (unknown for fMP4)
        b.extend_from_slice(&[0u8; 8]);
        b.extend_from_slice(&0u16.to_be_bytes()); // layer
        b.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
        b.extend_from_slice(&0u16.to_be_bytes()); // volume
        b.extend_from_slice(&0u16.to_be_bytes()); // reserved
        write_unity_matrix(b);
        // tkhd width/height are 16.16 fixed-point (unlike the plain u16
        // in the avc1 sample entry).
        b.extend_from_slice(&(m.width << 16).to_be_bytes());
        b.extend_from_slice(&(m.height << 16).to_be_bytes());
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
        b.extend_from_slice(&0u32.to_be_bytes()); // duration (unknown for fMP4)
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
        // vmhd is a FullBox whose flags MUST be 1 (ISO/IEC 14496-12 §12.1.2)
        write_box(b, b"vmhd", |b| {
            b.extend_from_slice(&1u32.to_be_bytes()); // version + flags=1
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
        // VisualSampleEntry width/height are u16 (unlike tkhd's 16.16 fixed)
        b.extend_from_slice(&m.width.to_be_bytes()[2..]);
        b.extend_from_slice(&m.height.to_be_bytes()[2..]);
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
            0x01, // configurationVersion
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
    // H.264 SPS, as written into the avcC box, is the complete NAL unit
    // including its 1-byte NAL header (ISO/IEC 14496-15); profile_idc is
    // then at offset 1. A header-less RBSP is also tolerated.
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

/// Parse (width, height) from an H.264 SPS. Accepts RBSP with or without
/// the 1-byte NAL header; emulation-prevention bytes (`00 00 03`) are
/// stripped first. Needed because discovered profiles carry no
/// width/height (the ONVIF backend skips GetVideoEncoderConfiguration),
/// so the muxer learns dimensions from the bitstream itself.
pub fn parse_sps_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    let nal = if !sps.is_empty() && (sps[0] & 0x1F) == 7 {
        &sps[1..]
    } else {
        sps
    };
    let rbsp = strip_emulation_prevention(nal);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.u8()?;
    let _constraint_flags = r.u8()?;
    let _level_idc = r.u8()?;
    r.ue()?; // seq_parameter_set_id

    let mut chroma_format_idc = 1u32; // 4:2:0 default
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.u1()?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.u1()?; // qpprime_y_zero_transform_bypass_flag
        if r.u1()? {
            // seq_scaling_matrix_present_flag
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                if r.u1()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    r.ue()?; // log2_max_frame_num_minus4
    let poc_type = r.ue()?;
    if poc_type == 0 {
        r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if poc_type == 1 {
        r.u1()?; // delta_pic_order_always_zero_flag
        r.se()?; // offset_for_non_ref_pic
        r.se()?; // offset_for_top_to_bottom_field
        let n = r.ue()?;
        for _ in 0..n {
            r.se()?; // offset_for_ref_frame[i]
        }
    }
    r.ue()?; // max_num_ref_frames
    r.u1()?; // gaps_in_frame_num_value_allowed_flag
    let pic_width_in_mbs_minus1 = r.ue()?;
    let pic_height_in_map_units_minus1 = r.ue()?;
    let frame_mbs_only = r.u1()?;
    if !frame_mbs_only {
        r.u1()?; // mb_adaptive_frame_field_flag
    }
    r.u1()?; // direct_8x8_inference_flag
    let (mut crop_l, mut crop_r, mut crop_t, mut crop_b) = (0, 0, 0, 0);
    if r.u1()? {
        // frame_cropping_flag
        crop_l = r.ue()?;
        crop_r = r.ue()?;
        crop_t = r.ue()?;
        crop_b = r.ue()?;
    }

    // Crop units (H.264 Table 7-1): sub-width/height depend on chroma
    // format; monochrome or separate-plane streams crop in whole units.
    let (sub_w, sub_h) = match (chroma_format_idc, separate_colour_plane) {
        (0, _) | (_, true) => (1, 2 - frame_mbs_only as u32),
        (1, false) => (2, 2 * (2 - frame_mbs_only as u32)),
        (2, false) | (3, false) => (4 - chroma_format_idc, 2 - frame_mbs_only as u32),
        _ => (1, 1),
    };
    let width = (pic_width_in_mbs_minus1 + 1) * 16 - (crop_l + crop_r) * sub_w;
    let height = (pic_height_in_map_units_minus1 + 1) * 16 * (2 - frame_mbs_only as u32)
        - (crop_t + crop_b) * sub_h;
    (width > 0 && height > 0).then_some((width, height))
}

/// Remove H.264 emulation-prevention bytes (`00 00 03` -> `00 00`).
fn strip_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Skip a scaling_list(i) payload (H.264 7.3.2.1.1).
fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = r.se()?;
            next_scale = (last_scale + delta + 256) % 256;
        }
        last_scale = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        };
    }
    Some(())
}

/// Minimal MSB-first bit reader with Exp-Golomb support.
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn u1(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.bit / 8)?;
        let v = (byte >> (7 - self.bit % 8)) & 1 == 1;
        self.bit += 1;
        Some(v)
    }

    fn u8(&mut self) -> Option<u8> {
        let mut v = 0u8;
        for _ in 0..8 {
            v = (v << 1) | self.u1()? as u8;
        }
        Some(v)
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while !self.u1()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut suffix = 0u32;
        for _ in 0..zeros {
            suffix = (suffix << 1) | self.u1()? as u32;
        }
        Some((1 << zeros) - 1 + suffix)
    }

    /// Signed Exp-Golomb.
    fn se(&mut self) -> Option<i32> {
        let v = self.ue()? as i32;
        Some(if v % 2 == 0 { -(v / 2) } else { (v + 1) / 2 })
    }
}

/// Build one fMP4 fragment: a single moof box followed by a single mdat
/// box holding exactly ONE sample (a whole access unit of `sample_size`
/// bytes, duration `sample_duration` ticks, decode time `base_ts`).
/// The caller appends the mdat payload itself after this returns.
fn write_moof_mdat(
    buf: &mut BytesMut,
    m: &Fmp4Muxer,
    sample_size: u32,
    sample_duration: u32,
    base_ts: u32,
) {
    // Build the moof box first so we know its size, then patch the
    // trun's data_offset.
    let mut moof_buf = BytesMut::new();
    write_moof(&mut moof_buf, m, sample_size, sample_duration, base_ts);
    let mdat_header_size = 8u32;
    // tfhd.default-base-is-moof is set, so data_offset is relative to
    // moof start. First (only) sample byte is at moof_size + 8.
    let data_offset = moof_buf.len() as u32 + mdat_header_size;
    patch_trun_data_offset(&mut moof_buf, data_offset);
    // Concatenate moof + mdat (no outer box — fragments are top-level
    // boxes per ISO/IEC 14496-12).
    let mdat_total = mdat_header_size + sample_size;
    buf.extend_from_slice(&moof_buf);
    buf.extend_from_slice(&mdat_total.to_be_bytes());
    buf.extend_from_slice(b"mdat");
}

/// Find the (single) trun box inside moof_buf and overwrite the
/// data_offset field at the expected byte position.
fn patch_trun_data_offset(moof_buf: &mut BytesMut, data_offset: u32) {
    // trun body layout (flags = 0x00000301 → data_offset | sample_duration
    // | sample_size present):
    //   [0..4)  version+flags
    //   [4..8)  sample_count
    //   [8..12) data_offset      <- patched here
    //   [12..16) sample_duration
    //   [16..20) sample_size
    if let Some(pos) = moof_buf.windows(4).position(|w| w == b"trun") {
        let off = pos + 4 + 8;
        moof_buf[off..off + 4].copy_from_slice(&data_offset.to_be_bytes());
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

fn write_moof(
    buf: &mut BytesMut,
    m: &Fmp4Muxer,
    sample_size: u32,
    sample_duration: u32,
    base_ts: u32,
) {
    write_box(buf, b"moof", |b| {
        write_mfhd(b, m);
        write_traf(b, sample_size, sample_duration, base_ts);
    });
}

fn write_mfhd(buf: &mut BytesMut, m: &Fmp4Muxer) {
    write_box(buf, b"mfhd", |b| {
        b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
        b.extend_from_slice(&m.next_sequence.to_be_bytes());
    });
}

fn write_traf(buf: &mut BytesMut, sample_size: u32, sample_duration: u32, base_ts: u32) {
    write_box(buf, b"traf", |b| {
        write_tfhd(b);
        write_tfdt(b, base_ts);
        write_trun(b, sample_size, sample_duration);
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

fn write_tfdt(buf: &mut BytesMut, base_ts: u32) {
    write_box(buf, b"tfdt", |b| {
        b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
        // version-0 tfdt carries a u32 baseMediaDecodeTime: the RTP
        // timestamp of this fragment's sample relative to the first AU,
        // so the MSE timeline starts at zero. At 90 kHz a u32 wraps
        // after ~13 hours of continuous streaming.
        b.extend_from_slice(&base_ts.to_be_bytes());
    });
}

fn write_trun(buf: &mut BytesMut, sample_size: u32, sample_duration: u32) {
    write_box(buf, b"trun", |b| {
        // flags (ISO/IEC 14496-12 §8.8.8):
        //   0x000001 = data_offset_present
        //   0x000100 = sample_duration_present
        //   0x000200 = sample_size_present
        // We declare only the fields we actually write.
        b.extend_from_slice(&0x00000301u32.to_be_bytes());
        // One sample = one whole access unit (all its NALs concatenated
        // with 4-byte length prefixes).
        b.extend_from_slice(&1u32.to_be_bytes());
        // data_offset: patched in later (after moof is fully written) via
        // `patch_trun_data_offset` to point at the first sample byte
        // inside the upcoming mdat box.
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&sample_duration.to_be_bytes());
        b.extend_from_slice(&sample_size.to_be_bytes());
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
    fn sps_dimensions_real_camera_rbsp() {
        // Real SPS from the office LIVE555 camera (1280x720 baseline).
        // RBSP without NAL header (stream.rs stores SPS with the header,
        // per the avcC convention; this variant must also parse).
        let rbsp = [0x42, 0x00, 0x1f, 0xe5, 0x40, 0x28, 0x02, 0xdc, 0x80];
        assert_eq!(parse_sps_dimensions(&rbsp), Some((1280, 720)));
        // same bytes with the NAL header byte still attached
        let with_header = [0x67, 0x42, 0x00, 0x1f, 0xe5, 0x40, 0x28, 0x02, 0xdc, 0x80];
        assert_eq!(parse_sps_dimensions(&with_header), Some((1280, 720)));
        // garbage must not panic
        assert_eq!(parse_sps_dimensions(&[]), None);
        assert_eq!(parse_sps_dimensions(&[0x67]), None);
    }

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
        // Complete NAL units with header byte (avcC convention,
        // ISO/IEC 14496-15).
        let sps = [0x67u8, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0];
        let pps = [0x68u8, 0xCE, 0x38, 0x80];
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
        assert_eq!(bytes[content_start], 0x01); // configurationVersion
        assert_eq!(bytes[content_start + 1], 0x42); // profile_idc baseline
        assert_eq!(bytes[content_start + 2], 0xC0); // constraints
        assert_eq!(bytes[content_start + 3], 0x1E); // level_idc 3.0
        assert_eq!(bytes[content_start + 4], 0xFF); // length_size_minus_one=3
        assert_eq!(bytes[content_start + 5], 0xE1); // number_of_sps = 1
        let sps_len =
            u16::from_be_bytes([bytes[content_start + 6], bytes[content_start + 7]]) as usize;
        assert_eq!(sps_len, sps.len());
        let sps_off = content_start + 8;
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
        // The first AU only fills the pending slot — a segment needs the
        // NEXT frame's timestamp to compute this one's duration.
        m.push_packet(Bytes::from_static(&[0, 0, 0, 1, 0x65, 0xAA, 0xBB]));
        assert_eq!(m.segments.len(), 0);
        m.push_packet(Bytes::from_static(&[0, 0, 0, 1, 0x41, 0xCC]));
        assert_eq!(m.segments.len(), 1);
        let seg = &m.segments[0];
        assert_eq!(&seg[4..8], b"moof");
        assert!(seg.windows(4).any(|w| w == b"mdat"));
    }

    #[test]
    fn segments_carry_real_rtp_durations() {
        let mut m = Fmp4Muxer::new();
        m.set_avc_config(AvcConfig {
            sps: Bytes::from_static(&[0x67]),
            pps: Bytes::from_static(&[0x68]),
        });
        m.set_dimensions(320, 240);
        let au = || vec![Bytes::from_static(&[0, 0, 0, 1, 0x65, 0xAA])];
        m.push_access_unit(&au(), 1000);
        m.push_access_unit(&au(), 1000 + 9000);
        m.push_access_unit(&au(), 1000 + 9000 + 18000);
        assert_eq!(m.segments.len(), 2, "last AU stays pending");

        // trun body (flags 0x301): [0..4) version+flags, [4..8) count,
        // [8..12) data_offset, [12..16) sample_duration, [16..20) size
        let trun_duration = |seg: &Bytes| {
            let p = seg.windows(4).position(|w| w == b"trun").unwrap() + 4;
            u32::from_be_bytes([seg[p + 12], seg[p + 13], seg[p + 14], seg[p + 15]])
        };
        // tfdt body: [0..4) version+flags, [4..8) baseMediaDecodeTime
        let tfdt_base = |seg: &Bytes| {
            let p = seg.windows(4).position(|w| w == b"tfdt").unwrap() + 4;
            u32::from_be_bytes([seg[p + 4], seg[p + 5], seg[p + 6], seg[p + 7]])
        };
        assert_eq!(trun_duration(&m.segments[0]), 9000);
        assert_eq!(
            tfdt_base(&m.segments[0]),
            0,
            "timeline starts at the first AU"
        );
        assert_eq!(trun_duration(&m.segments[1]), 18000);
        assert_eq!(tfdt_base(&m.segments[1]), 9000);
    }

    #[test]
    fn take_segments_since_skips() {
        let mut m = Fmp4Muxer::new();
        m.set_avc_config(AvcConfig {
            sps: Bytes::from_static(&[0x67]),
            pps: Bytes::from_static(&[0x68]),
        });
        m.set_dimensions(320, 240);
        // 6 pushes → 5 segments (one frame always stays pending).
        for _ in 0..6 {
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
        // 4 pushes → 3 segments with mfhd sequence numbers 1, 2, 3.
        for _ in 0..4 {
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
