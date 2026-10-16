//! 通话路由: re-INVITE 转发类型与函数.
//!
//! 提供 re-INVITE 在 dialog state loop 和通话任务之间路由的公共设施.

use rsipstack::dialog::DialogId;
use rsipstack::rsip;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::warn;

/// re-INVITE 请求 + 回复通道.
pub type UpdateMsg = (rsip::Request, oneshot::Sender<Option<Vec<u8>>>);

/// 通话路由表: dialog id -> 通话任务的 re-INVITE 通道.
pub type CallRoutes = Arc<Mutex<HashMap<DialogId, mpsc::Sender<UpdateMsg>>>>;

/// 将 re-INVITE 转发给对应 dialog 的通话任务.
/// 超时 2 秒未收到回复时返回 None, 调用方回不带 SDP 的 200.
///
/// # Arguments
///
/// * `routes`: 通话路由表
/// * `id`: dialog id
/// * `req`: re-INVITE 请求
///
/// returns: Option<Vec<u8>> — 通话任务的 answer SDP body, 超时或 dialog 不存在时 None
pub async fn forward_update(
    routes: CallRoutes,
    id: DialogId,
    req: rsip::Request,
) -> Option<Vec<u8>> {
    let tx = routes.lock().unwrap().get(&id).cloned();
    let Some(tx) = tx else {
        warn!(dialog = %id, "re-INVITE for unknown dialog, reply 200 without SDP");
        return None;
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if tx.send((req, reply_tx)).await.is_err() {
        warn!(dialog = %id, "call task gone, reply 200 without SDP");
        return None;
    }
    match tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx).await {
        Ok(Ok(body)) => body,
        _ => {
            warn!(dialog = %id, "call task did not answer re-INVITE in time");
            None
        }
    }
}
