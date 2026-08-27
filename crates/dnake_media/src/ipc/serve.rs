use anyhow::Context;
use ipcam_discovery::DiscoveryCredentials;
use tracing::info;

/// Return the first local IPv4 address, or "unknown" on error.
fn local_ip() -> String {
    if let Ok(ip) = local_ip_address::local_ip() {
        ip.to_string()
    } else {
        "unknown".to_string()
    }
}

// Default ONVIF credentials baked in for cheap IP cameras. CLI args always
// win; this is just so `serve` works out of the box against the typical
// admin/changeme / admin/admin devices found on a small LAN.
const DEFAULT_USER: &str = "admin";
const DEFAULT_PASS: &str = "changeme";

pub async fn run(
    bind: String,
    discovery_timeout_secs: u64,
    username: Option<String>,
    password: Option<String>,
    rtsp_urls: Vec<String>,
    audio_out: Option<String>,
) -> anyhow::Result<()> {
    info!(bind = %format_args!("http://{}", bind), local_ip = %format_args!("http://{}:8080", local_ip()), discovery_timeout_secs, ?username, manual = rtsp_urls.len(), audio_out = ?audio_out, "starting media server");
    let timeout = std::time::Duration::from_secs(discovery_timeout_secs);
    let (user, pass) = match (username, password) {
        (Some(u), Some(p)) => (u, p),
        _ => (DEFAULT_USER.to_string(), DEFAULT_PASS.to_string()),
    };
    let credentials = DiscoveryCredentials::new(user, pass);
    let web = web_display::WebDisplay::start(
        &bind,
        timeout,
        Some(credentials),
        manual_devices_from_urls(&rtsp_urls),
        audio_out,
    )
    .await
    .context("failed to start web display server")?;
    info!(addr = %web.local_addr(), "server ready");
    web.wait_for_shutdown().await
}

/// Build `DiscoveredDevice` entries from a list of user-supplied RTSP URLs.
/// Skips ONVIF entirely: xaddr is None and the URL itself is the profile URI.
fn manual_devices_from_urls(urls: &[String]) -> Vec<ipcam_core::DiscoveredDevice> {
    use ipcam_core::{AuthStatus, DiscoveredDevice, VideoProfile};
    urls.iter()
        .filter_map(|url| {
            let (label, host) = parse_rtsp_label(url)?;
            Some(DiscoveredDevice {
                id: uuid::Uuid::new_v4(),
                address: host.unwrap_or_else(|| label.clone()),
                xaddr: None,
                scopes: vec!["manual".to_string()],
                manufacturer: None,
                model: Some("manual-rtsp".to_string()),
                auth_status: AuthStatus::Valid,
                profiles: vec![VideoProfile {
                    profile_id: "p1".to_string(),
                    uri: Some(url.clone()),
                    codec: ipcam_core::VideoCodec::Unknown,
                    width: 0,
                    height: 0,
                    fps: 0.0,
                }],
            })
        })
        .collect()
}

/// Cheap `rtsp://[user[:pwd]@]host[:port]/path` parser. Just enough to
/// extract a host label for the UI; the URL is passed to RTSP as-is.
fn parse_rtsp_label(url: &str) -> Option<(String, Option<String>)> {
    let rest = url.strip_prefix("rtsp://")?;
    let (authority, _path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host_port = match authority.find('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    let host = host_port
        .split(':')
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Some((url.to_string(), host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_devices_builds_one_per_url() {
        let urls = vec![
            "rtsp://admin:changeme@192.168.1.13:8554/ch01".to_string(),
            "rtsp://10.0.0.5:554/stream".to_string(),
        ];
        let devs = manual_devices_from_urls(&urls);
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[0].profiles.len(), 1);
        assert_eq!(
            devs[0].profiles[0].uri.as_deref(),
            Some("rtsp://admin:changeme@192.168.1.13:8554/ch01")
        );
        assert!(devs[0].xaddr.is_none());
    }

    #[test]
    fn parse_rtsp_label_extracts_host() {
        let (label, host) =
            parse_rtsp_label("rtsp://admin:changeme@192.168.1.13:8554/ch01").unwrap();
        assert_eq!(label, "rtsp://admin:changeme@192.168.1.13:8554/ch01");
        assert_eq!(host.as_deref(), Some("192.168.1.13"));
    }
}
