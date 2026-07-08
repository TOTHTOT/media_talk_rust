use std::time::Duration;

use ipcam_core::{VideoCodec, VideoProfile};
use thiserror::Error;
use tracing::debug;
#[cfg(test)]
use uuid::Uuid;

use crate::DiscoveryCredentials;
use crate::soap::SoapEnvelope;
use crate::ws_security::WsSecurityToken;

#[derive(Debug, Error)]
pub enum DeviceMgmtError {
    #[error("http error: {0}")]
    Http(String),
    #[error("xml parse error: {0}")]
    Xml(String),
    #[error("invalid xaddr: {0}")]
    InvalidXaddr(String),
    #[error("auth failed")]
    AuthFailed,
    #[error("transport closed")]
    TransportClosed,
}

pub fn parse_xaddr_endpoint(xaddr: &str) -> Result<reqwest::Url, DeviceMgmtError> {
    let primary = xaddr.split_whitespace().next().unwrap_or(xaddr);
    primary
        .parse::<reqwest::Url>()
        .map_err(|e| DeviceMgmtError::InvalidXaddr(format!("{}: {}", xaddr, e)))
}

pub struct DeviceManagementClient {
    base: reqwest::Url,
    credentials: DiscoveryCredentials,
    timeout: Duration,
}

impl DeviceManagementClient {
    pub fn new(base: reqwest::Url, credentials: DiscoveryCredentials, timeout: Duration) -> Self {
        Self {
            base,
            credentials,
            timeout,
        }
    }

    pub async fn list_profiles(&self) -> Result<Vec<VideoProfile>, DeviceMgmtError> {
        let action = "http://www.onvif.org/ver10/media/wsdl/GetProfiles";
        let body = r#"<GetProfiles xmlns="http://www.onvif.org/ver10/media/wsdl"/>"#.to_string();
        let token = WsSecurityToken::build(&self.credentials);
        let envelope = SoapEnvelope::new(action, body).with_header(token.to_xml_header());
        let url = self.base.clone();
        let xml = self.send_envelope(&url, &envelope, action).await?;
        Ok(parse_profiles(&xml))
    }

    pub async fn get_stream_uri(&self, profile_token: &str) -> Result<String, DeviceMgmtError> {
        let action = "http://www.onvif.org/ver10/media/wsdl/GetStreamUri";
        let body = format!(
            r#"<GetStreamUri xmlns="http://www.onvif.org/ver10/media/wsdl"><StreamSetup><Transport><Protocol>RTSP</Protocol></Transport></StreamSetup><ProfileToken>{}</ProfileToken></GetStreamUri>"#,
            xml_escape(profile_token),
        );
        let token = WsSecurityToken::build(&self.credentials);
        let envelope = SoapEnvelope::new(action, body).with_header(token.to_xml_header());
        let url = self.base.clone();
        let xml = self.send_envelope(&url, &envelope, action).await?;
        extract_stream_uri(&xml).ok_or_else(|| DeviceMgmtError::Xml("no StreamUri in reply".into()))
    }

    pub async fn get_capabilities(&self) -> Result<String, DeviceMgmtError> {
        let action = "http://www.onvif.org/ver10/device/wsdl/GetCapabilities";
        let body = r#"<GetCapabilities xmlns="http://www.onvif.org/ver10/device/wsdl"><Category>All</Category></GetCapabilities>"#;
        let token = WsSecurityToken::build(&self.credentials);
        let envelope = SoapEnvelope::new(action, body).with_header(token.to_xml_header());
        let url = self.base.clone();
        self.send_envelope(&url, &envelope, action).await
    }

    async fn send_envelope(
        &self,
        url: &reqwest::Url,
        envelope: &SoapEnvelope,
        action: &str,
    ) -> Result<String, DeviceMgmtError> {
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| DeviceMgmtError::Http(e.to_string()))?;
        let req = client
            .post(url.clone())
            .header("Content-Type", SoapEnvelope::content_type())
            .header("SOAPAction", action)
            .body(envelope.render().to_vec());

        let resp = req
            .send()
            .await
            .map_err(|e| DeviceMgmtError::Http(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| DeviceMgmtError::Http(e.to_string()))?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(DeviceMgmtError::AuthFailed);
        }
        if !status.is_success() {
            debug!(status = %status, body = %&body.chars().take(256).collect::<String>(), "device mgmt non-2xx");
            if body.contains("NotAuthorized") || body.contains("Authentication") {
                return Err(DeviceMgmtError::AuthFailed);
            }
        }
        Ok(body)
    }
}

