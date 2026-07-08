use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream::Stream;
use ipcam_core::{AuthStatus, DeviceId, DiscoveredDevice, VideoProfile};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};
use uuid::Uuid;

pub mod device_mgmt;
pub mod soap;
pub mod ws_security;

use device_mgmt::{DeviceManagementClient, DeviceMgmtError};

pub const WS_DISCOVERY_MULTICAST: &str = "239.255.255.250:3702";
pub const WS_DISCOVERY_PORT: u16 = 3702;

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("xml parse: {0}")]
    Xml(String),
    #[error("device mgmt: {0}")]
    DeviceMgmt(#[from] DeviceMgmtError),
    #[error("invalid uri: {0}")]
    InvalidUri(String),
    #[error("timeout")]
    Timeout,
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
    fn probe(&self) -> impl Stream<Item = DiscoveredDevice> + Send;
    fn snapshot(&self) -> Vec<DiscoveredDevice>;
}

pub async fn probe_all(timeout: Duration) -> Vec<DiscoveredDevice> {
    probe_all_with_config(DiscoveryConfig {
        timeout,
        ..Default::default()
    })
    .await
}

pub async fn probe_all_with_config(config: DiscoveryConfig) -> Vec<DiscoveredDevice> {
    let probes = ws_discovery_probe(&config).await;
    let mut out: Vec<DiscoveredDevice> = Vec::new();
    for probe_match in probes {
        let device_id = DeviceId::new_v4();
        let mut device = DiscoveredDevice {
            id: device_id,
            address: probe_match.address.clone(),
            xaddr: Some(probe_match.xaddr.clone()),
            scopes: probe_match.scopes.clone(),
            manufacturer: None,
            model: None,
            auth_status: AuthStatus::Unknown,
            profiles: Vec::new(),
        };

        if let Some(creds) = &config.credentials {
            match fetch_profiles(&probe_match.xaddr, creds, &config.timeout).await {
                Ok(profiles) => {
                    device.auth_status = AuthStatus::Valid;
                    device.profiles = profiles;
                }
                Err(DiscoveryError::DeviceMgmt(DeviceMgmtError::AuthFailed)) => {
                    warn!(addr = %probe_match.address, "auth failed");
                    device.auth_status = AuthStatus::InvalidCredentials;
                }
                Err(e) => {
                    warn!(addr = %probe_match.address, err = %e, "device mgmt error");
                    device.auth_status = AuthStatus::Anonymous;
                }
            }
        } else {
            device.auth_status = AuthStatus::Anonymous;
        }
        out.push(device);
    }
    out
}

#[derive(Debug, Clone)]
struct ProbeMatch {
    address: String,
    xaddr: String,
    scopes: Vec<String>,
    #[allow(dead_code)]
    types: Vec<String>,
}

async fn ws_discovery_probe(config: &DiscoveryConfig) -> Vec<ProbeMatch> {
    let msg = build_probe_message();
    let dest: SocketAddr = WS_DISCOVERY_MULTICAST.parse().unwrap();

    let bind_addrs: Vec<std::net::IpAddr> = if config.interfaces.is_empty() {
        default_local_addrs()
    } else {
        config.interfaces.clone()
    };

    let mut all = Vec::new();
    let mut bind_count = 0usize;
    let mut send_count = 0usize;
    let mut recv_count = 0usize;

    for addr in bind_addrs {
        let bind_addr = std::net::SocketAddr::new(addr, 0);
        match UdpSocket::bind(bind_addr).await {
            Ok(sock) => {
                bind_count += 1;
                match send_probe(&sock, &msg, dest).await {
                    Ok(()) => {
                        send_count += 1;
                        debug!(local = %bind_addr, dest = %dest, "ws-discovery probe sent");
                    }
                    Err(e) => {
                        warn!(addr = %addr, err = %e, "send probe failed");
                        continue;
                    }
                }
                let (replies, raw_count) = collect_replies(&sock, config.timeout).await;
                recv_count += raw_count;
                for r in replies {
                    all.push(r);
                }
            }
            Err(e) => {
                debug!(addr = %addr, err = %e, "bind failed");
            }
        }
    }

    info!(
        bound = bind_count,
        sent = send_count,
        recv_packets = recv_count,
        matched = all.len(),
        "ws-discovery probe summary"
    );
    all
}

async fn send_probe(sock: &UdpSocket, msg: &Bytes, dest: SocketAddr) -> std::io::Result<()> {
    sock.send_to(msg, dest).await?;
    Ok(())
}

async fn collect_replies(sock: &UdpSocket, timeout: Duration) -> (Vec<ProbeMatch>, usize) {
    let mut buf = vec![0u8; 8192];
    let mut out = Vec::new();
    let mut raw_count = 0usize;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let recv = tokio::time::timeout(remaining, sock.recv_from(&mut buf)).await;
        match recv {
            Ok(Ok((n, peer))) => {
                raw_count += 1;
                let body = &buf[..n];
                if let Some(m) = parse_probe_message(body) {
                    let mut m = m;
                    if m.address.is_empty() {
                        m.address = peer.ip().to_string();
                    }
                    out.push(m);
                } else {
                    let has_pm = body.windows(10).any(|w| w == b"ProbeMatch");
                    let has_hello = body.windows(5).any(|w| w == b"Hello");
                    let has_xaddrs = body.windows(7).any(|w| w == b"XAddrs");
                    let has_resolve = body.windows(13).any(|w| w == b"ResolveMatches");
                    debug!(
                        peer = %peer.ip(),
                        n,
                        has_probe_match = has_pm,
                        has_hello,
                        has_xaddrs,
                        has_resolve,
                        "received non-matching packet"
                    );
                }
            }
            Ok(Err(e)) => {
                warn!(err = %e, "recv failed");
                break;
            }
            Err(_) => break,
        }
    }
    (out, raw_count)
}

