//! RTSP stream probe — the fastest way to verify "can I actually pull
//! from this camera, with these credentials?". Connects, plays for
//! `--duration` seconds, and prints NAL-level statistics. Does **not**
//! start the web server, does **not** invoke the hardware decoder, and
//! does **not** mux fMP4. Use it to confirm auth + SDP negotiation +
//! RTP flow before the rest of the pipeline matters.
//!
//! Media collection goes through `ipcam_gst` (the RTSP handshake for
//! codec discovery still uses `ipcam_rtsp::RtspClient::connect`), so
//! this command requires building with `--features gst`; without it
//! probe prints an error and exits non-zero.
//!
//! Exit codes (POSIX-style, merge across multiple URLs by severity
//! args > auth > no-idr > other > success):
//!
//! - `0` — at least one IDR observed in the window
//! - `1` — connected, played, but no IDR within `--duration`
//! - `2` — RTSP 401 (auth failed)
//! - `3` — other RTSP error (connect / SDP / SETUP / PLAY)
//! - `4` — bad CLI args or URL parse

#[cfg(feature = "gst")]
use std::collections::BTreeMap;
#[cfg(feature = "gst")]
use std::time::{Duration, Instant};

use ipcam_core::{EncodedPacket, NalStats, VideoCodec, classify_h264_nal};
#[cfg(feature = "gst")]
use ipcam_rtsp::{RtspClient, RtspConfig, RtspError};
#[cfg(feature = "gst")]
use serde::Serialize;

// Pure classification logic stays compilable (and testable) without
// the `gst` feature; only the gst-gated collector consumes it.
#[cfg_attr(not(feature = "gst"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ProbeExit {
    Ok = 0,
    NoIdr = 1,
    Auth = 2,
    Rtsp = 3,
    Args = 4,
}

impl ProbeExit {
    #[cfg_attr(not(feature = "gst"), allow(dead_code))]
    fn merge(self, other: ProbeExit) -> ProbeExit {
        // Higher value = more severe (Args is the most diagnostic).
        std::cmp::max(self, other)
    }
}

#[cfg(feature = "gst")]
#[derive(Debug, Serialize)]
struct ProbeReport {
    url: String,
    ok: bool,
    elapsed_ms: u64,
    video_codec: String,
    audio_codec: String,
    access_units: u64,
    idr_count: u64,
    time_to_first_idr_ms: Option<u64>,
    sps_hex: Option<String>,
    pps_hex: Option<String>,
    nal_by_type: BTreeMap<u8, u64>,
    bytes_total: u64,
    bitrate_kbps: u64,
    exit_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_kind: Option<String>,
}

pub async fn run(
    urls: Vec<String>,
    username: Option<String>,
    password: Option<String>,
    duration: u64,
    json: bool,
) -> i32 {
    if urls.is_empty() {
        eprintln!("probe: at least one --rtsp-url is required");
        return ProbeExit::Args as i32;
    }

    #[cfg(feature = "gst")]
    let code = {
        let mut worst = ProbeExit::Ok;
        for url in &urls {
            let report = probe_one(url, username.as_deref(), password.as_deref(), duration).await;
            let code = report_exit_code(&report);
            if json {
                match serde_json::to_string(&report) {
                    Ok(line) => println!("{line}"),
                    Err(e) => eprintln!("probe: json encode failed: {e}"),
                }
            } else {
                print_human(&report);
            }
            worst = worst.merge(code);
        }
        worst as i32
    };

    #[cfg(not(feature = "gst"))]
    let code = {
        let _ = (username, password, duration, json);
        eprintln!("probe: media collection requires building with --features gst");
        ProbeExit::Rtsp as i32
    };

    code
}

#[cfg(feature = "gst")]
fn report_exit_code(r: &ProbeReport) -> ProbeExit {
    if r.ok {
        if r.idr_count > 0 {
            ProbeExit::Ok
        } else {
            ProbeExit::NoIdr
        }
    } else {
        match r.error_kind.as_deref() {
            Some("auth") => ProbeExit::Auth,
            Some("args") => ProbeExit::Args,
            _ => ProbeExit::Rtsp,
        }
    }
}

