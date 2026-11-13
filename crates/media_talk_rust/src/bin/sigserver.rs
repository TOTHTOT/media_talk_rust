//! sigserver: ICE 验证用微型 WS 信令转发 (房间制, 两人配对).
//! 生产形态由 kamailio 取代 (spec 产品化路线), 本服务仅测试期用.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};

#[derive(Debug, Serialize, Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    room: String,
    #[serde(default)]
    payload: String,
    #[serde(default)]
    detail: String,
}

// 房间表: 房号 -> 两个槽位 + 晚加入补发的缓存.
#[derive(Default)]
struct Room {
    /// 槽位 0/1 的出站通道 (无对等端时为 None).
    peers: [Option<mpsc::UnboundedSender<Message>>; 2],
    /// slot 0 (offerer) 最后一条初始 SDP (offer/blob). answerer 晚加入时
    /// join 后立刻补发, 避免 offer 在它进房前被丢弃.
    cached_offer: Option<Message>,
}
type Rooms = Arc<Mutex<HashMap<String, Room>>>;

/// 服务入口: 接受连接, 按房间两两配对转发 offer/answer/hangup.
///
/// # Arguments
///
/// * `l`: 已绑定的 tokio TcpListener (测试绑 127.0.0.1:0; main 绑 CLI 端口).
///
/// returns: Result<(), std::io::Error>
///
/// # Examples
///
/// ```
///
/// ```
async fn serve(l: TcpListener) -> std::io::Result<()> {
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));
    // 当前 TCP 连接数, 只用于日志; 判断"远程设备到底有没有连上来"时
    // active=0 → 包没到本机; active=1 但没有 join → 到了但没发 join.
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        let (stream, peer) = l.accept().await?;
        let now = active.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(%peer, active = now, "tcp accepted");
        let rooms = rooms.clone();
        let active = active.clone();
        tokio::spawn(async move {
            match accept_hdr_async(stream, LogHandshake { peer }).await {
                Ok(ws) => {
                    tracing::info!(%peer, "ws handshake ok");
                    if let Err(e) = handle(ws, rooms, peer).await {
                        tracing::warn!(%peer, error = %e, "connection handler error");
                    }
                }
                Err(e) => tracing::warn!(%peer, error = %e, "ws handshake failed"),
            }
            let left = active.fetch_sub(1, Ordering::Relaxed) - 1;
            tracing::info!(%peer, active = left, "connection gone");
        });
    }
}

/// WS 升级回调: 把握手请求里的来源信息打进日志, 便于确认远程设备是谁
/// (NAT/反代后面 `peer` 只剩网关地址, 靠 UA / X-Forwarded-For 区分).
struct LogHandshake {
    peer: SocketAddr,
}

impl Callback for LogHandshake {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
        };
        tracing::info!(
            peer = %self.peer,
            method = %request.method(),
            path = %request.uri(),
            user_agent = header("user-agent"),
            origin = header("origin"),
            forwarded_for = header("x-forwarded-for"),
            "ws upgrade request"
        );
        Ok(response)
    }
}

