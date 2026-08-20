//! Per-session RTSP streaming pipeline.
//!
//! When a session is created via `POST /api/sessions`, this task:
//!   1. Calls ONVIF `GetStreamUri` (with the user-supplied credentials
//!      held by the registry) to obtain an RTSP URL.
//!   2. Starts an `ipcam_gst` session (rtspsrc → depay/parse → appsink,
//!      requires the `gst` feature) which pushes each Annex-B H.264 NAL
//!      into the per-session fMP4 muxer.
//!   3. Captures SPS/PPS from the first packets so the muxer can emit a
//!      proper avcC init segment before serving the WebSocket.
//!
//! Without the `gst` feature the task logs an error and immediately
//! reports `Ended` — the binary still builds and runs on hosts without
//! GStreamer, just without live video.

use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "gst")]
use bytes::Bytes;
#[cfg(feature = "gst")]
use ipcam_core::{EncodedPacket, VideoCodec};
use ipcam_discovery::{DeviceManagementClient, DiscoveryCredentials, parse_xaddr_endpoint};
use parking_lot::Mutex;
use tokio::sync::broadcast;

#[cfg(feature = "gst")]
use crate::mux::AvcConfig;
use crate::mux::Fmp4Muxer;
use crate::{SessionId, SessionStateEvent};

#[cfg(feature = "gst")]
#[derive(Default)]
struct LocalStreamState {
    sps: Option<Bytes>,
    pps: Option<Bytes>,
    muxer_configured: bool,
    /// NALs belonging to the current in-progress access unit (one frame).
    /// Flushed either on RTP marker=1 or when a "frame-start" NAL type
    /// (5 = IDR, 7 = SPS, 8 = PPS) appears at the start of a new packet.
    current_au: Vec<Bytes>,
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

        #[cfg(feature = "gst")]
        {
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
                    // lifetime semantics the old play_loop had.
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
        }

        #[cfg(not(feature = "gst"))]
        {
            let _ = &rtsp_uri;
            if audio_out.is_some() {
                tracing::warn!("--audio-out ignored: built without the gst feature");
            }
            tracing::error!("streaming requires building with --features gst (GStreamer runtime)");
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
#[cfg(feature = "gst")]
fn is_frame_start_nal(nal_type: u8) -> bool {
    matches!(nal_type, 5 | 7 | 8)
}

#[cfg(feature = "gst")]
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
    if !s.muxer_configured {
        let rbsp = Bytes::copy_from_slice(&data[5..]);
        match nal_type {
            7 => s.sps = Some(rbsp),
            8 => s.pps = Some(rbsp),
            _ => {}
        }
        if s.sps.is_some() && s.pps.is_some() {
            let sps = s.sps.as_ref().unwrap().clone();
            let pps = s.pps.as_ref().unwrap().clone();
            mux.lock().set_avc_config(AvcConfig { sps, pps });
            s.muxer_configured = true;
        }
    }

    // Frame-boundary detection: either RTP marker=1 (last fragment of
    // the access unit) or a frame-start NAL arriving on an already
    // populated buffer.
    let flush_now = pkt.marker || (s.have_pending && is_frame_start_nal(nal_type));
    if flush_now {
        let au = std::mem::take(&mut s.current_au);
        s.have_pending = false;
        drop(s);
        mux.lock().push_access_unit(&au);
    } else {
        s.have_pending = true;
    }

    // Always append this NAL to the current buffer.
    let mut s = state.lock();
    s.current_au.push(data);
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