#[cfg(feature = "gst")]
async fn probe_one(
    url: &str,
    username: Option<&str>,
    password: Option<&str>,
    duration: u64,
) -> ProbeReport {
    let mut cfg = RtspConfig::new(url);
    if let (Some(u), Some(p)) = (username, password) {
        cfg = cfg.with_credentials(u, p);
    }
    let client = RtspClient::new(cfg);

    let started = Instant::now();
    let connect_result = match tokio::time::timeout(Duration::from_secs(15), client.connect()).await
    {
        Ok(Ok(info)) => Ok(info),
        Ok(Err(e)) => Err(translate_error(&e)),
        Err(_) => Err(ProbeError {
            kind: ErrorKind::Rtsp,
            message: "connect timed out (15s)".into(),
        }),
    };
    let info = match connect_result {
        Ok(info) => info,
        Err(err) => {
            return failure_report(url, started.elapsed(), err, info_codec_label(None, None));
        }
    };

    let mut stats = NalStats::default();

    // Time-boxed media collection via ipcam-gst. The session runs on
    // GStreamer threads; the blocking collector drains packets into
    // NalStats until the window expires.
    let window = if duration == 0 {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(duration)
    };

    let play_deadline = started + window;
    let play_outcome = {
        let url_owned = url.to_string();
        let user = username.map(str::to_string);
        let pass = password.map(str::to_string);
        let mut collected = std::mem::take(&mut stats);
        let join = tokio::task::spawn_blocking(move || {
            let outcome = play_collect(
                &url_owned,
                user.as_deref(),
                pass.as_deref(),
                play_deadline,
                &mut collected,
            );
            (outcome, collected)
        })
        .await;
        match join {
            Ok((outcome, collected)) => {
                stats = collected;
                outcome
            }
            Err(e) => Err(format!("play task join: {e}")),
        }
    };

    if let Err(e) = client.teardown().await {
        tracing::debug!(err = %e, "teardown error (non-fatal)");
    }

    let elapsed = started.elapsed();
    let bitrate_kbps = if elapsed.as_secs() > 0 {
        stats.bytes_total * 8 / elapsed.as_secs() / 1000
    } else {
        0
    };
    let idr_count = stats.idr_count;
    let ok = play_outcome.is_ok() && idr_count > 0;
    let (exit_reason, error_kind) = match &play_outcome {
        Err(e) if is_auth_error(e) => (format!("play_error: {e}"), Some(ErrorKind::Auth)),
        Err(e) => (format!("play_error: {e}"), None),
        Ok(_) if idr_count == 0 => ("duration_reached_without_idr".to_string(), None),
        Ok(_) => ("duration_reached".to_string(), None),
    };

    ProbeReport {
        url: url.to_string(),
        ok,
        elapsed_ms: elapsed.as_millis() as u64,
        video_codec: format!("{:?}", info.video_codec),
        audio_codec: format!("{:?}", info.audio_codec),
        access_units: stats.access_units,
        idr_count,
        time_to_first_idr_ms: stats.time_to_first_idr.map(|d| d.as_millis() as u64),
        sps_hex: stats.sps.as_deref().map(hex_upper),
        pps_hex: stats.pps.as_deref().map(hex_upper),
        nal_by_type: stats.nal_by_type,
        bytes_total: stats.bytes_total,
        bitrate_kbps,
        exit_reason,
        error_kind: error_kind.map(|k| k.to_string()),
    }
}

#[cfg(feature = "gst")]
fn failure_report(
    url: &str,
    elapsed: Duration,
    err: ProbeError,
    video_codec: String,
) -> ProbeReport {
    ProbeReport {
        url: url.to_string(),
        ok: false,
        elapsed_ms: elapsed.as_millis() as u64,
        video_codec,
        audio_codec: "Unknown".to_string(),
        access_units: 0,
        idr_count: 0,
        time_to_first_idr_ms: None,
        sps_hex: None,
        pps_hex: None,
        nal_by_type: BTreeMap::new(),
        bytes_total: 0,
        bitrate_kbps: 0,
        exit_reason: err.message.clone(),
        error_kind: Some(err.kind.to_string()),
    }
}

#[cfg(feature = "gst")]
fn info_codec_label(video: Option<VideoCodec>, audio: Option<ipcam_core::AudioCodec>) -> String {
    format!(
        "{:?}/{:?}",
        video.unwrap_or(VideoCodec::Unknown),
        audio.unwrap_or(ipcam_core::AudioCodec::Unknown)
    )
}

#[cfg(feature = "gst")]
#[derive(Debug, Clone)]
struct ProbeError {
    kind: ErrorKind,
    message: String,
}

#[cfg(feature = "gst")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ErrorKind {
    Auth,
    Rtsp,
}

