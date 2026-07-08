//! RTSP client + RTP/H.264 demux for ONVIF IP cameras.
//!
//! The RTSP transport is delegated to the third-party
//! [`rtsp_runtime`] crate (sans-IO engine + tokio adapter). This
//! module is just a thin wrapper that:
//!
//! * Parses the `rtsp://[user[:pwd]@]host[:port]/path` URL into a TCP
//!   endpoint plus a request-line URI (userinfo is stripped — LIVE555
//!   otherwise tries Basic auth using the embedded password and
//!   short-circuits the Digest flow).
//! * Drives `OPTIONS → DESCRIBE → SETUP(*) → PLAY` and surfaces the
//!   parsed SDP via [`RtspSessionInfo`].
//! * Pumps interleaved RTP frames through [`rtp::H264Depacketizer`]
//!   and hands each Annex-B NAL unit to a user callback as
//!   [`EncodedPacket`] (H.264 only; other payloads are skipped).
//!
//! Auth (Basic + Digest) is handled transparently by rtsp-runtime on
//! a `401` — callers just pass credentials via
//! [`RtspConfig::with_credentials`].

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use ipcam_core::{AudioCodec, CoreError, CoreResult, EncodedPacket, VideoCodec};
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use rtsp_runtime::{
    AsyncRtspClient, ClientSession, Credentials, LowerTransport, Transport, TransportSpec,
};

pub mod rtp;
pub mod sdp;

use rtp::{H264Depacketizer, parse_rtp_header};
use sdp::SdpSession;

#[derive(Debug, Error)]
pub enum RtspError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("rtsp parse: {0}")]
    Parse(String),
    #[error("rtsp error {status}: {body}")]
    Status { status: u16, body: String },
    #[error("codec unsupported: {0}")]
    CodecUnsupported(String),
    #[error("timeout")]
    Timeout,
    #[error("transport closed")]
    TransportClosed,
    #[error("rtsp-runtime: {0}")]
    Runtime(String),
}

pub type RtspResult<T> = Result<T, RtspError>;

impl From<RtspError> for CoreError {
    fn from(e: RtspError) -> Self {
        match e {
            RtspError::Io(io) => CoreError::Io(io),
            RtspError::CodecUnsupported(c) => CoreError::CodecUnsupported(c),
            RtspError::TransportClosed => CoreError::TransportClosed,
            RtspError::Timeout => CoreError::Timeout(Some(Duration::from_secs(0))),
            other => CoreError::Other(other.to_string()),
        }
    }
}

