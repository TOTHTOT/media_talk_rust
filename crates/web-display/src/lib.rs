use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use dashmap::DashMap;
use ipcam_core::{DiscoveredDevice, SessionId, SessionInfo, SessionState};
use ipcam_discovery::DiscoveryCredentials;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::info;

pub mod stream;

use stream::spawn_streaming;

#[derive(Clone)]
pub struct WebDisplay {
    inner: Arc<Inner>,
}

struct Inner {
    bind: String,
    registry: Arc<SessionRegistry>,
    state_tx: broadcast::Sender<SessionStateEvent>,
    /// ALSA device for on-board audio playback (`None` = disabled).
    audio_out: Option<String>,
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
    /// GStreamer session backing this entry; `stop()`ped on removal so
    /// a closed page doesn't leave a camera connection running.
    handle: parking_lot::Mutex<Option<ipcam_gst::GstStreamHandle>>,
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
                handle: parking_lot::Mutex::new(None),
            },
        );
        Some(session_id)
    }

    pub fn set_handle(&self, session_id: SessionId, handle: ipcam_gst::GstStreamHandle) {
        if let Some(entry) = self.sessions.get(&session_id) {
            *entry.handle.lock() = Some(handle);
        }
    }

    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.sessions.iter().map(|kv| kv.info.clone()).collect()
    }

    pub fn get_session(&self, session_id: SessionId) -> Option<SessionInfo> {
        self.sessions.get(&session_id).map(|kv| kv.info.clone())
    }

    pub fn remove(&self, session_id: SessionId) -> Option<SessionInfo> {
        self.sessions.remove(&session_id).map(|(_, mut e)| {
            if let Some(h) = e.handle.lock().take() {
                h.stop();
            }
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
        credentials: Option<DiscoveryCredentials>,
        manual_devices: Vec<DiscoveredDevice>,
        audio_out: Option<String>,
    ) -> anyhow::Result<Self> {
        // All WebRTC sessions register on this one in-process signalling
        // server (ws://<host>:8443); must be up before the first stream.
        ipcam_gst::ensure_signalling_server()
            .context("failed to start WebRTC signalling server")?;

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
            audio_out,
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
        .route(
            "/api/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/", get(index_page))
        .route("/index.html", get(index_page))
        .route("/play.js", get(play_js))
        .route("/gstwebrtc-api.min.js", get(gstwebrtc_api_js))
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

    spawn_streaming(
        session_id,
        device.xaddr.clone(),
        profile.profile_id,
        profile.uri,
        inner.registry.credentials(),
        inner.audio_out.clone(),
        inner.registry.clone(),
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

async fn index_page() -> impl IntoResponse {
    let body = include_str!("../../../web/index.html");
    ([("content-type", "text/html; charset=utf-8")], body)
}

async fn play_js() -> impl IntoResponse {
    let body = include_str!("../../../web/play.js");
    ([("content-type", "application/javascript")], body)
}

async fn gstwebrtc_api_js() -> impl IntoResponse {
    let body = include_str!("../../../web/gstwebrtc-api.min.js");
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
