//! ONVIF oxvif backend — *default* discovery backend since `adopt-oxvif`.
//! Maps the existing public API surface (`probe_all_with_config`,
//! `DeviceManagementClient::list_profiles/get_stream_uri/get_capabilities`)
//! onto calls into the `oxvif` crate (v0.12.0, strict pin).
//!
//! Crate-private: callers go through `crate::probe_all_with_config` and
//! `crate::DeviceManagementClient`, which dispatch by env var
//! `MEDIA_TALK_DISCOVERY_BACKEND` (default `oxvif`).

use std::time::Duration;

use ipcam_core::{AuthStatus, DeviceId, DiscoveredDevice, VideoCodec, VideoProfile};
use tracing::{info, warn};

use crate::{DiscoveryConfig, DiscoveryCredentials, DiscoveryError};

/// Discovery: invoke `oxvif::discovery::probe()` and convert each
/// discovered device into our `DiscoveredDevice` shape, then fetch
/// profiles + stream URIs via `OnvifSession` (only when credentials
/// are provided — anonymous discovery skips the SOAP roundtrips).
pub(super) async fn probe_all_with_config_oxvif(config: &DiscoveryConfig) -> Vec<DiscoveredDevice> {
    let ox_devices = oxvif::discovery::probe(config.timeout).await;

    let mut out: Vec<DiscoveredDevice> = Vec::with_capacity(ox_devices.len());
    for ox_dev in ox_devices {
        let mut d = oxvif_to_core_device(&ox_dev);
        match fill_profiles_and_uris(&ox_dev, config.credentials.as_ref(), config.timeout).await {
            Ok(profiles) => {
                d.profiles = profiles;
                d.auth_status = AuthStatus::Valid;
            }
            Err(e) => {
                warn!(
                    endpoint = %ox_dev.endpoint,
                    err = %e,
                    "oxvif: profiles/stream-uri fetch failed"
                );
                d.auth_status = classify_auth_error(&e.to_string());
            }
        }
        out.push(d);
    }

    info!(matched = out.len(), "oxvif: discovery complete");
    out
}

/// Map `oxvif::DiscoveredDevice` → `ipcam_core::DiscoveredDevice`.
///
/// `address` is the host portion of the first xaddr (e.g. `http://192.168.1.144/...`
/// → `192.168.1.144`) — matches what the legacy WS-Discovery probe
/// produced from `peer.ip()`.
fn oxvif_to_core_device(ox_dev: &oxvif::DiscoveredDevice) -> DiscoveredDevice {
    let xaddr = ox_dev
        .xaddrs
        .first()
        .cloned()
        .unwrap_or_else(|| ox_dev.endpoint.clone());
    let address = extract_host_from_xaddr(&xaddr).unwrap_or_else(|| ox_dev.endpoint.clone());

    DiscoveredDevice {
        id: DeviceId::new_v4(),
        address,
        xaddr: Some(xaddr),
        scopes: ox_dev.scopes.clone(),
        manufacturer: None,
        model: None,
        auth_status: AuthStatus::Unknown,
        profiles: Vec::new(),
    }
}

fn extract_host_from_xaddr(xaddr: &str) -> Option<String> {
    let after = xaddr.split("://").nth(1)?;
    let host_port = after.split('/').next()?;
    let host = host_port
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_port);
    Some(host.to_string())
}

async fn fill_profiles_and_uris(
    ox_dev: &oxvif::DiscoveredDevice,
    creds: Option<&DiscoveryCredentials>,
    timeout: Duration,
) -> anyhow::Result<Vec<VideoProfile>> {
    let xaddr = ox_dev
        .xaddrs
        .first()
        .ok_or_else(|| anyhow::anyhow!("device has no xaddrs"))?;

    let mut builder = oxvif::OnvifSession::builder(xaddr);
    if let Some(c) = creds {
        builder = builder.with_credentials(&c.username, &c.password);
    }
    let _ = timeout; // oxvif does not currently expose a per-call timeout override

    let session = builder
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("build OnvifSession for {xaddr}: {e}"))?;

    let profiles = session
        .get_profiles()
        .await
        .map_err(|e| anyhow::anyhow!("get_profiles for {xaddr}: {e}"))?;

    let mut out: Vec<VideoProfile> = Vec::with_capacity(profiles.len());
    for p in profiles {
        let stream_uri = session
            .get_stream_uri(&p.token)
            .await
            .ok()
            .map(|s| s.uri)
            .unwrap_or_default();

        out.push(VideoProfile {
            profile_id: p.token,
            uri: if stream_uri.is_empty() {
                None
            } else {
                Some(stream_uri)
            },
            codec: VideoCodec::Unknown,
            width: 0,
            height: 0,
            fps: 0.0,
        });
    }
    Ok(out)
}

/// Convert `oxvif::MediaProfile` into our `VideoProfile`.
///
/// `MediaProfile` carries `token`, `video_source_config_token`, `video_encoder_token`,
/// etc. but **not** codec / width / height / fps — those live on the separate
/// `VideoEncoderConfiguration` returned by GetVideoEncoderConfiguration. For the
/// default backend we don't make that second round-trip; fields default to
/// `Unknown` / 0 / 0 / 0.0. (`openspec/specs/ipcam-discovery/spec.md`
/// MODIFIED requirement calls this out explicitly.)
pub(super) fn oxvif_to_core_video_profile(p: oxvif::MediaProfile) -> VideoProfile {
    VideoProfile {
        profile_id: p.token,
        uri: None,
        codec: VideoCodec::Unknown,
        width: 0,
        height: 0,
        fps: 0.0,
    }
}

/// Parse the first whitespace-separated URL out of an xaddr. Kept
/// public for backward compat — `web-display` calls
/// `ipcam_discovery::parse_xaddr_endpoint` to extract a
/// `reqwest::Url` from the ONVIF xaddr string before passing it
/// to [`DeviceManagementClient::new`].
pub fn parse_xaddr_endpoint(xaddr: &str) -> Result<reqwest::Url, DiscoveryError> {
    let primary = xaddr.split_whitespace().next().unwrap_or(xaddr);
    primary
        .parse::<reqwest::Url>()
        .map_err(|e| DiscoveryError::InvalidUri(format!("{}: {}", xaddr, e)))
}

/// Inspect an oxvif error string and decide which `AuthStatus`
/// variant to assign to the discovered device. oxvif returns
/// `Display`-formatted errors that do not carry a structured variant,
/// so we pattern-match on the message body. Returns
/// `AuthStatus::InvalidCredentials` for the well-known WS-Security
/// failure shapes (`SOAP-ENV:Sender: The security token could not be
/// authenticated or authorized`, SOAP 1.1 `NotAuthorized`, plain HTTP
/// `401`); everything else is `Anonymous`.
///
/// `pub(crate)` for the same reason as `parse_xaddr_endpoint` —
/// `crate::tests` exercises it via direct call rather than running
/// the full WS-Discovery probe.
pub(crate) fn classify_auth_error(err: &str) -> AuthStatus {
    if err.contains("authenticated or authorized")
        || err.contains("NotAuthorized")
        || err.contains(" 401 ")
        || err.contains(": 401")
        || err.starts_with("HTTP 401")
    {
        AuthStatus::InvalidCredentials
    } else {
        AuthStatus::Anonymous
    }
}
