//! Per-session RTSP → WebRTC streaming pipeline.
//!
//! When a session is created via `POST /api/sessions`, this task:
//!   1. Calls ONVIF `GetStreamUri` (with the user-supplied credentials
//!      held by the registry) to obtain an RTSP URL, unless the profile
//!      already carries one (manual RTSP entries).
//!   2. Starts an `ipcam_gst` session (rtspsrc → depay/parse →
//!      webrtcsink) which republishes the encoded stream on the
//!      process-wide signalling server under the session id as the
//!      producer name (`meta.name`). The browser's gstwebrtc-api client
//!      matches on that name to consume the right camera.
//!   3. Stores the `GstStreamHandle` in the registry so
//!      `DELETE /api/sessions/{id}` tears the pipeline down.

use std::sync::Arc;
use std::time::Duration;

use ipcam_discovery::{DeviceManagementClient, DiscoveryCredentials, parse_xaddr_endpoint};
use tokio::sync::broadcast;

use crate::{SessionId, SessionRegistry, SessionStateEvent};

#[allow(clippy::too_many_arguments)]
pub fn spawn_streaming(
    session_id: SessionId,
    xaddr: Option<String>,
    profile_token: String,
    profile_uri: Option<String>,
    credentials: Option<DiscoveryCredentials>,
    audio_out: Option<String>,
    registry: Arc<SessionRegistry>,
    state_tx: broadcast::Sender<SessionStateEvent>,
) {
    tokio::spawn(async move {
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
            // The browser consumer matches producers by meta.name, so
            // the session id doubles as the producer name.
            stream_name: session_id.to_string(),
            ..Default::default()
        };
        if let Some(c) = credentials.as_ref() {
            gst_cfg.credentials = Some((c.username.clone(), c.password.clone()));
        }

        match ipcam_gst::start(gst_cfg) {
            Ok(handle) => {
                registry.set_handle(session_id, handle.clone());
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
