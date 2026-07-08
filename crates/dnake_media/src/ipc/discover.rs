use ipcam_core::DiscoveredDevice;
use ipcam_discovery::{DiscoveryConfig, DiscoveryCredentials};
use tracing::info;

const DEFAULT_USER: &str = "admin";
const DEFAULT_PASS: &str = "changeme";

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
    let credentials = DiscoveryCredentials::new(user, pass);
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
            println!("(no ONVIF devices found in {timeout_secs}s)");
        } else {
            println!("Found {} device(s):", devices.len());
            for d in &devices {
                let profiles = d.profiles.len();
                println!(
                    "  - {} @ {} (auth={:?}, profiles={})",
                    d.id, d.address, d.auth_status, profiles
                );
                for p in &d.profiles {
                    println!(
                        "      profile={} codec={:?} {}x{} @ {:.1}fps uri={:?}",
                        p.profile_id, p.codec, p.width, p.height, p.fps, p.uri
                    );
                }
            }
        }
    }
    Ok(())
}
