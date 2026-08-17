use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use ipcam_core::{DiscoveredDevice, SessionId, SessionInfo, SessionState};
use ipcam_discovery::DiscoveryCredentials;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::{info, warn};

pub mod mux;
pub mod stream;

use mux::{AvcConfig, Fmp4Muxer};
use stream::spawn_streaming;

#[derive(Clone)]
pub struct WebDisplay {
    inner: Arc<Inner>,
}

struct Inner {
    bind: String,
    registry: Arc<SessionRegistry>,
    state_tx: broadcast::Sender<SessionStateEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStateEvent {
    pub session_id: SessionId,
    pub state: SessionState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub device_id: uuid::Uuid,
    pub profile_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: SessionId,
}

pub struct SessionRegistry {
    sessions: DashMap<SessionId, SessionEntry>,
    devices: parking_lot::RwLock<Vec<DiscoveredDevice>>,
    credentials: parking_lot::RwLock<Option<DiscoveryCredentials>>,
}

struct SessionEntry {
    info: SessionInfo,
    muxer: Arc<parking_lot::Mutex<Fmp4Muxer>>,
}

impl SessionRegistry {
    pub fn new(credentials: Option<DiscoveryCredentials>) -> Self {
        Self {
            sessions: DashMap::new(),
            devices: parking_lot::RwLock::new(Vec::new()),
            credentials: parking_lot::RwLock::new(credentials),
        }
    }

    pub fn credentials(&self) -> Option<DiscoveryCredentials> {
        self.credentials.read().clone()
    }

    pub fn set_devices(&self, devices: Vec<DiscoveredDevice>) {
        *self.devices.write() = devices;
    }

    pub fn list_devices(&self) -> Vec<DiscoveredDevice> {
        self.devices.read().clone()
    }

    pub fn find_device(&self, id: uuid::Uuid) -> Option<DiscoveredDevice> {
        self.devices.read().iter().find(|d| d.id == id).cloned()
    }

    pub fn create_session(&self, device_id: uuid::Uuid, profile_id: String) -> Option<SessionId> {
        let session_id = SessionId::new_v4();
        let info = SessionInfo {
            session_id,
            device_id,
            profile_id,
            state: SessionState::Creating,
            created_at: chrono::Utc::now(),
        };
        self.sessions.insert(
            session_id,
            SessionEntry {
                info,
                muxer: Arc::new(parking_lot::Mutex::new(Fmp4Muxer::new())),
            },
        );
        Some(session_id)
    }

    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.sessions.iter().map(|kv| kv.info.clone()).collect()
    }

    pub fn get_session(&self, session_id: SessionId) -> Option<SessionInfo> {
        self.sessions.get(&session_id).map(|kv| kv.info.clone())
    }

    pub fn push_packet(&self, session_id: SessionId, pkt: bytes::Bytes) {
        if let Some(entry) = self.sessions.get(&session_id) {
            entry.muxer.lock().push_packet(pkt);
        }
    }

    pub fn configure_muxer(&self, session_id: SessionId, cfg: AvcConfig) {
        if let Some(entry) = self.sessions.get(&session_id) {
            entry.muxer.lock().set_avc_config(cfg);
        }
    }

    pub fn init_segment_for(&self, session_id: SessionId) -> Option<bytes::Bytes> {
        let entry = self.sessions.get(&session_id)?;
        let mux = entry.muxer.lock();
        if mux.is_ready() {
            Some(mux.make_init_segment())
        } else {
            None
        }
    }

    pub fn take_segments_since(&self, session_id: SessionId, since: usize) -> Vec<bytes::Bytes> {
        let Some(entry) = self.sessions.get(&session_id) else {
            return Vec::new();
        };
        entry.muxer.lock().take_segments_since(since)
    }

    pub fn muxer(&self, session_id: SessionId) -> Option<Arc<parking_lot::Mutex<Fmp4Muxer>>> {
        self.sessions.get(&session_id).map(|kv| kv.muxer.clone())
    }

    pub fn remove(&self, session_id: SessionId) -> Option<SessionInfo> {
        self.sessions.remove(&session_id).map(|(_, mut e)| {
            e.info.state = SessionState::Ended;
            e.info.clone()
        })
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new(None)
    }
}

