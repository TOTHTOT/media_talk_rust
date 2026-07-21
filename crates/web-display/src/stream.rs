//! Per-session RTSP streaming pipeline.
//!
//! When a session is created via `POST /api/sessions`, this task:
//!   1. Calls ONVIF `GetStreamUri` (with the user-supplied credentials
//!      held by the registry) to obtain an RTSP URL.
//!   2. Connects to that RTSP URL, performs OPTIONS / DESCRIBE / SETUP /
//!      PLAY.
//!   3. Reads interleaved RTP/H.264 frames, depacketizes them via
//!      `ipcam_rtsp::H264Depacketizer`, and pushes each NAL unit into
//!      the per-session fMP4 muxer.
//!   4. Captures SPS/PPS from the first packets so the muxer can emit a
//!      proper avcC init segment before serving the WebSocket.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ipcam_core::{EncodedPacket, VideoCodec};
use ipcam_discovery::{DeviceManagementClient, DiscoveryCredentials, parse_xaddr_endpoint};
use ipcam_rtsp::{RtspClient, RtspConfig};
use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::mux::{AvcConfig, Fmp4Muxer};
use crate::{SessionId, SessionStateEvent};

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

        let mut cfg = RtspConfig::new(rtsp_uri.clone());
        if let Some(c) = credentials.as_ref() {
            cfg = cfg.with_credentials(c.username.clone(), c.password.clone());
        }
        let client = RtspClient::new(cfg);

        if let Err(e) = client.connect().await {
            tracing::warn!(err = %e, "rtsp connect failed");
            let _ = state_tx.send(SessionStateEvent {
                session_id,
                state: ipcam_core::SessionState::Ended,
            });
            return;
        }

        let state = Arc::new(Mutex::new(LocalStreamState::default()));

        let on_video = {
            let mux = mux.clone();
            let state = state.clone();
            move |pkt: EncodedPacket| -> ipcam_core::CoreResult<()> {
                ingest_packet(&mux, &state, pkt);
                Ok(())
            }
        };
        let on_audio = |_pkt: EncodedPacket| -> ipcam_core::CoreResult<()> { Ok(()) };

        if let Err(e) = client.play_loop(on_video, on_audio).await {
            tracing::warn!(err = %e, "rtsp play_loop ended");
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
