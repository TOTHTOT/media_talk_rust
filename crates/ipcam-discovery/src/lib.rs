//! ONVIF WS-Discovery + Device Management for `media_talk`.
//!
//! Implementation delegates to the `oxvif` 0.12.0 crate (strict pin).
//! Public API (`probe_all_with_config`, [`DeviceManagementClient`], [`Discovery`])
//! stays identical to the pre-oxvif implementation so callers in `media_talk`
//! and `web-display` need no changes.

use std::time::Duration;

use ipcam_core::{DiscoveredDevice, VideoProfile};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;

mod oxvif_backend;

pub const WS_DISCOVERY_MULTICAST: &str = "239.255.255.250:3702";
pub const WS_DISCOVERY_PORT: u16 = 3702;

// ----- public types (unchanged surface) -----

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("xml parse: {0}")]
    Xml(String),
    #[error("device mgmt: {0}")]
    DeviceMgmt(String),
    #[error("invalid uri: {0}")]
    InvalidUri(String),
    #[error("timeout")]
    Timeout,
    #[error("backend error: {0}")]
    Backend(String),
}

pub type DiscoveryResult<T> = Result<T, DiscoveryError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryCredentials {
    pub username: String,
    pub password: String,
}

impl DiscoveryCredentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub timeout: Duration,
    pub credentials: Option<DiscoveryCredentials>,
    pub interfaces: Vec<std::net::IpAddr>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            credentials: None,
            interfaces: Vec::new(),
        }
    }
}

pub trait Discovery {
    fn probe(&self) -> impl futures_util::stream::Stream<Item = DiscoveredDevice> + Send;
    fn snapshot(&self) -> Vec<DiscoveredDevice>;
}

pub async fn probe_all(timeout: Duration) -> Vec<DiscoveredDevice> {
    probe_all_with_config(DiscoveryConfig {
        timeout,
        ..Default::default()
    })
    .await
}

/// Discovers ONVIF devices via `oxvif::discovery::probe`. See
/// [`oxvif_backend::probe_all_with_config_oxvif`] for implementation.
pub async fn probe_all_with_config(config: DiscoveryConfig) -> Vec<DiscoveredDevice> {
    info!(?config, "probe config: ");
    oxvif_backend::probe_all_with_config_oxvif(&config).await
}

/// Re-export of [`ipcam_discovery`]'s URL parser (kept for
/// backward-compat — was `device_mgmt::parse_xaddr_endpoint`).
pub use crate::oxvif_backend::parse_xaddr_endpoint;

#[derive(Debug, Clone)]
pub struct ProbeResults {
    pub devices: Vec<DiscoveredDevice>,
}

impl ProbeResults {
    pub fn empty() -> Self {
        Self {
            devices: Vec::new(),
        }
    }
}

// ----- DeviceManagementClient (sync construction, async ops) -----

pub struct DeviceManagementClient {
    xaddr: reqwest::Url,
    credentials: DiscoveryCredentials,
    #[allow(dead_code)]
    timeout: Duration,
    oxvif_session: tokio::sync::OnceCell<oxvif::OnvifSession>,
}

impl DeviceManagementClient {
    pub fn new(base: reqwest::Url, credentials: DiscoveryCredentials, timeout: Duration) -> Self {
        Self {
            xaddr: base,
            credentials,
            timeout,
            oxvif_session: tokio::sync::OnceCell::new(),
        }
    }

    async fn session(&self) -> &oxvif::OnvifSession {
        self.oxvif_session
            .get_or_init(|| async {
                let mut b = oxvif::OnvifSession::builder(self.xaddr.as_str());
                b = b.with_credentials(&self.credentials.username, &self.credentials.password);
                // build() returns Result; OnceCell::get_or_init takes the future's
                // value not a Result. A build failure here means the supplied
                // xaddr/creds are malformed — surface it as panic.
                b.build().await.expect("build OnvifSession")
            })
            .await
    }

    pub async fn list_profiles(&self) -> Result<Vec<VideoProfile>, DiscoveryError> {
        let profiles = self
            .session()
            .await
            .get_profiles()
            .await
            .map_err(|e| DiscoveryError::DeviceMgmt(e.to_string()))?;
        Ok(profiles
            .into_iter()
            .map(oxvif_backend::oxvif_to_core_video_profile)
            .collect())
    }

    pub async fn get_stream_uri(&self, profile_token: &str) -> Result<String, DiscoveryError> {
        let stream_uri = self
            .session()
            .await
            .get_stream_uri(profile_token)
            .await
            .map_err(|e| DiscoveryError::DeviceMgmt(e.to_string()))?;
        Ok(stream_uri.uri)
    }

    /// `oxvif::OnvifSession` caches capabilities on first build, so
    /// capabilities aren't a separate call. This method returns the
    /// cached xaddr + a hint; we don't expose raw capabilities here.
    pub async fn get_capabilities(&self) -> Result<String, DiscoveryError> {
        // oxvif doesn't expose capabilities directly; the closest equivalent
        // is the xaddr we connected to.
        Ok(self.xaddr.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipcam_core::AuthStatus;

    #[test]
    fn parse_xaddr_endpoint_returns_first_url() {
        let url =
            parse_xaddr_endpoint("http://192.168.1.144/onvif/device_service").expect("parse");
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("192.168.1.144"));
    }

    #[test]
    fn parse_xaddr_endpoint_skips_whitespace_before_url() {
        let url = parse_xaddr_endpoint("  http://10.0.0.1/onvif  ").expect("parse");
        assert_eq!(url.host_str(), Some("10.0.0.1"));
    }

    #[test]
    fn parse_xaddr_endpoint_rejects_garbage() {
        assert!(parse_xaddr_endpoint("not-a-url").is_err());
    }

    #[test]
    fn classify_auth_error_recognises_soap_fault() {
        assert_eq!(
            oxvif_backend::classify_auth_error(
                "get_profiles for http://192.168.1.19/onvif/device_service: SOAP fault [SOAP-ENV:Sender]: The security token could not be authenticated or authorized"
            ),
            AuthStatus::InvalidCredentials
        );
    }

    #[test]
    fn classify_auth_error_recognises_not_authorized() {
        assert_eq!(
            oxvif_backend::classify_auth_error("SOAP fault NotAuthorized"),
            AuthStatus::InvalidCredentials
        );
    }

    #[test]
    fn classify_auth_error_recognises_http_401() {
        assert_eq!(
            oxvif_backend::classify_auth_error("HTTP 401 unauthorized"),
            AuthStatus::InvalidCredentials
        );
    }

    #[test]
    fn classify_auth_error_default_is_anonymous() {
        assert_eq!(
            oxvif_backend::classify_auth_error("connection refused"),
            AuthStatus::Anonymous
        );
    }
}
