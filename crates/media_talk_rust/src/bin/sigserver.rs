//! sigserver: ICE 验证用微型 WS 信令转发 (房间制, 两人配对).
//! 生产形态由 kamailio 取代 (spec 产品化路线), 本服务仅测试期用.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

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

// 房间表: 房号 -> 两个槽位的出站通道 (槽内无对等端时为 None).
type Rooms = Arc<Mutex<HashMap<String, [Option<mpsc::UnboundedSender<Message>>; 2]>>>;

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
    loop {
        let (stream, _) = l.accept().await?;
        let rooms = rooms.clone();
        tokio::spawn(async move {
            match tokio_tungstenite::accept_async(stream).await {
                Ok(ws) => {
                    if let Err(e) = handle(ws, rooms).await {
                        tracing::warn!(?e, "connection handler error");
                    }
                }
                Err(e) => tracing::warn!(?e, "ws handshake failed"),
            }
        });
    }
}

/// 单连接处理: 首帧必须为 join (分配槽位), 之后把文本帧原样转发给同房另一槽.
///
/// # Arguments
///
/// * `ws`: 已完成 HTTP 升级的 WebSocket 连接.
/// * `rooms`: 共享房间表, 函数返回前清理本连接占用的槽位.
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
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let Some(first) = ws.next().await else {
        return Ok(());
    };
    let frame: Frame = match serde_json::from_slice(&first?.into_data()) {
        Ok(f) => f,
        Err(_) => {
            ws.send(Message::text(error_frame("bad json"))).await?;
            return Ok(());
        }
    };
    if frame.kind != "join" || frame.room.is_empty() {
        ws.send(Message::text(error_frame("first frame must be join")))
            .await?;
        return Ok(());
    }

    // 单次加锁内完成查位 + 建通道 + 装槽, 避免两锁之间的 TOCTOU.
    let joined = {
        let mut rooms = rooms.lock().unwrap();
        let room = rooms.entry(frame.room.clone()).or_default();
        if room[0].is_none() || room[1].is_none() {
            let slot = if room[0].is_none() { 0 } else { 1 };
            let (tx, rx) = mpsc::unbounded_channel::<Message>();
            room[slot] = Some(tx);
            Some((slot, rx))
        } else {
            None
        }
    };
    let Some((slot, mut rx)) = joined else {
        ws.send(Message::text(error_frame("room full"))).await?;
        return Ok(());
    };
    ws.send(Message::text(joined_frame(slot as u8))).await?;

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
                tracing::debug!(?e, "ws read ended with error");
                break;
            }
        };
        if msg.is_close() {
            break;
        }
        // 原样转发 JSON 文本帧 (offer/answer/hangup), payload 含换行也不变.
        // 对端可能晚于本连接加入, 所以每次现查槽位而不是 join 时快照.
        let peer = rooms
            .lock()
            .unwrap()
            .get(&frame.room)
            .and_then(|room| room[1 - slot].clone());
        if let Some(peer) = peer {
            let _ = peer.send(msg);
        }
    }
    fwd.abort();

    // 清理槽位; 房间空了就从表里删除.
    let mut rooms = rooms.lock().unwrap();
    if let Some(room) = rooms.get_mut(&frame.room) {
        room[slot] = None;
        if room[0].is_none() && room[1].is_none() {
            rooms.remove(&frame.room);
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