fn build_probe_message() -> Bytes {
    let msg_id = Uuid::new_v4();
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Envelope xmlns:dn="http://www.onvif.org/ver10/network/wsdl" xmlns="http://www.w3.org/2003/05/soap-envelope">
  <Header>
    <wsa:MessageID xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing">uuid:{msg_id}</wsa:MessageID>
    <wsa:To xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing">urn:schemas-xmlsoap-org:ws:2005:04:discovery</wsa:To>
    <wsa:Action xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</wsa:Action>
  </Header>
  <Body>
    <Probe xmlns="http://schemas.xmlsoap.org/ws/2005/04/discovery">
      <Types>dn:NetworkVideoTransmitter</Types>
    </Probe>
  </Body>
</Envelope>"#
    );
    Bytes::from(body)
}

fn local_name(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes).to_string();
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

fn parse_probe_message(body: &[u8]) -> Option<ProbeMatch> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut current_xaddr: Option<String> = None;
    let mut scopes: Vec<String> = Vec::new();
    let mut types: Vec<String> = Vec::new();
    let mut current_local: String = String::new();
    let mut capture = false;
    let mut in_proberesp = false;
    let mut in_match = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let lname = local_name(e.name().as_ref()).to_ascii_lowercase();
                if lname == "proberesponse" || lname == "probematches" {
                    in_proberesp = true;
                }
                if in_proberesp && (lname == "proberesponse" || lname == "probematch") {
                    in_match = true;
                }
                current_local = lname.clone();
                if matches!(
                    lname.as_str(),
                    "xaddrs"
                        | "xaddress"
                        | "xaddr"
                        | "scopes"
                        | "types"
                        | "relatesto"
                        | "endpointreference"
                        | "address"
                ) {
                    capture = matches!(
                        lname.as_str(),
                        "xaddrs" | "xaddress" | "xaddr" | "scopes" | "types"
                    );
                }
            }
            Ok(Event::Text(t)) if capture => {
                let txt = t.unescape().unwrap_or_default().to_string();
                if in_match {
                    match current_local.as_str() {
                        "xaddrs" | "xaddress" | "xaddr" => current_xaddr = Some(txt),
                        "scopes" => scopes.push(txt),
                        "types" => types.push(txt),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let lname = local_name(e.name().as_ref()).to_ascii_lowercase();
                if lname == "probematch" {
                    in_match = false;
                }
                if lname == "proberesponse" || lname == "probematches" {
                    in_proberesp = false;
                }
                capture = false;
                current_local.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                warn!(err = %e, "xml parse failed");
                return None;
            }
            _ => {}
        }
        buf.clear();
    }

    let xaddr = match current_xaddr {
        Some(v) => v,
        None => scan_url(body)?,
    };
    let primary = xaddr.split_whitespace().next()?.to_string();
    Some(ProbeMatch {
        address: String::new(),
        xaddr: primary,
        scopes,
        types,
    })
}

fn scan_url(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let marker = "http://";
    let pos = text.find(marker)?;
    let rest = &text[pos..];
    let end = rest
        .find(|c: char| ['<', ' ', '"', '\''].contains(&c))
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn default_local_addrs() -> Vec<std::net::IpAddr> {
    let mut out = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            let ip = iface.ip();
            if ip.is_loopback() {
                continue;
            }
            if !ip.is_ipv4() {
                continue;
            }
            out.push(ip);
        }
    }
    if out.is_empty() {
        out.push(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    }
    out
}

async fn fetch_profiles(
    xaddr: &str,
    creds: &DiscoveryCredentials,
    timeout: &Duration,
) -> DiscoveryResult<Vec<VideoProfile>> {
    let endpoint = device_mgmt::parse_xaddr_endpoint(xaddr)?;
    let client = DeviceManagementClient::new(endpoint, creds.clone(), *timeout);
    let profiles = client.list_profiles().await?;
    Ok(profiles)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_probe_basic() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
        <Envelope xmlns="http://www.w3.org/2003/05/soap-envelope">
          <Body>
            <ProbeMatches>
              <ProbeMatch>
                <XAddrs>http://192.168.1.10/onvif/device_service</XAddrs>
                <Scopes>onvif://www.onvif.org/name/Test</Scopes>
                <Types>dn:NetworkVideoTransmitter</Types>
              </ProbeMatch>
            </ProbeMatches>
          </Body>
        </Envelope>"#;
        let m = parse_probe_message(xml.as_bytes()).expect("parse");
        assert!(m.xaddr.contains("192.168.1.10"));
        assert!(!m.scopes.is_empty());
    }

    #[test]
    fn parse_probe_matches_with_s() {
        // Regression: a typo ("probesmatches" instead of "probematches")
        // silently disabled the in_match flag, so no XAddrs/Scopes/Types
        // were ever captured.
        let xml = r#"<Envelope><Body><ProbeMatches><ProbeMatch><XAddrs>http://10.0.0.1/onvif</XAddrs><Scopes>s1</Scopes></ProbeMatch></ProbeMatches></Body></Envelope>"#;
        let m = parse_probe_message(xml.as_bytes()).expect("parse");
        assert_eq!(m.xaddr, "http://10.0.0.1/onvif");
        assert_eq!(m.scopes, vec!["s1"]);
    }

    #[test]
    fn parse_probe_no_match_returns_none() {
        let xml = r#"<?xml version="1.0"?>
        <Envelope><Body><ProbeMatches/></Body></Envelope>"#;
        assert!(parse_probe_message(xml.as_bytes()).is_none());
    }
}
