//! sigserver WS 信令客户端: join / 槽位 / 角色判定 + blob 交换.
//!
//! 协议与 `media_talk_rust/src/bin/sigserver.rs` 同构; `Frame` 与帧收发
//! 集中在这里, call-ui 的 ICE 控制器和 `ice_probe` 共用, 避免两套协议
//! 解析逻辑各自漂移.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Message;

use crate::blob::IceBlob;

/// WS 建连超时 (局域网验证工具, 不无限等).
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 等 joined / 单帧的通用超时.
const WS_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
/// offerer 等 answer 的超时. sigserver 会补发缓存 offer, 但旧版服务器
/// 仍可能丢掉晚加入前的 offer, 因此保留超时兜底.
const ANSWER_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// call-ui 的 SDP offer 帧类型.
pub const OFFER: &str = "offer";
/// call-ui 的 SDP answer 帧类型.
pub const ANSWER: &str = "answer";
/// ice_probe 的 SDP blob 帧类型 (双向同 kind).
pub const BLOB: &str = "blob";

/// 本端作为 WS 客户端的连接类型 (sigserver 明文 ws, MaybeTls 兼容 wss).
pub type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// sigserver 线协议帧. 字段与 `media_talk_rust/src/bin/sigserver.rs` 对齐.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Frame {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub room: String,
    #[serde(default)]
    pub payload: String,
    #[serde(default)]
    pub detail: String,
    /// joined 帧携带的槽位 (0=offerer, 1=answerer); 其余帧为 None.
    #[serde(default)]
    pub slot: Option<u8>,
}

/// WS 信令错误.
#[derive(Debug, thiserror::Error)]
pub enum SignalError {
    #[error("signaling server connection failed: {0}")]
    Ws(String),
    #[error("signaling exchange failed: {0}")]
    Sig(String),
}

/// sigserver 槽位角色判定: slot 0 = offerer (controlling).
pub fn is_offerer(slot: u8) -> bool {
    slot == 0
}

/// 连接 sigserver 并 join 房间, 阻塞到收到 joined 为止 (拿槽位).
///
/// # Arguments
///
/// * `sigserver`: sigserver WS 地址 (ws:// 或 wss://)
/// * `room`: 房间名
///
/// returns: Result<(WsClient, u8), SignalError> — WS 连接与槽位
pub async fn ws_connect_join(sigserver: &str, room: &str) -> Result<(WsClient, u8), SignalError> {
    let (mut ws, _) = tokio::time::timeout(
        WS_CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async(sigserver),
    )
    .await
    .map_err(|_| SignalError::Ws(format!("connecting to {sigserver} timed out")))?
    .map_err(|e| SignalError::Ws(e.to_string()))?;
    send_frame(
        &mut ws,
        Frame {
            kind: "join".into(),
            room: room.to_string(),
            ..Default::default()
        },
    )
    .await?;
    let frame = recv_frame(&mut ws, "waiting for join confirmation", WS_FRAME_TIMEOUT).await?;
    match frame.kind.as_str() {
        "joined" => {
            let slot = frame
                .slot
                .filter(|slot| *slot <= 1)
                .ok_or_else(|| SignalError::Sig("joined frame missing valid slot".into()))?;
            Ok((ws, slot))
        }
        "error" => Err(SignalError::Sig(format!(
            "signaling server rejected: {}",
            frame.detail
        ))),
        other => Err(SignalError::Sig(format!(
            "unexpected frame while waiting for joined: {other}"
        ))),
    }
}

/// 角色化交换对端 blob.
///
/// offerer (slot 0) 先发本地 blob 再等对端帧; answerer 先等对端帧再回本地
/// blob. `offer_kind`/`answer_kind` 由调用方选择: call-ui 用
/// [`OFFER`]/[`ANSWER`], ice_probe 双向用 [`BLOB`].
///
/// 返回对端 blob; WS 连接随本函数结束关闭.
pub async fn ws_exchange(
    ws: WsClient,
    slot: u8,
    local: &IceBlob,
    offer_kind: &str,
    answer_kind: &str,
) -> Result<IceBlob, SignalError> {
    let mut ws = ws;
    if is_offerer(slot) {
        send_frame(
            &mut ws,
            Frame {
                kind: offer_kind.into(),
                payload: local.as_str().to_string(),
                ..Default::default()
            },
        )
        .await?;
        loop {
            let frame = recv_frame(
                &mut ws,
                "waiting for peer answer (peer may not have joined yet)",
                ANSWER_WAIT_TIMEOUT,
            )
            .await?;
            match frame.kind.as_str() {
                kind if kind == answer_kind => return Ok(IceBlob::from_sdp(frame.payload)),
                "hangup" => return Err(SignalError::Sig("peer hung up".into())),
                "error" => {
                    return Err(SignalError::Sig(format!(
                        "signaling error: {}",
                        frame.detail
                    )));
                }
                _ => continue,
            }
        }
    }

    let remote = loop {
        let frame = recv_frame(&mut ws, "waiting for peer offer", WS_FRAME_TIMEOUT).await?;
        match frame.kind.as_str() {
            kind if kind == offer_kind => break frame.payload,
            "hangup" => return Err(SignalError::Sig("peer hung up".into())),
            "error" => {
                return Err(SignalError::Sig(format!(
                    "signaling error: {}",
                    frame.detail
                )));
            }
            _ => continue,
        }
    };
    send_frame(
        &mut ws,
        Frame {
            kind: answer_kind.into(),
            payload: local.as_str().to_string(),
            ..Default::default()
        },
    )
    .await?;
    Ok(IceBlob::from_sdp(remote))
}

/// 发送一帧 JSON 文本.
pub async fn send_frame(ws: &mut WsClient, frame: Frame) -> Result<(), SignalError> {
    let text = serde_json::to_string(&frame).map_err(|e| SignalError::Sig(e.to_string()))?;
    ws.send(Message::text(text))
        .await
        .map_err(|e| SignalError::Ws(e.to_string()))
}

/// 收一帧文本 JSON (跳过 ping/pong/binary 等非文本帧), 带超时.
pub async fn recv_frame(
    ws: &mut WsClient,
    what: &str,
    timeout: Duration,
) -> Result<Frame, SignalError> {
    let msg = loop {
        let next = tokio::time::timeout(timeout, ws.next())
            .await
            .map_err(|_| SignalError::Sig(format!("{what} timed out")))?;
        match next {
            Some(Ok(msg)) if msg.is_text() => break msg,
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(SignalError::Ws(e.to_string())),
            None => return Err(SignalError::Sig("signaling connection closed".into())),
        }
    };
    serde_json::from_str::<Frame>(msg.to_text().map_err(|e| SignalError::Sig(e.to_string()))?)
        .map_err(|e| SignalError::Sig(format!("failed to parse signaling frame: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sigserver_frame() {
        let f: Frame = serde_json::from_str(r#"{"type":"offer","payload":"v=0\r\n"}"#).unwrap();
        assert_eq!(f.kind, "offer");
        assert_eq!(f.payload, "v=0\r\n");
        assert_eq!(f.slot, None);
    }

    #[test]
    fn parses_joined_frame_slot() {
        let f: Frame = serde_json::from_str(r#"{"type":"joined","slot":1}"#).unwrap();
        assert_eq!(f.slot, Some(1));
        assert!(!is_offerer(f.slot.unwrap()));
    }

    #[test]
    fn decides_offerer_by_slot() {
        assert!(is_offerer(0) && !is_offerer(1));
    }
}
