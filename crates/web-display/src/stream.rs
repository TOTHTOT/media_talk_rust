//! Per-session RTSP streaming pipeline.
//!
//! When a session is created via `POST /api/sessions`, this task:
//!   1. Calls ONVIF `GetStreamUri` (with the user-supplied credentials
//!      held by the registry) to obtain an RTSP URL.
//!   2. Starts an `ipcam_gst` session (rtspsrc → depay/parse → appsink)
//!      which pushes each Annex-B H.264 NAL
//!      into the per-session fMP4 muxer.
//!   3. Captures SPS/PPS from the first packets so the muxer can emit a
//!      proper avcC init segment before serving the WebSocket.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ipcam_core::{EncodedPacket, VideoCodec};
use ipcam_discovery::{DeviceManagementClient, DiscoveryCredentials, parse_xaddr_endpoint};
use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::mux::AvcConfig;
use crate::mux::Fmp4Muxer;
use crate::{SessionId, SessionStateEvent};

#[derive(Default)]
struct LocalStreamState {
    sps: Option<Bytes>,
    pps: Option<Bytes>,
    muxer_configured: bool,
    /// true once the first IDR access unit has been emitted. MSE cannot
    /// decode before a keyframe, and this camera does NOT send an IDR on
    /// RTSP connect (it continues its regular GOP cycle), so everything
    /// before the first IDR is undecodable noise that must be dropped.
    got_keyframe: bool,
    /// NALs belonging to the current in-progress access unit (one frame).
    /// Flushed either on RTP marker=1 or when a "frame-start" NAL type
    /// (5 = IDR, 7 = SPS, 8 = PPS) appears at the start of a new packet.
    current_au: Vec<Bytes>,
    /// RTP timestamp of the first NAL in `current_au`; handed to the
    /// muxer so sample durations are the real inter-frame deltas.
    current_au_ts: u32,
    /// true if we already have an unflushed access unit — prevents
    /// duplicates on back-to-back IDR + SPS/PPS sequences.
    have_pending: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_streaming(
    session_id: SessionId,
    xaddr: Option<String>,
    profile_token: String,
    profile_uri: Option<String>,
    width_hint: u32,
    height_hint: u32,
    credentials: Option<DiscoveryCredentials>,
    audio_out: Option<String>,
    mux: Arc<Mutex<Fmp4Muxer>>,
    state_tx: broadcast::Sender<SessionStateEvent>,
) {
    tokio::spawn(async move {
        if width_hint > 0 {
            mux.lock().set_dimensions(width_hint, height_hint);
        }
        let rtsp_uri = match resolve_stream_uri(
            xaddr.as_deref(),
            &profile_token,
            profile_uri.as_deref(),
            credentials.as_ref(),
        )
        .await
        {
            Ok(uri) => uri,
            Err(e) => {
                tracing::warn!(err = %e, "get stream uri failed");
                let _ = state_tx.send(SessionStateEvent {
                    session_id,
                    state: ipcam_core::SessionState::Ended,
                });
                return;
            }
        };

        let audio_output = match audio_out {
            Some(device) => ipcam_gst::AudioOutput::Alsa { device },
            None => ipcam_gst::AudioOutput::Disabled,
        };
        let mut gst_cfg = ipcam_gst::GstStreamConfig {
            uri: rtsp_uri,
            audio_output,
            ..Default::default()
        };
        if let Some(c) = credentials.as_ref() {
            gst_cfg.credentials = Some((c.username.clone(), c.password.clone()));
        }

        let state = Arc::new(Mutex::new(LocalStreamState::default()));
        let on_video = {
            let mux = mux.clone();
            move |pkt: EncodedPacket| {
                // H.265 frames are received but dropped here until
                // the muxer grows hvcC support (research.md R8).
                if pkt.codec != VideoCodec::H264 {
                    tracing::warn!(codec = ?pkt.codec, "dropping non-H264 frame (muxer is H264-only)");
                    return;
                }
                ingest_packet(&mux, &state, pkt);
            }
        };
        // The web fMP4 path carries no audio track this phase; the
        // encoded frames reach the pipeline's own ALSA branch when
        // --audio-out is set (US2/T021).
        let on_audio = |_pkt: ipcam_gst::AudioPacket| {};

        match ipcam_gst::start(gst_cfg, on_video, on_audio) {
            Ok(handle) => {
                // The session lives on GStreamer threads; poll for a
                // terminal state so this task keeps the same
                // lifetime semantics the retired play_loop had.
                loop {
                    match handle.state() {
                        ipcam_gst::StreamState::Failed | ipcam_gst::StreamState::Ended => break,
                        _ => tokio::time::sleep(Duration::from_millis(200)).await,
                    }
                }
                if let Some(err) = handle.stats().last_error {
                    tracing::warn!(err = %err, "gst stream ended with error");
                }
            }
            Err(e) => {
                tracing::warn!(err = %e, "ipcam_gst start failed");
            }
        }

        let _ = state_tx.send(SessionStateEvent {
            session_id,
            state: ipcam_core::SessionState::Ended,
        });
    });
}

/// Decide whether `nal_type` indicates the start of a new access unit.
/// Per H.264, types 5 (IDR), 7 (SPS), 8 (PPS) are frame-delimiting
/// headers; a non-empty current buffer should be flushed before they.
fn is_frame_start_nal(nal_type: u8) -> bool {
    matches!(nal_type, 5 | 7 | 8)
}

fn ingest_packet(
    mux: &Arc<Mutex<Fmp4Muxer>>,
    state: &Arc<Mutex<LocalStreamState>>,
    pkt: EncodedPacket,
) {
    if pkt.codec != VideoCodec::H264 {
        return;
    }
    let data = pkt.data;
    if data.len() < 5 {
        return;
    }
    let nal_type = data[4] & 0x1F;
    let mut s = state.lock();

    // Capture SPS/PPS into AvcConfig before any flush; these NALs are
    // still part of the access unit so they also go into current_au.
    // avcC stores COMPLETE NAL units including the 1-byte NAL header
    // (ISO/IEC 14496-15) — strip only the 4-byte start code.
    if !s.muxer_configured {
        let nal = Bytes::copy_from_slice(&data[4..]);
        match nal_type {
            7 => s.sps = Some(nal),
            8 => s.pps = Some(nal),
            _ => {}
        }
        if s.sps.is_some() && s.pps.is_some() {
            let sps = s.sps.as_ref().unwrap().clone();
            let pps = s.pps.as_ref().unwrap().clone();
            let mut m = mux.lock();
            m.set_avc_config(AvcConfig { sps: sps.clone(), pps });
            // Profiles carry no width/height (ONVIF backend skips the
            // encoder-config round-trip), so derive dimensions from SPS.
            if let Some((w, h)) = crate::mux::parse_sps_dimensions(&sps) {
                m.set_dimensions(w, h);
            }
            s.muxer_configured = true;
        }
    }

    // Frame-boundary detection: a frame-start NAL (IDR/SPS/PPS) arriving
    // on an already populated buffer starts a new AU; a marker NAL is the
    // LAST fragment of the current AU — append before flushing.
    //
    // Single lock scope: `state` is a parking_lot mutex (non-reentrant).
    // `s` stays held across the flush — the lock order state → muxer is
    // the same one the SPS/PPS capture above already establishes.
    let frame_start = s.have_pending && is_frame_start_nal(nal_type);
    if frame_start {
        let au = std::mem::take(&mut s.current_au);
        let ts = s.current_au_ts;
        s.have_pending = false;
        flush_access_unit(&mut s, mux, au, ts);
    }
    if s.current_au.is_empty() {
        s.current_au_ts = pkt.rtp_ts;
    }
    s.current_au.push(data);
    s.have_pending = true;
    if pkt.marker {
        let au = std::mem::take(&mut s.current_au);
        let ts = s.current_au_ts;
        s.have_pending = false;
        flush_access_unit(&mut s, mux, au, ts);
    }
}

fn nal_type_of(nal: &[u8]) -> u8 {
    if nal.len() > 4 { nal[4] & 0x1F } else { 0 }
}

/// Emit a completed access unit to the muxer, with two filters:
///
/// - nothing goes out before the first IDR — MSE cannot decode P-slices
///   without a reference frame, and this camera doesn't force an IDR on
///   connect, so early AUs would only poison the browser's decoder;
/// - AUs without any VCL NAL (SPS/PPS/AUD/SEI-only) carry no picture;
///   the parameter sets already live in the init segment's avcC.
///
/// `rtp_ts` is the RTP timestamp of the AU's first packet; the muxer
/// turns inter-AU deltas into real sample durations.
fn flush_access_unit(
    s: &mut LocalStreamState,
    mux: &Arc<Mutex<Fmp4Muxer>>,
    au: Vec<Bytes>,
    rtp_ts: u32,
) {
    let has_idr = au.iter().any(|n| nal_type_of(n) == 5);
    let has_vcl = au.iter().any(|n| matches!(nal_type_of(n), 1..=5));
    if has_idr {
        s.got_keyframe = true;
    }
    if !s.got_keyframe || !has_vcl || au.is_empty() {
        return;
    }
    // AUD NALs carry no payload; some decoders complain about AUD-only
    // samples, so strip them (SPS/PPS/SEI stay — in-band parameter sets
    // are legal and make the stream self-healing).
    let au: Vec<Bytes> = au.into_iter().filter(|n| nal_type_of(n) != 9).collect();
    mux.lock().push_access_unit(&au, rtp_ts);
}

async fn resolve_stream_uri(
    xaddr: Option<&str>,
    profile_token: &str,
    profile_uri: Option<&str>,
    credentials: Option<&DiscoveryCredentials>,
) -> anyhow::Result<String> {
    if let Some(uri) = profile_uri {
        if !uri.is_empty() {
            return Ok(uri.to_string());
        }
    }
    let (Some(xaddr), Some(creds)) = (xaddr, credentials) else {
        anyhow::bail!("no profile.uri and no xaddr/credentials to call GetStreamUri");
    };
    let endpoint =
        parse_xaddr_endpoint(xaddr).map_err(|e| anyhow::anyhow!("invalid xaddr: {e}"))?;
    let client = DeviceManagementClient::new(endpoint, creds.clone(), Duration::from_secs(10));
    let uri = client
        .get_stream_uri(profile_token)
        .await
        .map_err(|e| anyhow::anyhow!("get_stream_uri: {e}"))?;
    Ok(uri)
}


#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(nal_type_byte: u8, payload: &[u8], marker: bool, rtp_ts: u32) -> EncodedPacket {
        let mut data = vec![0, 0, 0, 1, nal_type_byte];
        data.extend_from_slice(payload);
        EncodedPacket {
            codec: VideoCodec::H264,
            data: Bytes::from(data),
            rtp_ts,
            arrival_us: 0,
            is_keyframe: nal_type_byte & 0x1F == 5,
            marker,
        }
    }

    // Real office-camera SPS RBSP (1280x720 baseline) so the muxer becomes
    // ready once SPS+PPS are captured.
    const SPS_RBSP: &[u8] = &[0x42, 0x00, 0x1f, 0xe5, 0x40, 0x28, 0x02, 0xdc, 0x80];
    const PPS_RBSP: &[u8] = &[0xce, 0x31, 0x12];

    fn setup() -> (Arc<Mutex<Fmp4Muxer>>, Arc<Mutex<LocalStreamState>>) {
        (
            Arc::new(Mutex::new(Fmp4Muxer::new())),
            Arc::new(Mutex::new(LocalStreamState::default())),
        )
    }

    #[test]
    fn nothing_is_emitted_before_first_idr() {
        let (mux, state) = setup();
        // Camera connect burst: SPS+PPS, then P-slices (this camera does
        // not force an IDR on connect).
        ingest_packet(&mux, &state, pkt(0x67, SPS_RBSP, true, 9000));
        ingest_packet(&mux, &state, pkt(0x68, PPS_RBSP, true, 9000));
        assert!(mux.lock().is_ready(), "SPS+PPS must configure the muxer");
        ingest_packet(&mux, &state, pkt(0x41, &[0x9a, 0x20], true, 18000));
        ingest_packet(&mux, &state, pkt(0x41, &[0x9a, 0x30], true, 27000));
        assert_eq!(mux.lock().segment_count(), 0, "P-slices before first IDR must be dropped");

        // First IDR AU (AUD + IDR slice) opens the stream but only fills
        // the muxer's pending slot — a segment needs the NEXT frame's
        // timestamp to compute this one's duration.
        ingest_packet(&mux, &state, pkt(0x09, &[0xf0], false, 36000)); // AUD, no marker
        ingest_packet(&mux, &state, pkt(0x65, &[0x88, 0x84], true, 36000));
        assert_eq!(mux.lock().segment_count(), 0, "first AU stays pending its successor");

        // The next AU flushes the IDR segment...
        ingest_packet(&mux, &state, pkt(0x41, &[0x9a, 0x40], true, 45000));
        assert_eq!(mux.lock().segment_count(), 1);
        // ...and so on.
        ingest_packet(&mux, &state, pkt(0x41, &[0x9a, 0x50], true, 54000));
        assert_eq!(mux.lock().segment_count(), 2);
        // Picture-less AUs (AUD-only) are still dropped...
        ingest_packet(&mux, &state, pkt(0x09, &[0xf0], true, 63000));
        assert_eq!(mux.lock().segment_count(), 2, "VCL-less AU must be dropped");
        // ...and the following real frame absorbs the gap into the
        // pending frame's duration.
        ingest_packet(&mux, &state, pkt(0x41, &[0x9a, 0x60], true, 72000));
        assert_eq!(mux.lock().segment_count(), 3);
    }
}