#[cfg(feature = "gst")]
impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ErrorKind::Auth => "auth",
            ErrorKind::Rtsp => "rtsp",
        };
        f.write_str(s)
    }
}

#[cfg(feature = "gst")]
fn translate_error(e: &RtspError) -> ProbeError {
    match e {
        RtspError::Status { status, body } if *status == 401 => ProbeError {
            kind: ErrorKind::Auth,
            message: format!("RTSP 401 Unauthorized (body {} bytes)", body.len()),
        },
        RtspError::Status { status, body } => ProbeError {
            kind: ErrorKind::Rtsp,
            message: format!("RTSP {status}: {} bytes", body.len()),
        },
        RtspError::Parse(s) => ProbeError {
            kind: ErrorKind::Rtsp,
            message: format!("rtsp parse: {s}"),
        },
        RtspError::Timeout => ProbeError {
            kind: ErrorKind::Rtsp,
            message: "rtsp timeout".into(),
        },
        RtspError::TransportClosed => ProbeError {
            kind: ErrorKind::Rtsp,
            message: "transport closed".into(),
        },
        RtspError::Io(io) => ProbeError {
            kind: ErrorKind::Rtsp,
            message: format!("io: {io}"),
        },
        RtspError::CodecUnsupported(c) => ProbeError {
            kind: ErrorKind::Rtsp,
            message: format!("codec unsupported: {c}"),
        },
        RtspError::Runtime(s) => ProbeError {
            kind: ErrorKind::Rtsp,
            message: format!("rtsp-runtime: {s}"),
        },
    }
}

/// Collect `EncodedPacket`s via `ipcam_gst::start` until `deadline`
/// (or a terminal stream failure). Runs on a blocking thread: the
/// appsink callbacks push packets into a channel, this loop drains it
/// into `NalStats` and stops the session before returning.
#[cfg(feature = "gst")]
fn play_collect(
    url: &str,
    username: Option<&str>,
    password: Option<&str>,
    deadline: Instant,
    stats: &mut NalStats,
) -> Result<(), String> {
    let mut cfg = ipcam_gst::GstStreamConfig {
        uri: url.to_string(),
        ..Default::default()
    };
    if let (Some(u), Some(p)) = (username, password) {
        cfg.credentials = Some((u.to_string(), p.to_string()));
    }

    let (tx, rx) = std::sync::mpsc::channel::<EncodedPacket>();
    let on_video = move |pkt: EncodedPacket| {
        let _ = tx.send(pkt);
    };
    let on_audio = |_pkt: ipcam_gst::AudioPacket| {};

    let handle =
        ipcam_gst::start(cfg, on_video, on_audio).map_err(|e| format!("gst start: {e}"))?;
    let outcome = loop {
        let now = Instant::now();
        if deadline <= now {
            break Ok(());
        }
        match rx.recv_timeout(deadline - now) {
            Ok(pkt) => accumulate_nal(stats, &pkt),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
        }
        // Surface terminal failures (incl. 401 auth errors from the bus)
        // instead of silently running the window out.
        if handle.state() == ipcam_gst::StreamState::Failed {
            let err = handle
                .stats()
                .last_error
                .unwrap_or_else(|| "stream failed".into());
            break Err(err);
        }
    };
    handle.stop();
    outcome
}

/// 401 classification for gst play-phase errors: the bus error text
/// keeps the Unauthorized wording (contract requirement).
#[cfg(feature = "gst")]
fn is_auth_error(msg: &str) -> bool {
    msg.contains("401") || msg.contains("Unauthorized") || msg.contains("Not Authorized")
}

#[cfg_attr(not(feature = "gst"), allow(dead_code))]
pub(crate) fn accumulate_nal(stats: &mut NalStats, pkt: &EncodedPacket) {
    if pkt.codec != VideoCodec::H264 {
        return;
    }
    let nal_type = match classify_h264_nal(&pkt.data) {
        Some(t) => t,
        None => return,
    };
    *stats.nal_by_type.entry(nal_type).or_insert(0) += 1;
    // The Annex-B start code is 4 bytes; the header byte is index 4
    // (which `classify_h264_nal` already validated). RBSP = data[5..].
    if pkt.data.len() > 5 {
        stats.bytes_total += (pkt.data.len() - 5) as u64;
    }
    if pkt.marker {
        stats.access_units += 1;
    }
    if nal_type == 5 {
        stats.idr_count += 1;
        if stats.time_to_first_idr.is_none() {
            stats.time_to_first_idr = Some(stats.elapsed);
        }
    } else if nal_type == 7 && stats.sps.is_none() {
        // Save the RBSP only if the packet actually carries payload
        // beyond the 4-byte start code + 1-byte header. `pkt.data` is
        // guaranteed to be >= 5 bytes by `classify_h264_nal` above.
        stats.sps = Some(pkt.data[5..].to_vec());
    } else if nal_type == 8 && stats.pps.is_none() {
        stats.pps = Some(pkt.data[5..].to_vec());
    }
}

