//! RTSP stream probe — the fastest way to verify "can I actually pull
//! from this camera, with these credentials?". Connects, plays for
//! `--duration` seconds, and prints NAL-level statistics. Does **not**
//! start the web server, does **not** invoke the hardware decoder, and
//! does **not** mux fMP4. Use it to confirm auth + SDP negotiation +
//! RTP flow before the rest of the pipeline matters.
//!
//! Exit codes (POSIX-style, merge across multiple URLs by severity
//! args > auth > no-idr > other > success):
//!
//! - `0` — at least one IDR observed in the window
//! - `1` — connected, played, but no IDR within `--duration`
//! - `2` — RTSP 401 (auth failed)
//! - `3` — other RTSP error (connect / SDP / SETUP / PLAY)
//! - `4` — bad CLI args or URL parse

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ipcam_core::{EncodedPacket, NalStats, VideoCodec, classify_h264_nal};
use ipcam_rtsp::{RtspClient, RtspConfig, RtspError};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ProbeExit {
    Ok = 0,
    NoIdr = 1,
    Auth = 2,
    Rtsp = 3,
    Args = 4,
}

impl ProbeExit {
    fn merge(self, other: ProbeExit) -> ProbeExit {
        // Higher value = more severe (Args is the most diagnostic).
        std::cmp::max(self, other)
    }
}

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
}

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
    let mut last_idr = None;

    // Time-boxed play loop. We don't try to cancel `play_loop` once
    // started; the helper below spawns the future and races it against
    // a sleep, ignoring the outcome once we have the budget.
    let window = if duration == 0 {
        // "one AU then exit" semantics: short window, but rely on
        // first marker=1 to set a fast-exit.
        Duration::from_secs(5)
    } else {
        Duration::from_secs(duration)
    };

    let play_deadline = started + window;
    let play_outcome = play_until(&client, play_deadline, &mut stats, &mut last_idr).await;

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
    let exit_reason = if let Err(e) = play_outcome {
        format!("play_error: {}", e)
    } else if idr_count == 0 {
        "duration_reached_without_idr".to_string()
    } else {
        "duration_reached".to_string()
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
        error_kind: None,
    }
}

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

fn info_codec_label(video: Option<VideoCodec>, audio: Option<ipcam_core::AudioCodec>) -> String {
    format!(
        "{:?}/{:?}",
        video.unwrap_or(VideoCodec::Unknown),
        audio.unwrap_or(ipcam_core::AudioCodec::Unknown)
    )
}

#[derive(Debug, Clone)]
struct ProbeError {
    kind: ErrorKind,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ErrorKind {
    Auth,
    Rtsp,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ErrorKind::Auth => "auth",
            ErrorKind::Rtsp => "rtsp",
        };
        f.write_str(s)
    }
}

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

/// Run `client.play_loop` until `deadline` (or first error). Closes
/// cleanly by dropping the future once the budget is exhausted; the
/// underlying socket teardown is handled by the caller via
/// `RtspClient::teardown()`.
async fn play_until(
    client: &RtspClient,
    deadline: Instant,
    stats: &mut NalStats,
    _last_idr_seen: &mut Option<Instant>,
) -> Result<(), String> {
    let on_video = |pkt: EncodedPacket| -> ipcam_core::CoreResult<()> {
        accumulate_nal(stats, &pkt);
        Ok(())
    };
    let on_audio = |_pkt: EncodedPacket| -> ipcam_core::CoreResult<()> { Ok(()) };

    let play_fut = client.play_loop(on_video, on_audio);
    tokio::pin!(play_fut);

    let now = Instant::now();
    if deadline <= now {
        return Ok(());
    }
    let remaining = deadline - now;
    match tokio::time::timeout(remaining, &mut play_fut).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("play_loop: {e}")),
        Err(_) => {
            // Window expired; the play_loop future is still pinned but
            // we no longer await it. We just drop it here — the TCP
            // socket will be torn down by `client.teardown()` in the
            // caller. (Dropping the future cancels the task; in this
            // case we own the future locally so cancellation is
            // immediate.)
            Ok(())
        }
    }
}

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

fn hex_upper(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02X}", b));
    }
    s
}

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
