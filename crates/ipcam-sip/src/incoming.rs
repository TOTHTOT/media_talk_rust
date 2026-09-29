//! 来电编排: 事务层分发 (run_incoming_loop) 和 dialog 状态循环
//! (run_dialog_state_loop). 只做 SIP 协议路由, 不碰 SDP/媒体 —— 每通
//! 电话的具体处理由调用方在 on_incoming_call 回调里 spawn 独立任务.

use anyhow::Result;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog::{Dialog, DialogState, DialogStateReceiver, DialogStateSender};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::rsip;
use rsipstack::sip::HeadersExt;
use rsipstack::transaction::key::TransactionRole;
use std::sync::Arc;
use tracing::{info, warn};

use crate::SipClient;

impl SipClient {
    /// 事务层分发循环：只做路由，不碰 SDP/媒体。in-dialog 请求（To 带
    /// tag）交给已跟踪的 dialog；新 INVITE 建 server dialog 并把 handle
    /// 甩进独立任务，生命周期事件走 state_sender 通道（由
    /// [`run_dialog_state_loop`] 接管）；其他新请求（OPTIONS 保活之类）
    /// 直接回 OK。
    pub async fn run_incoming_loop(&self, state_sender: &DialogStateSender) -> Result<()> {
        let mut incoming = self.endpoint.incoming_transactions()?;
        while let Some(mut tx) = incoming.recv().await {
            info!(key = ?tx.key, method = %tx.original.method, "received transaction");

            // to_header().and_then(h.tag()) 返回 Some(None) 时 flatten 后是 None
            let has_to_tag = tx
                .original
                .to_header()
                .ok()
                .and_then(|h| h.tag().ok())
                .flatten()
                .is_some();

            if has_to_tag {
                match self.dialog_layer.match_dialog(&tx) {
                    Some(mut dialog) => {
                        // in-dialog 请求 (re-INVITE/BYE/ACK...) 全部交给
                        // dialog.handle 路由. re-INVITE 会走 handle_reinvite:
                        // 状态切 Updated 并等应用层回 200 (见
                        // run_dialog_state_loop), 别在这里调 accept() —
                        // accept 只认初始 INVITE 的事务通道, 对 re-INVITE
                        // 必报 "channel closed"
                        tokio::spawn(async move {
                            if let Err(e) = dialog.handle(&mut tx).await {
                                warn!(error = %e, "dialog handle failed");
                            }
                        });
                    }
                    None => {
                        info!("dialog not found for in-dialog request");
                        let _ = tx
                            .reply(rsip::StatusCode::CallTransactionDoesNotExist)
                            .await;
                    }
                }
                continue;
            }

            match tx.original.method {
                rsip::Method::Invite => {
                    let mut dialog = match self.dialog_layer.get_or_create_server_invite(
                        &tx,
                        state_sender.clone(),
                        None,
                        Some(self.contact.clone()),
                    ) {
                        Ok(d) => d,
                        Err(e) => {
                            warn!(error = %e, "failed to create server invite dialog");
                            let _ = tx.reply(rsip::StatusCode::BusyHere).await;
                            continue;
                        }
                    };
                    tokio::spawn(async move {
                        if let Err(e) = dialog.handle(&mut tx).await {
                            warn!(error = %e, "dialog handle failed");
                        }
                    });
                }
                _ => {
                    let _ = tx.reply(rsip::StatusCode::OK).await;
                }
            }
        }
        Ok(())
    }
}

/// dialog 状态循环：全部状态打日志；Calling 且本端是 server（被叫）时
/// 回调 on_incoming_call（每通电话在回调里 spawn 独立任务，别阻塞循环）；
/// Updated (re-INVITE/UPDATE) 回调 on_update 拿 answer SDP 并回 200 OK —
/// rsipstack 的 handle_reinvite 会把事务挂起等应用层回复, 没人理就兜底
/// 501, 对端收不到 200 会停媒体甚至拆通话; Terminated 按 rsipstack 文档
/// 要求 remove_dialog，否则 confirmed dialog 永远挂在 registry 里
/// （内存泄漏）。
///
/// `on_update` 回调: 入参是 dialog id 和 re-INVITE/UPDATE 请求 (带新
/// offer), 返回 answer SDP body; None 表示不带 SDP 回 200. 回调是 async
/// 的, 方便应用层转发给正在跑的通话任务做媒体重启后再应答.
///
/// 注意：rsipstack 的状态通道是普通 mpsc（不是广播），dialog 的事件只
/// 发给创建时给的那个 sender，所以必须消费调用方自己建的通道。
pub async fn run_dialog_state_loop<F, G, Fut>(
    dialog_layer: Arc<DialogLayer>,
    mut state_receiver: DialogStateReceiver,
    on_incoming_call: F,
    on_update: G,
) -> Result<()>
where
    F: Fn(InviteDialog) + Send + Sync,
    G: Fn(DialogId, rsip::Request) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Option<Vec<u8>>> + Send,
{
    while let Some(state) = state_receiver.recv().await {
        info!(%state, "dialog state");
        match state {
            DialogState::Calling(id) => {
                let Some(Dialog::Invite(d)) = dialog_layer.get_dialog(&id) else {
                    continue;
                };
                if d.role() == TransactionRole::Server {
                    on_incoming_call(d);
                }
            }
            DialogState::Updated(id, req, handle) => {
                let body = on_update(id.clone(), req).await;
                let headers = body
                    .as_ref()
                    .map(|_| vec![rsip::headers::Header::ContentType("application/sdp".into())]);
                if let Err(e) = handle.respond(rsip::StatusCode::OK, headers, body).await {
                    warn!(dialog = %id, error = %e, "failed to respond to re-INVITE");
                }
            }
            DialogState::Terminated(id, reason) => {
                info!(dialog = %id, ?reason, "dialog terminated");
                dialog_layer.remove_dialog(&id);
            }
            _ => {}
        }
    }
    Ok(())
}