#[cfg(feature = "gst")]
fn hex_upper(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02X}", b));
    }
    s
}

#[cfg(feature = "gst")]
fn print_human(r: &ProbeReport) {
    println!("=== {} ===", r.url);
    println!("  elapsed:        {} ms", r.elapsed_ms);
    println!("  video/audio:    {} / {}", r.video_codec, r.audio_codec);
    println!("  access_units:   {}", r.access_units);
    println!("  idr_count:      {}", r.idr_count);
    if let Some(t) = r.time_to_first_idr_ms {
        println!("  first IDR at:   {} ms", t);
    } else {
        println!("  first IDR at:   (none in window)");
    }
    match (&r.sps_hex, &r.pps_hex) {
        (Some(s), Some(p)) => {
            println!("  SPS:            {}", s);
            println!("  PPS:            {}", p);
        }
        _ => println!("  SPS/PPS:        (incomplete)"),
    }
    if r.nal_by_type.is_empty() {
        println!("  NAL 分布:        (none)");
    } else {
        print!("  NAL 分布:        ");
        let mut first = true;
        for (t, c) in &r.nal_by_type {
            if !first {
                print!("  ");
            }
            print!("{}: {}", t, c);
            first = false;
        }
        println!();
    }
    println!(
        "  bytes/bitrate:  {} bytes / ~{} kbps",
        r.bytes_total, r.bitrate_kbps
    );
    let status = if r.ok {
        "OK"
    } else if r.idr_count == 0 && r.error_kind.is_none() {
        "WARN"
    } else {
        "FAIL"
    };
    println!("  exit:           {} — {}", status, r.exit_reason);
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn nalu(nal_byte: u8) -> EncodedPacket {
        let mut data = vec![0u8; 5];
        data[4] = nal_byte;
        EncodedPacket {
            codec: VideoCodec::H264,
            data: Bytes::from(data),
            rtp_ts: 0,
            arrival_us: 0,
            is_keyframe: nal_byte == 5,
            marker: false,
        }
    }

    fn marker_pkt(nal_byte: u8) -> EncodedPacket {
        let mut p = nalu(nal_byte);
        p.marker = true;
        p
    }

    #[test]
    fn accumulate_empty_input() {
        let s = NalStats::default();
        assert_eq!(s.idr_count, 0);
        assert_eq!(s.access_units, 0);
        assert!(s.sps.is_none());
        assert!(s.pps.is_none());
    }

    #[test]
    fn accumulate_only_sps_no_idr() {
        let mut s = NalStats::default();
        accumulate_nal(&mut s, &nalu(7));
        accumulate_nal(&mut s, &nalu(8));
        assert_eq!(*s.nal_by_type.get(&7).unwrap(), 1);
        assert_eq!(*s.nal_by_type.get(&8).unwrap(), 1);
        assert!(s.sps.is_some());
        assert!(s.pps.is_some());
        assert_eq!(s.idr_count, 0);
    }

    #[test]
    fn accumulate_mixed_idr_and_p() {
        let mut s = NalStats::default();
        // IDR + P + IDR + P
        accumulate_nal(&mut s, &marker_pkt(5));
        accumulate_nal(&mut s, &marker_pkt(1));
        accumulate_nal(&mut s, &marker_pkt(5));
        accumulate_nal(&mut s, &marker_pkt(1));
        assert_eq!(s.idr_count, 2);
        assert_eq!(s.access_units, 4);
        assert_eq!(*s.nal_by_type.get(&5).unwrap(), 2);
        assert_eq!(*s.nal_by_type.get(&1).unwrap(), 2);
    }

    #[test]
    fn merge_picks_most_severe() {
        assert_eq!(ProbeExit::Ok.merge(ProbeExit::Auth), ProbeExit::Auth);
        assert_eq!(ProbeExit::NoIdr.merge(ProbeExit::Args), ProbeExit::Args);
        assert_eq!(ProbeExit::Auth.merge(ProbeExit::Ok), ProbeExit::Auth);
    }
}