impl From<rtsp_runtime::Error> for RtspError {
    fn from(e: rtsp_runtime::Error) -> Self {
        match e {
            rtsp_runtime::Error::Io(s) => RtspError::Runtime(format!("io: {s}")),
            rtsp_runtime::Error::Auth(s) => RtspError::Runtime(format!("auth: {s}")),
            other => RtspError::Runtime(other.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RtspConfig {
    pub uri: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub keepalive_interval: Duration,
    pub connect_timeout: Duration,
}

impl RtspConfig {
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            username: None,
            password: None,
            keepalive_interval: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
        }
    }

    pub fn with_credentials(mut self, u: impl Into<String>, p: impl Into<String>) -> Self {
        self.username = Some(u.into());
        self.password = Some(p.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct RtpTrack {
    pub control: String,
    pub payload_type: u8,
    pub codec: VideoCodec,
    pub audio_codec: AudioCodec,
    pub clock_rate: u32,
}

#[derive(Debug, Clone)]
pub struct RtspSessionInfo {
    pub session_id: Option<String>,
    pub tracks: Vec<RtpTrack>,
    pub video_codec: VideoCodec,
    pub audio_codec: AudioCodec,
    pub sdp: SdpSession,
}

/// RTSP client built on top of `rtsp_runtime::AsyncRtspClient`.
///
/// The connect flow completes `OPTIONS → DESCRIBE → SETUP(*) → PLAY`
/// before returning. Callers then drive media with
/// [`RtspClient::play_loop`] until the server closes the socket.
pub struct RtspClient {
    cfg: RtspConfig,
    state: Mutex<Option<Connected>>,
}

struct Connected {
    session: AsyncRtspClient<tokio::net::TcpStream>,
    #[allow(dead_code)]
    info: RtspSessionInfo,
}

impl RtspClient {
    pub fn new(cfg: RtspConfig) -> Self {
        Self {
            cfg,
            state: Mutex::new(None),
        }
    }

    pub async fn connect(&self) -> RtspResult<RtspSessionInfo> {
        let (host, port, base_uri) = parse_rtsp_uri(&self.cfg.uri)?;

        // DNS lookup is bounded by connect_timeout so a misbehaving network
        // doesn't strand the session forever.
        let addr: SocketAddr = tokio::time::timeout(
            self.cfg.connect_timeout,
            tokio::net::lookup_host((host.as_str(), port)),
        )
        .await
        .map_err(|_| RtspError::Timeout)?
        .map_err(RtspError::Io)?
        .next()
        .ok_or_else(|| RtspError::Parse(format!("no address for {host}")))?;
        tracing::info!(addr = %addr, base = %base_uri, "rtsp tcp connect");

        let mut session = ClientSession::new().with_user_agent("mediatalk-media/0.1");
        if let (Some(u), Some(p)) = (&self.cfg.username, &self.cfg.password) {
            session = session.with_credentials(Credentials::new(u.clone(), p.clone()));
        }
        let mut client = AsyncRtspClient::connect_with(addr, session)
            .await
            .map_err(RtspError::from)?;

        // OPTIONS — most cheap cameras don't actually require it, but it
        // primes rtsp-runtime's session id handling and gives a clean 401
        // early if credentials are wrong.
        match client.options(&base_uri).await.map_err(RtspError::from)? {
            rtsp_runtime::ClientEvent::Response { status, .. } if !status.is_success() => {
                warn!(?status, "OPTIONS did not return 2xx");
            }
            _ => {}
        }

        // DESCRIBE — pulls the SDP body that drives SETUP.
        let sdp_body = match client.describe(&base_uri).await.map_err(RtspError::from)? {
            rtsp_runtime::ClientEvent::Response { status, body, .. } => {
                if !status.is_success() {
                    return Err(RtspError::Status {
                        status: u16::from(status),
                        body: String::from_utf8_lossy(&body).into_owned(),
                    });
                }
                body
            }
            other => {
                return Err(RtspError::Parse(format!(
                    "unexpected DESCRIBE event: {other:?}"
                )));
            }
        };
        let sdp_text = String::from_utf8_lossy(&sdp_body).into_owned();
        let sdp = sdp::parse_sdp(&sdp_text).map_err(|e| RtspError::Parse(e.to_string()))?;
        let video_codec = sdp.video_codec().unwrap_or(VideoCodec::Unknown);
        let audio_codec = sdp.audio_codec().unwrap_or(AudioCodec::Unknown);

        let transport = Transport::single(TransportSpec {
            lower_transport: Some(LowerTransport::Tcp),
            delivery: Some(rtsp_runtime::Delivery::Unicast),
            interleaved: Some((0, 1)),
            mode: Some("play".to_string()),
            ..Default::default()
        });

        let mut tracks: Vec<RtpTrack> = Vec::new();
        let mut session_id: Option<String> = None;

        for m in &sdp.media {
            let control = m
                .attributes
                .iter()
                .find(|(k, _)| k == "control")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let track_url = if control.starts_with("rtsp://") {
                control.clone()
            } else {
                let trimmed_base = base_uri.trim_end_matches('/');
                let trimmed_ctrl = control.trim_start_matches('/');
                format!("{trimmed_base}/{trimmed_ctrl}")
            };

            let pt = m.payload_types.first().copied().unwrap_or(96);
            let track_codec = m.video_codec().unwrap_or(VideoCodec::Unknown);
            let track_audio = m.audio_codec().unwrap_or(AudioCodec::Unknown);
            let clock_rate = m.clock_rate.unwrap_or(90000);

            match client
                .setup(&track_url, &transport)
                .await
                .map_err(RtspError::from)?
            {
                rtsp_runtime::ClientEvent::Response { status, body, .. } => {
                    let code = u16::from(status);
                    if code != 200 {
                        return Err(RtspError::Status {
                            status: code,
                            body: String::from_utf8_lossy(&body).into_owned(),
                        });
                    }
                }
                other => {
                    return Err(RtspError::Parse(format!(
                        "unexpected SETUP event: {other:?}"
                    )));
                }
            }
            if session_id.is_none() {
                session_id = client.session_id().map(|s| s.to_string());
            }
            tracks.push(RtpTrack {
                control,
                payload_type: pt,
                codec: track_codec,
                audio_codec: track_audio,
                clock_rate,
            });
        }

        match client.play(&base_uri).await.map_err(RtspError::from)? {
            rtsp_runtime::ClientEvent::Response { status, body, .. } => {
                let code = u16::from(status);
                if code != 200 {
                    return Err(RtspError::Status {
                        status: code,
                        body: String::from_utf8_lossy(&body).into_owned(),
                    });
                }
            }
            other => {
                return Err(RtspError::Parse(format!(
                    "unexpected PLAY event: {other:?}"
                )));
            }
        }

        let info = RtspSessionInfo {
            session_id,
            tracks,
            video_codec,
            audio_codec,
            sdp,
        };
        *self.state.lock().await = Some(Connected {
            session: client,
            info: info.clone(),
        });
        Ok(info)
    }

    pub async fn play_loop<V, A>(&self, mut on_video: V, mut on_audio: A) -> RtspResult<()>
    where
        V: FnMut(EncodedPacket) -> CoreResult<()>,
        A: FnMut(EncodedPacket) -> CoreResult<()>,
    {
        let mut h264 = H264Depacketizer::new();
        loop {
            let event = {
                let mut guard = self.state.lock().await;
                let conn = match guard.as_mut() {
                    Some(c) => c,
                    None => return Err(RtspError::TransportClosed),
                };
                match conn
                    .session
                    .recv_interleaved()
                    .await
                    .map_err(RtspError::from)?
                {
                    Some(e) => e,
                    None => return Ok(()), // peer closed cleanly
                }
            };
            let (channel, payload) = match event {
                rtsp_runtime::ClientEvent::MediaData { channel, data } => (channel, data),
                other => {
                    debug!(event = ?other, "non-media event during play; ignored");
                    continue;
                }
            };

            if channel & 0x01 == 1 {
                // RTCP on odd channels — drop.
                continue;
            }
            let packet = Bytes::from(payload);
            let rtp = match parse_rtp_header(&packet) {
                Some(h) => h,
                None => {
                    debug!(len = packet.len(), "non-RTP interleaved packet, skipped");
                    continue;
                }
            };
            if !(96..=127).contains(&rtp.payload_type) {
                continue;
            }
            let nalus = h264.push(&packet[rtp.payload_offset..]);
            let arrival_us = ipcam_core::now_micros();
            for nalu in nalus {
                if nalu.len() < 5 {
                    continue;
                }
                // H264Depacketizer emits NAL units WITH the Annex-B start
                // code prefix (00 00 00 01), so the NAL type byte is at
                // index 4.
                let nal_type = nalu[4] & 0x1F;
                let is_keyframe = nal_type == 5;
                let pkt = EncodedPacket {
                    codec: VideoCodec::H264,
                    data: nalu,
                    rtp_ts: rtp.timestamp,
                    arrival_us,
                    is_keyframe,
                    marker: rtp.marker,
                };
                if let Err(e) = on_video(pkt) {
                    warn!(err = %e, "video sink rejected packet");
                }
            }
            let _ = &mut on_audio;
        }
    }

    pub async fn teardown(&self) -> RtspResult<()> {
        let mut guard = self.state.lock().await;
        if let Some(mut conn) = guard.take() {
            let uri = self.cfg.uri.clone();
            let _ = conn.session.teardown(&uri).await;
        }
        Ok(())
    }
}

/// Parse `rtsp://[user[:pwd]@]host[:port]/path` into the TCP endpoint
/// and a clean request-line URI.
///
/// The returned request URI **omits** any embedded userinfo: LIVE555
/// and similar servers interpret `rtsp://user:pw@host/path` as a
/// Basic auth attempt using the userinfo, which short-circuits the
/// Digest flow that rtsp-runtime manages. Credentials are forwarded
/// separately via the Authorization header.
fn parse_rtsp_uri(uri: &str) -> RtspResult<(String, u16, String)> {
    let stripped = uri
        .strip_prefix("rtsp://")
        .ok_or_else(|| RtspError::Parse("not rtsp".into()))?;
    let (authority, path) = match stripped.find('/') {
        Some(i) => (&stripped[..i], &stripped[i..]),
        None => (stripped, "/"),
    };
    let host_port = match authority.find('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(554)),
        None => (host_port.to_string(), 554),
    };
    let base = format!("rtsp://{}/{}", host_port, path.trim_start_matches('/'));
    Ok((host, port, base))
}

pub async fn connect_and_describe(uri: &str) -> RtspResult<SdpSession> {
    let cfg = RtspConfig::new(uri);
    let client = RtspClient::new(cfg);
    let info = client.connect().await?;
    Ok(info.sdp)
}

pub mod prelude {
    pub use super::{
        RtpTrack, RtspClient, RtspConfig, RtspError, RtspSessionInfo, connect_and_describe,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rtsp_uri_basic() {
        let (host, port, base) =
            parse_rtsp_uri("rtsp://192.168.1.10/Streaming/Tracks/101").unwrap();
        assert_eq!(host, "192.168.1.10");
        assert_eq!(port, 554);
        assert!(base.contains("192.168.1.10"));
    }

    #[test]
    fn parses_rtsp_uri_strips_userinfo() {
        let (host, port, base) = parse_rtsp_uri("rtsp://admin:pwd@10.0.0.1:8554/stream").unwrap();
        assert_eq!(host, "10.0.0.1");
        assert_eq!(port, 8554);
        // request-line URI must not contain userinfo
        assert!(!base.contains("admin"), "base leaked userinfo: {base}");
        assert!(!base.contains("pwd"), "base leaked password: {base}");
        assert!(base.starts_with("rtsp://10.0.0.1:8554/"));
    }

    #[test]
    fn parses_rtsp_uri_default_port() {
        let (_, port, _) = parse_rtsp_uri("rtsp://h/p").unwrap();
        assert_eq!(port, 554);
    }
}