/// 单连接处理: 首帧必须为 join (分配槽位), 之后把文本帧原样转发给同房另一槽.
///
/// # Arguments
///
/// * `ws`: 已完成 HTTP 升级的 WebSocket 连接.
/// * `rooms`: 共享房间表, 函数返回前清理本连接占用的槽位.
/// * `peer`: 对端地址 (仅用于日志; NAT 后是网关地址).
///
/// returns: Result<(), tokio_tungstenite::tungstenite::Error>
///
/// # Examples
///
/// ```
///
/// ```
async fn handle(
    mut ws: WebSocketStream<TcpStream>,
    rooms: Rooms,
    peer: SocketAddr,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let Some(first) = ws.next().await else {
        tracing::info!(%peer, "peer closed before first frame");
        return Ok(());
    };
    let frame: Frame = match serde_json::from_slice(&first?.into_data()) {
        Ok(f) => f,
        Err(_) => {
            tracing::warn!(%peer, "first frame is not valid json");
            ws.send(Message::text(error_frame("bad json"))).await?;
            return Ok(());
        }
    };
    if frame.kind != "join" || frame.room.is_empty() {
        tracing::warn!(
            %peer,
            kind = %frame.kind,
            room = %frame.room,
            "first frame must be join with room"
        );
        ws.send(Message::text(error_frame("first frame must be join")))
            .await?;
        return Ok(());
    }

    // 单次加锁内完成查位 + 建通道 + 装槽, 避免两锁之间的 TOCTOU.
    let joined = {
        let mut rooms = rooms.lock().unwrap();
        let room = rooms.entry(frame.room.clone()).or_default();
        if room.peers[0].is_none() || room.peers[1].is_none() {
            let slot = if room.peers[0].is_none() { 0 } else { 1 };
            let (tx, rx) = mpsc::unbounded_channel::<Message>();
            room.peers[slot] = Some(tx);
            let cached_offer = if slot == 1 {
                room.cached_offer.clone()
            } else {
                None
            };
            let occupied =
                usize::from(room.peers[0].is_some()) + usize::from(room.peers[1].is_some());
            Some((slot, rx, cached_offer, occupied))
        } else {
            None
        }
    };
    let Some((slot, mut rx, cached_offer, occupied)) = joined else {
        tracing::warn!(%peer, room = %frame.room, "room full, rejecting");
        ws.send(Message::text(error_frame("room full"))).await?;
        return Ok(());
    };
    let room = frame.room.clone();
    tracing::info!(
        %peer,
        %room,
        slot,
        peers = occupied,
        cached_offer = cached_offer.is_some(),
        "room joined"
    );
    ws.send(Message::text(joined_frame(slot as u8))).await?;
    if let Some(cached) = cached_offer {
        // joined 必须先到, 客户端拿到槽位后才能把 offer 当对端 blob 处理.
        tracing::info!(%peer, %room, slot, "replay cached offer to late peer");
        ws.send(cached).await?;
    }

    let (mut ws_tx, mut ws_rx) = ws.split();
    let fwd = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });
    while let Some(msg) = ws_rx.next().await {
        // 读错误 (如 TCP reset) 按关闭处理, 统一走下方的清理.
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(%peer, %room, slot, error = %e, "ws read ended with error");
                break;
            }
        };
        if msg.is_close() {
            tracing::info!(%peer, %room, slot, "peer sent close frame");
            break;
        }
        // 原样转发 JSON 文本帧 (offer/answer/hangup), payload 含换行也不变.
        // slot 0 的初始 SDP 同时写入缓存, 供晚加入的 slot 1 补发.
        let text = msg.to_text().ok();
        let parsed = text.and_then(|text| serde_json::from_str::<Frame>(text).ok());
        let kind = parsed.as_ref().map(|frame| frame.kind.as_str());
        let payload_len = parsed.as_ref().map_or(0, |frame| frame.payload.len());
        let peer_slot = 1 - slot;
        let (peer_tx, cached) = {
            let mut rooms = rooms.lock().unwrap();
            let Some(room) = rooms.get_mut(&frame.room) else {
                tracing::warn!(%peer, room = %frame.room, slot, "room gone while forwarding");
                break;
            };
            let mut cached = false;
            if slot == 0 && matches!(kind, Some("offer" | "blob")) {
                room.cached_offer = Some(msg.clone());
                cached = true;
            } else if matches!(kind, Some("hangup")) {
                room.cached_offer = None;
            }
            (room.peers[peer_slot].clone(), cached)
        };
        tracing::info!(
            %peer,
            room = %frame.room,
            from_slot = slot,
            to_slot = peer_slot,
            kind = kind.unwrap_or("?"),
            payload_len,
            cached,
            delivered = peer_tx.is_some(),
            "frame relayed"
        );
        if let Some(peer_tx) = peer_tx {
            let _ = peer_tx.send(msg);
        }
    }
    fwd.abort();

    // 清理槽位; offerer (slot 0) 离开时清掉缓存, 避免给新 answerer 发过期 SDP.
    // 房间空了就从表里删除.
    let mut rooms = rooms.lock().unwrap();
    if let Some(entry) = rooms.get_mut(&room) {
        entry.peers[slot] = None;
        if slot == 0 {
            entry.cached_offer = None;
        }
        if entry.peers[0].is_none() && entry.peers[1].is_none() {
            rooms.remove(&room);
            tracing::info!(%peer, %room, slot, "room removed (empty)");
        } else {
            tracing::info!(%peer, %room, slot, "slot released, waiting for peer");
        }
    }
    Ok(())
}

fn joined_frame(slot: u8) -> String {
    serde_json::json!({"type": "joined", "slot": slot}).to_string()
}

fn error_frame(detail: &str) -> String {
    serde_json::json!({"type": "error", "detail": detail}).to_string()
}

