use ipcam_core::DiscoveredDevice;
use ipcam_discovery::{DiscoveryConfig, DiscoveryCredentials};
use tracing::info;

const DEFAULT_USER: &str = "admin";
const DEFAULT_PASS: &str = "changeme";

/// Build a directly-usable RTSP URL by embedding the effective credentials
/// into the ONVIF-reported URI (`rtsp://host/path` -> `rtsp://user:pass@host/path`).
fn full_rtsp_url(uri: &str, user: &str, pass: &str) -> String {
    match uri.strip_prefix("rtsp://") {
        Some(rest) if !rest.contains('@') => format!("rtsp://{user}:{pass}@{rest}"),
        _ => uri.to_string(),
    }
}

pub async fn run(
    timeout_secs: u64,
    json: bool,
    username: Option<String>,
    password: Option<String>,
) -> anyhow::Result<()> {
    info!(timeout_secs, ?username, "starting ONVIF discovery");
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let (user, pass) = match (username, password) {
        (Some(u), Some(p)) => (u, p),
        _ => (DEFAULT_USER.to_string(), DEFAULT_PASS.to_string()),
    };
    let credentials = DiscoveryCredentials::new(user.clone(), pass.clone());
    let config = DiscoveryConfig {
        timeout,
        credentials: Some(credentials),
        ..Default::default()
    };
    let devices: Vec<DiscoveredDevice> = ipcam_discovery::probe_all_with_config(config).await;

    if json {
        let payload = serde_json::to_string_pretty(&devices)?;
        println!("{payload}");
    } else {
        if devices.is_empty() {
            info!("no ONVIF devices found in {timeout_secs}s");
        } else {
            info!(count = devices.len(), "found device(s)");
            for d in &devices {
                info!(
                    id = %d.id,
                    address = %d.address,
                    auth = ?d.auth_status,
                    profiles = d.profiles.len(),
                    "device"
                );
                for p in &d.profiles {
                    let rtsp = p.uri.as_deref().map(|u| full_rtsp_url(u, &user, &pass));
                    info!(
                        profile = %p.profile_id,
                        codec = ?p.codec,
                        width = p.width,
                        height = p.height,
                        fps = p.fps,
                        rtsp = ?rtsp,
                        "profile"
                    );
                }
            }
        }
    }
    Ok(())
}
