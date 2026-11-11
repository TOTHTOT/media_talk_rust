//! sigserver WS 信令客户端: join/槽位/角色化 blob 交换. 协议与 media_talk_rust/src/bin/sigserver.rs 同构, 字段增减必须与它对齐.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use ipcam_ice::blob::IceBlob;
use tokio_tungstenite::tungstenite::Message;

use crate::ice::{Frame, IceCtlError};

/// WS 建连超时 (局域网验证工具, 不给长).
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 等 joined 等单帧的通用超时.
const WS_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
/// offerer 等 answer 的超时: 对端晚加入时 offer 已被丢弃, 只能靠超时解脱.
const ANSWER_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// 本端作为 WS 客户端的连接类型 (sigserver 明文 ws, MaybeTls 兼容 wss).
pub(super) type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// sigserver 槽位角色判定: slot 0 = offerer (controlling).
pub(super) fn is_offerer(slot: u8) -> bool {
    slot == 0
}

/// 连接 sigserver 并 join 房间, 阻塞到收到 joined 帧 (拿槽位).
///
///
///
/// # Arguments
///
/// * `sigserver`: sigserver WS 地址 (ws://…)
/// * `room`: 房间名
///
/// returns: Result<(WsClient, u8), IceCtlError> — WS 连接与槽位
/// (0=offerer, 1=answerer)
///
/// # Examples
///
/// ```
///
/// ```
pub(super) async fn ws_connect_join(
    sigserver: &str,
    room: &str,
) -> Result<(WsClient, u8), IceCtlError> {
    let (mut ws, _) = tokio::time::timeout(
        WS_CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async(sigserver),
    )
    .await
    .map_err(|_| IceCtlError::Ws(format!("连接 {sigserver} 超时")))?
    .map_err(|e| IceCtlError::Ws(e.to_string()))?;
    send_frame(
        &mut ws,
        Frame {
            kind: "join".into(),
            room: room.to_string(),
            ..Default::default()
        },
    )
    .await?;
    let frame = recv_frame(&mut ws, "等待加入房间确认", WS_FRAME_TIMEOUT).await?;
    match frame.kind.as_str() {
        "joined" => {
            let slot = frame
                .slot
                .filter(|s| *s == 0 || *s == 1)
                .ok_or_else(|| IceCtlError::Sig("joined 帧缺槽位".into()))?;
            Ok((ws, slot as u8))
        }
        "error" => Err(IceCtlError::Sig(format!(
            "信令服务器拒绝: {}",
            frame.detail
        ))),
        other => Err(IceCtlError::Sig(format!(
            "等待 joined 时收到意外帧: {other}"
        ))),
    }
}

/// 角色化交换对端 blob: offerer (slot 0) 发本地 blob 再等 answer;
/// answerer 等 offer 后回本地 blob. 返回对端 blob; WS 连接随本函数
/// 结束关闭 (通话期对端 hangup 转发不读, 见 crate::ice 模块文档).
///
///
///
/// # Arguments
///
/// * `ws`: 已 join 的 WS 连接 (消费, 函数结束即关)
/// * `slot`: 本端槽位 (0=offerer)
/// * `local`: 本端 gather 完成的 blob
///
/// returns: Result<IceBlob, IceCtlError> — 对端 blob
///
/// # Examples
///
/// ```
///
/// ```
pub(super) async fn ws_exchange(
    ws: WsClient,
    slot: u8,
    local: &IceBlob,
) -> Result<IceBlob, IceCtlError> {
    let mut ws = ws;
    if is_offerer(slot) {
        send_frame(
            &mut ws,
            Frame {
                kind: "offer".into(),
                payload: local.as_str().to_string(),
                ..Default::default()
            },
        )
        .await?;
        loop {
            // 对端晚加入时 offer 已被服务器丢弃, 只能靠超时解脱.
            let frame = recv_frame(
                &mut ws,
                "等待对端应答 (对端可能尚未加入房间)",
                ANSWER_WAIT_TIMEOUT,
            )
            .await?;
            match frame.kind.as_str() {
                "answer" => return Ok(IceBlob::from_sdp(frame.payload)),
                "hangup" => return Err(IceCtlError::Sig("对端已挂断".into())),
                "error" => return Err(IceCtlError::Sig(format!("信令错误: {}", frame.detail))),
                _ => continue,
            }
        }
    }
    let remote = loop {
        let frame = recv_frame(&mut ws, "等待对端呼叫", WS_FRAME_TIMEOUT).await?;
        match frame.kind.as_str() {
            "offer" => break frame.payload,
            "hangup" => return Err(IceCtlError::Sig("对端已挂断".into())),
            "error" => return Err(IceCtlError::Sig(format!("信令错误: {}", frame.detail))),
            _ => continue,
        }
    };
    send_frame(
        &mut ws,
        Frame {
            kind: "answer".into(),
            payload: local.as_str().to_string(),
            ..Default::default()
        },
    )
    .await?;
    Ok(IceBlob::from_sdp(remote))
}

/// 发送一帧 JSON 文本.
///
///
///
/// # Arguments
///
/// * `ws`: 目标连接
/// * `frame`: 信令帧
///
/// returns: Result<(), IceCtlError> — 序列化/发送失败时 Err
///
/// # Examples
///
/// ```
///
/// ```
async fn send_frame(ws: &mut WsClient, frame: Frame) -> Result<(), IceCtlError> {
    let text = serde_json::to_string(&frame).map_err(|e| IceCtlError::Sig(e.to_string()))?;
    ws.send(Message::text(text))
        .await
        .map_err(|e| IceCtlError::Ws(e.to_string()))
}

/// 收一帧文本 JSON (跳过 ping/pong/binary 等非文本帧), 带超时.
///
///
///
/// # Arguments
///
/// * `ws`: 源连接
/// * `what`: 超时信息里的等待目的 (进错误文案)
/// * `timeout`: 等待上限
///
/// returns: Result<Frame, IceCtlError> — 超时/断连/坏 JSON 时 Err
///
/// # Examples
///
/// ```
///
/// ```
async fn recv_frame(
    ws: &mut WsClient,
    what: &str,
    timeout: Duration,
) -> Result<Frame, IceCtlError> {
    let msg = loop {
        let next = tokio::time::timeout(timeout, ws.next())
            .await
            .map_err(|_| IceCtlError::Sig(format!("{what} 超时")))?;
        match next {
            Some(Ok(m)) if m.is_text() => break m,
            Some(Ok(_)) => continue, // 协议层非文本帧, 跳过
            Some(Err(e)) => return Err(IceCtlError::Ws(e.to_string())),
            None => return Err(IceCtlError::Sig("信令连接已关闭".into())),
        }
    };
    serde_json::from_str::<Frame>(msg.to_text().map_err(|e| IceCtlError::Sig(e.to_string()))?)
        .map_err(|e| IceCtlError::Sig(format!("信令帧解析失败: {e}")))
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
        assert!(!is_offerer(f.slot.unwrap() as u8));
    }

    #[test]
    fn decides_offerer_by_slot() {
        assert!(is_offerer(0) && !is_offerer(1));
    }
}