impl WebDisplay {
    pub async fn start(
        bind: &str,
        discovery_timeout: Duration,
        credentials: Option<ipcam_discovery::DiscoveryCredentials>,
        manual_devices: Vec<ipcam_core::DiscoveredDevice>,
    ) -> anyhow::Result<Self> {
        let registry = Arc::new(SessionRegistry::new(credentials.clone()));
        let (state_tx, _rx) = broadcast::channel(64);

        let config = ipcam_discovery::DiscoveryConfig {
            timeout: discovery_timeout,
            credentials,
            ..Default::default()
        };
        let mut devices = ipcam_discovery::probe_all_with_config(config).await;
        // Manual RTSP entries (xaddr=None) are prepended so they show up
        // first in the device list and don't get filtered by ONVIF auth
        // failures on the discovered devices.
        let mut all = manual_devices;
        all.append(&mut devices);
        registry.set_devices(all);

        let inner = Arc::new(Inner {
            bind: bind.to_string(),
            registry,
            state_tx,
        });
        let app = build_router(inner.clone());

        let listener = TcpListener::bind(bind)
            .await
            .with_context(|| format!("bind {}", bind))?;
        let local_addr = listener.local_addr()?;
        info!(addr = %local_addr, "web display server listening");

        let me = WebDisplay {
            inner: inner.clone(),
        };
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(err = %e, "axum serve error");
            }
        });
        Ok(me)
    }

    pub fn local_addr(&self) -> String {
        self.inner.bind.clone()
    }

    pub async fn wait_for_shutdown(&self) -> anyhow::Result<()> {
        let ctrl_c = async {
            tokio::signal::ctrl_c().await.ok();
        };
        let term = async {
            #[cfg(unix)]
            {
                let mut s =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
                if let Some(sig) = s.as_mut() {
                    sig.recv().await;
                }
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! { _ = ctrl_c => {}, _ = term => {} }
        info!("shutdown signal received");
        Ok(())
    }
}

fn build_router(inner: Arc<Inner>) -> Router {
    Router::new()
        .route("/api/devices", get(list_devices))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/:id", get(get_session).delete(delete_session))
        .route("/ws/:id", get(ws_handler))
        .route("/", get(index_page))
        .route("/index.html", get(index_page))
        .route("/play.js", get(play_js))
        .with_state(inner)
}

async fn list_devices(State(inner): State<Arc<Inner>>) -> impl IntoResponse {
    Json(inner.registry.list_devices())
}

async fn list_sessions(State(inner): State<Arc<Inner>>) -> impl IntoResponse {
    Json(inner.registry.list_sessions())
}

async fn create_session(
    State(inner): State<Arc<Inner>>,
    Json(req): Json<CreateSessionRequest>,
) -> Response {
    let device = match inner.registry.find_device(req.device_id) {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "device not found").into_response(),
    };
    let profile = match device
        .profiles
        .iter()
        .find(|p| p.profile_id == req.profile_id)
    {
        Some(p) => p.clone(),
        None => return (StatusCode::NOT_FOUND, "profile not found").into_response(),
    };
    let session_id = match inner
        .registry
        .create_session(req.device_id, req.profile_id.clone())
    {
        Some(sid) => sid,
        None => return (StatusCode::BAD_REQUEST, "could not create session").into_response(),
    };

    // Reject if the device has neither an ONVIF xaddr nor a profile.uri
    // (manual RTSP path). Without one of these we cannot resolve an RTSP URL.
    if device.xaddr.is_none() && profile.uri.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "device has no ONVIF xaddr and profile has no RTSP uri",
        )
            .into_response();
    }

    let mux = match inner.registry.muxer(session_id) {
        Some(m) => m,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "muxer missing").into_response(),
    };

    spawn_streaming(
        session_id,
        device.xaddr.clone(),
        profile.profile_id,
        profile.uri,
        profile.width,
        profile.height,
        inner.registry.credentials(),
        mux,
        inner.state_tx.clone(),
    );

    (
        StatusCode::CREATED,
        Json(CreateSessionResponse { session_id }),
    )
        .into_response()
}

async fn get_session(State(inner): State<Arc<Inner>>, Path(id): Path<SessionId>) -> Response {
    match inner.registry.get_session(id) {
        Some(info) => Json(info).into_response(),
        None => (StatusCode::NOT_FOUND, "session not found").into_response(),
    }
}

async fn delete_session(State(inner): State<Arc<Inner>>, Path(id): Path<SessionId>) -> Response {
    match inner.registry.remove(id) {
        Some(info) => Json(info).into_response(),
        None => (StatusCode::NOT_FOUND, "session not found").into_response(),
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(inner): State<Arc<Inner>>,
    Path(id): Path<SessionId>,
) -> Response {
    ws.on_upgrade(move |socket| ws_loop(socket, inner, id))
}

async fn ws_loop(socket: WebSocket, inner: Arc<Inner>, session_id: SessionId) {
    let (mut sender, mut receiver) = socket.split();

    let mut ticker = tokio::time::interval(Duration::from_millis(50));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut init_sent = false;
    let mut last_sent: usize = 0;
    let mut init_wait_logged = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if !init_sent {
                    if let Some(init) = inner.registry.init_segment_for(session_id) {
                        if sender.send(Message::Binary(init)).await.is_err() {
                            warn!("ws send init failed");
                            return;
                        }
                        init_sent = true;
                        info!(%session_id, "init segment sent to ws");
                    } else if !init_wait_logged {
                        info!(%session_id, "ws waiting for muxer config (waiting on SPS/PPS)");
                        init_wait_logged = true;
                    }
                    if tokio::time::Instant::now() > deadline {
                        warn!(%session_id, "init wait deadline exceeded");
                        let _ = sender
                            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                code: axum::extract::ws::close_code::POLICY,
                                reason: "init timeout".into(),
                            })))
                            .await;
                        return;
                    }
                }
                if init_sent {
                    let segments = inner.registry.take_segments_since(session_id, last_sent);
                    for seg in segments {
                        if sender.send(Message::Binary(seg)).await.is_err() {
                            return;
                        }
                        last_sent += 1;
                    }
                }
            }
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        warn!(err = %e, "ws receive error");
                        break;
                    }
                    None => break,
                }
            }
        }
    }
}

async fn index_page() -> impl IntoResponse {
    let body = include_str!("../../../web/index.html");
    ([("content-type", "text/html; charset=utf-8")], body)
}

async fn play_js() -> impl IntoResponse {
    let body = include_str!("../../../web/play.js");
    ([("content-type", "application/javascript")], body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_create_and_list() {
        let r = SessionRegistry::new(None);
        let dev = uuid::Uuid::new_v4();
        let sid = r.create_session(dev, "p1".into()).unwrap();
        let list = r.list_sessions();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, sid);
        assert_eq!(list[0].state, SessionState::Creating);
        assert!(r.remove(sid).is_some());
        assert!(r.list_sessions().is_empty());
    }
}
