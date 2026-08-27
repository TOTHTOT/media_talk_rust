//! Per-session RTSP streaming pipeline.
//!
//! When a session is created via `POST /api/sessions`, this task:
//!   1. Calls ONVIF `GetStreamUri` (with the user-supplied credentials
//!      held by the registry) to obtain an RTSP URL.
//!   2. Starts an `ipcam_gst` session (rtspsrc → depay/parse → appsink)
//!      which pushes each complete Annex-B H.264 access unit (one frame
//!      per callback, `alignment=au`) into the per-session fMP4 muxer.
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
                ingest_au(&mux, &state, pkt);
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

/// Ingest ONE complete access unit (one frame, Annex-B) from the
/// GStreamer appsink. `alignment=au` guarantees one packet == one AU, so
/// no reassembly happens here — the AU is only inspected and filtered:
///
/// - SPS/PPS are captured into the muxer's AvcConfig on first sight
///   (the avcC init segment cannot be built without them);
/// - nothing goes out before the first IDR — MSE cannot decode P-slices
///   without a reference frame, and this camera doesn't force an IDR on
///   connect, so early AUs would only poison the browser's decoder;
/// - AUs without any VCL NAL (SPS/PPS/AUD/SEI-only) carry no picture and
///   are dropped; AUD NALs are stripped from the rest.
fn ingest_au(
    mux: &Arc<Mutex<Fmp4Muxer>>,
    state: &Arc<Mutex<LocalStreamState>>,
    pkt: EncodedPacket,
) {
    if pkt.codec != VideoCodec::H264 {
        return;
    }
    // Split for inspection only; each returned NAL is normalized to a
    // 4-byte start code, so the NAL header byte is at nal[4].
    let nals = ipcam_gst::packet::split_au_into_nals(&pkt.data);
    if nals.is_empty() {
        return;
    }
    let nal_type = |n: &Bytes| n[4] & 0x1F;
    let mut s = state.lock();

    // Capture SPS/PPS into AvcConfig. avcC stores COMPLETE NAL units
    // including the 1-byte NAL header (ISO/IEC 14496-15) — strip only
    // the 4-byte start code.
    if !s.muxer_configured {
        for nal in &nals {
            let body = Bytes::copy_from_slice(&nal[4..]);
            match nal_type(nal) {
                7 => s.sps = Some(body),
                8 => s.pps = Some(body),
                _ => {}
            }
        }
        if let (Some(sps), Some(pps)) = (s.sps.clone(), s.pps.clone()) {
            let mut m = mux.lock();
            m.set_avc_config(AvcConfig {
                sps: sps.clone(),
                pps,
            });
            // Profiles carry no width/height (ONVIF backend skips the
            // encoder-config round-trip), so derive dimensions from SPS.
            if let Some((w, h)) = crate::mux::parse_sps_dimensions(&sps) {
                m.set_dimensions(w, h);
            }
            s.muxer_configured = true;
        }
    }

    let has_idr = nals.iter().any(|n| nal_type(n) == 5);
    let has_vcl = nals.iter().any(|n| matches!(nal_type(n), 1..=5));
    if has_idr {
        s.got_keyframe = true;
    }
    if !s.got_keyframe || !has_vcl {
        return;
    }
    // AUD NALs carry no payload; strip them (SPS/PPS/SEI stay — in-band
    // parameter sets are legal and make the stream self-healing).
    let filtered: Vec<Bytes> = nals.into_iter().filter(|n| nal_type(n) != 9).collect();
    mux.lock().push_access_unit(&filtered, pkt.rtp_ts);
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

    /// Build ONE complete access unit (Annex-B) from (nal_byte, payload)
    /// pairs — the shape the GStreamer appsink delivers with alignment=au.
    fn au(nals: &[(u8, &[u8])], rtp_ts: u32) -> EncodedPacket {
        let mut data = Vec::new();
        for (t, p) in nals {
            data.extend_from_slice(&[0, 0, 0, 1, *t]);
            data.extend_from_slice(p);
        }
        EncodedPacket {
            codec: VideoCodec::H264,
            data: Bytes::from(data),
            rtp_ts,
            arrival_us: 0,
            is_keyframe: nals.iter().any(|(t, _)| t & 0x1F == 5),
            marker: true,
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
        // Camera connect burst: SPS+PPS attached to a P-frame AU (this
        // camera does not force an IDR on connect).
        ingest_au(
            &mux,
            &state,
            au(
                &[(0x67, SPS_RBSP), (0x68, PPS_RBSP), (0x41, &[0x9a, 0x20])],
                9000,
            ),
        );
        assert!(mux.lock().is_ready(), "SPS+PPS must configure the muxer");
        assert_eq!(mux.lock().segment_count(), 0, "pre-IDR AU must be dropped");
        ingest_au(&mux, &state, au(&[(0x41, &[0x9a, 0x30])], 18000));
        assert_eq!(
            mux.lock().segment_count(),
            0,
            "P-slices before first IDR must be dropped"
        );

        // First IDR AU (AUD + IDR slice) opens the stream but only fills
        // the muxer's pending slot — a segment needs the NEXT frame's
        // timestamp to compute this one's duration.
        ingest_au(
            &mux,
            &state,
            au(&[(0x09, &[0xf0]), (0x65, &[0x88, 0x84])], 27000),
        );
        assert_eq!(
            mux.lock().segment_count(),
            0,
            "first AU stays pending its successor"
        );

        // The next AU flushes the IDR segment...
        ingest_au(&mux, &state, au(&[(0x41, &[0x9a, 0x40])], 36000));
        assert_eq!(mux.lock().segment_count(), 1);
        // Picture-less AUs (AUD-only) are dropped...
        ingest_au(&mux, &state, au(&[(0x09, &[0xf0])], 45000));
        assert_eq!(mux.lock().segment_count(), 1, "VCL-less AU must be dropped");
        // ...and the following real frame absorbs the gap into the
        // pending frame's duration.
        ingest_au(&mux, &state, au(&[(0x41, &[0x9a, 0x50])], 54000));
        assert_eq!(mux.lock().segment_count(), 2);
    }
}