// 流程: 1. 解析 --bind; 2. 初始化 tracing; 3. 建 tokio runtime 并 serve.
fn main() -> anyhow::Result<()> {
    let bind = std::env::args()
        .position(|a| a == "--bind")
        .and_then(|i| std::env::args().nth(i + 1))
        .unwrap_or_else(|| "0.0.0.0:9000".into());
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let listener = TcpListener::bind(&bind).await?;
        tracing::info!(%bind, "sigserver listening");
        serve(listener).await
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::connect_async;

    type Client = WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

    fn join(room: &str) -> Message {
        Message::text(serde_json::json!({"type": "join", "room": room}).to_string())
    }

    fn text_msg(kind: &str, payload: &str) -> Message {
        Message::text(serde_json::json!({"type": kind, "payload": payload}).to_string())
    }

    // 起真实服务: std listener 占端口后立即释放再让 tokio 绑定 (最多重试 5 次).
    async fn spawn_server() -> u16 {
        for _ in 0..5 {
            let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = std_listener.local_addr().unwrap().port();
            drop(std_listener);
            if let Ok(l) = TcpListener::bind(("127.0.0.1", port)).await {
                tokio::spawn(async move { serve(l).await.unwrap() });
                return port;
            }
        }
        panic!("no ephemeral port available after 5 retries");
    }

    async fn connect(port: u16) -> Client {
        connect_async(format!("ws://127.0.0.1:{port}"))
            .await
            .unwrap()
            .0
    }

    async fn recv(client: &mut Client) -> Frame {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), client.next())
            .await
            .expect("timed out waiting for a frame")
            .unwrap()
            .unwrap();
        serde_json::from_str(msg.to_text().unwrap()).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relays_offer_between_two_peers() {
        let port = spawn_server().await;
        let mut c1 = connect(port).await;
        let mut c2 = connect(port).await;
        c1.send(join("room-a")).await.unwrap();
        c2.send(join("room-a")).await.unwrap();
        assert_eq!(recv(&mut c1).await.kind, "joined");
        assert_eq!(recv(&mut c2).await.kind, "joined");

        // payload 含换行, 验证转发逐字节保留.
        c1.send(text_msg("offer", "sdp-A\nline2")).await.unwrap();
        let frame = recv(&mut c2).await;
        assert_eq!(frame.kind, "offer");
        assert_eq!(frame.payload, "sdp-A\nline2");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replays_cached_offer_to_late_peer() {
        let port = spawn_server().await;
        let mut c1 = connect(port).await;
        c1.send(join("room-cache")).await.unwrap();
        assert_eq!(recv(&mut c1).await.kind, "joined");
        // offerer 在 answerer 进房前就把 blob 发出来.
        c1.send(text_msg("blob", "sdp-cached")).await.unwrap();
        // 给服务端读循环一点时间写入缓存 (无对端时不会回显).
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let mut c2 = connect(port).await;
        c2.send(join("room-cache")).await.unwrap();
        assert_eq!(recv(&mut c2).await.kind, "joined");
        let cached = recv(&mut c2).await;
        assert_eq!(cached.kind, "blob");
        assert_eq!(cached.payload, "sdp-cached");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_third_peer() {
        let port = spawn_server().await;
        let mut c1 = connect(port).await;
        let mut c2 = connect(port).await;
        c1.send(join("room-b")).await.unwrap();
        c2.send(join("room-b")).await.unwrap();
        assert_eq!(recv(&mut c1).await.kind, "joined");
        assert_eq!(recv(&mut c2).await.kind, "joined");

        let mut c3 = connect(port).await;
        c3.send(join("room-b")).await.unwrap();
        let err = recv(&mut c3).await;
        assert_eq!(err.kind, "error");
        assert_eq!(err.detail, "room full");
        // 服务端随后关闭该连接.
        let follow_up = tokio::time::timeout(std::time::Duration::from_secs(5), c3.next())
            .await
            .expect("timed out waiting for connection close");
        assert!(
            follow_up.is_none()
                || follow_up.as_ref().unwrap().is_err()
                || follow_up.as_ref().unwrap().as_ref().unwrap().is_close()
        );

        // 前两人通道不受影响: hangup 仍可转发.
        c1.send(text_msg("hangup", "")).await.unwrap();
        assert_eq!(recv(&mut c2).await.kind, "hangup");
    }
}