fn parse_profiles(xml: &str) -> Vec<VideoProfile> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut profiles: Vec<VideoProfile> = Vec::new();
    let mut current: Option<VideoProfile> = None;
    let mut capture_text: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "Profiles" {
                    let mut pid = String::new();
                    for attr in e.attributes().flatten() {
                        let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                        if key == "token" {
                            pid = attr.unescape_value().unwrap_or_default().to_string();
                        }
                    }
                    current = Some(VideoProfile {
                        profile_id: pid,
                        uri: None,
                        codec: VideoCodec::Unknown,
                        width: 0,
                        height: 0,
                        fps: 0.0,
                    });
                }
                if matches!(
                    name.as_str(),
                    "Width" | "Height" | "FrameRateLimit" | "Encoding"
                ) {
                    capture_text = Some(String::new());
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(buf) = capture_text.as_mut() {
                    let v = t.unescape().unwrap_or_default().to_string();
                    buf.push_str(&v);
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let text = capture_text.take();
                if name == "Profiles" {
                    if let Some(p) = current.take() {
                        profiles.push(p);
                    }
                } else if let (Some(text), Some(p)) = (text, current.as_mut()) {
                    match name.as_str() {
                        "Width" => p.width = text.trim().parse().unwrap_or(0),
                        "Height" => p.height = text.trim().parse().unwrap_or(0),
                        "FrameRateLimit" => p.fps = text.trim().parse().unwrap_or(0.0),
                        "token" => p.profile_id = text.trim().to_string(),
                        "Encoding" => p.codec = VideoCodec::from_name(text.trim()),
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                debug!(err = %e, "profiles parse failed");
                return profiles;
            }
            _ => {}
        }
        buf.clear();
    }
    if profiles.is_empty() {
        profiles.push(VideoProfile {
            profile_id: "default".into(),
            uri: None,
            codec: VideoCodec::Unknown,
            width: 0,
            height: 0,
            fps: 0.0,
        });
    }
    profiles
}

fn extract_stream_uri(xml: &str) -> Option<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_uri = false;
    let mut capture = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "Uri" {
                    in_uri = true;
                    capture.clear();
                }
            }
            Ok(Event::Text(t)) if in_uri => {
                capture.push_str(&t.unescape().unwrap_or_default());
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "Uri" {
                    return Some(capture.trim().to_string());
                }
                in_uri = false;
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
    None
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profiles_xml() {
        let xml = r#"<?xml version="1.0"?>
        <Envelope><Body>
          <GetProfilesResponse>
            <Profiles token="p1">
              <VideoEncoderConfiguration>
                <Encoding>H264</Encoding>
                <Resolution><Width>1920</Width><Height>1080</Height></Resolution>
                <RateControl><FrameRateLimit>25</FrameRateLimit></RateControl>
              </VideoEncoderConfiguration>
            </Profiles>
            <Profiles token="p2">
              <VideoEncoderConfiguration>
                <Encoding>H265</Encoding>
                <Resolution><Width>1280</Width><Height>720</Height></Resolution>
                <RateControl><FrameRateLimit>15</FrameRateLimit></RateControl>
              </VideoEncoderConfiguration>
            </Profiles>
          </GetProfilesResponse>
        </Body></Envelope>"#;
        let p = parse_profiles(xml);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].profile_id, "p1");
        assert_eq!(p[0].width, 1920);
        assert_eq!(p[1].codec, VideoCodec::H265);
    }

    #[test]
    fn extracts_stream_uri() {
        let xml = r#"<?xml version="1.0"?>
        <Envelope><Body>
          <GetStreamUriResponse><MediaUri><Uri>rtsp://192.168.1.10/Streaming/Tracks/101</Uri></MediaUri></GetStreamUriResponse>
        </Body></Envelope>"#;
        let uri = extract_stream_uri(xml).unwrap();
        assert!(uri.starts_with("rtsp://"));
    }

    #[test]
    fn known_digest_format() {
        let creds = DiscoveryCredentials::new("admin", "test_pwd");
        let t = WsSecurityToken::build(&creds);
        let _id = Uuid::new_v4();
        assert!(t.password_digest_b64.len() >= 24);
    }
}
