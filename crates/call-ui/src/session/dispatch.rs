//! 通话入口分发与槽位管理: 来电 (make_incoming_handler) 与 UI 命令
//! (handle_cmd) 统一在这里过占线检查再占 CallCtlTx 槽位, 并登记
//! re-INVITE 路由; 通话三键 (Hangup/Accept/Reject) 经槽位转发给通话任务.

use std::net::Ipv4Addr;
use std::sync::Arc;

use ipcam_gst::VideoFrameSink;
use ipcam_sip::{CallRoutes, SipClient};
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip as rsip;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::app::UiEvent;
use crate::bridge::{UiHandle, WorkerCmd};

use super::incoming;
use super::outgoing;
use super::types::{CallBase, CallCtlTx, DialPrep, GuiSipConfig, caller_from_request};

/// 来电入口工厂: 占线直接 486, 否则占槽 + 登记 re-INVITE 路由 + spawn 通话任务
///
/// ```mermaid
/// sequenceDiagram
///     participant DL as dialog_layer
///     participant RSL as run_dialog_state_loop
///     participant MIH as make_incoming_handler
///     participant HI as handle_incoming
///
///     DL->>+RSL: DialogState::Calling
///     RSL->>+MIH: on_incoming_call(dialog)
///     Note over MIH: 占线检查
///     MIH->>HI: spawn handle_incoming()
///     Note over HI: wait_user_decision
///     alt Accept
///         HI->>DL: dialog.accept(200 OK)
///         HI->>+HI: call_loop()
///     else Reject/Timeout/Cancel
///         HI->>DL: dialog.reject/repo
///     end
/// ```
///
/// # Arguments
///
/// * `call_ctl_tx`: 通话槽位 (占线检查 + 占槽)
/// * `routes`: re-INVITE 路由表 (登记 update_tx)
/// * `ui`: 事件回投句柄, 透传给被叫通话任务
/// * `tap`: 视频帧回调, 透传给被叫通话任务
/// * `cfg`: SIP 服务器账号与本机媒体源配置
/// * `local`: 本机 IPv4 (SDP 与 contact 用)
///
/// returns: impl Fn(InviteDialog) + Send + Sync — 交给
/// run_dialog_state_loop 的 on_incoming_call 回调
///
/// # Examples
///
/// ```
///
/// ```
pub fn make_incoming_handler(
    call_ctl_tx: CallCtlTx,
    routes: CallRoutes,
    ui: UiHandle,
    tap: VideoFrameSink,
    cfg: Arc<GuiSipConfig>,
    local: Ipv4Addr,
) -> impl Fn(InviteDialog) + Send + Sync {
    move |dialog: InviteDialog| {
        let caller = caller_from_request(&dialog.initial_request());
        {
            let mut guard = call_ctl_tx.lock();
            if guard.is_some() {
                warn!(%caller, "busy, rejecting new incoming call with 486");
                drop(guard);
                if let Err(e) = dialog.reject(Some(rsip::StatusCode::BusyHere), None) {
                    warn!(error = %e, "486 reject failed");
                }
                return;
            }
            let (ctl_tx, answer_ctl_rx) = mpsc::channel(4);
            *guard = Some(ctl_tx);
            let (update_tx, update_rx) = mpsc::channel(4);
            routes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(dialog.id(), update_tx);

            let call_ctl_tx = call_ctl_tx.clone();
            let routes = routes.clone();
            let ctx = CallBase {
                cfg: cfg.clone(),
                ui: ui.clone(),
                tap: tap.clone(),
                local,
                call_answer_ctl_rx: answer_ctl_rx,
                update_rx,
            };
            tokio::spawn(async move {
                incoming::handle_incoming(dialog, ctx, call_ctl_tx, routes).await;
            });
        }
    }
}

/// UI 命令处理: Dial 占线检查 + spawn 主叫任务; 通话三键转发进 ctl 通道.
///
/// ```mermaid
/// sequenceDiagram
///     participant UI
///     participant handle_cmd
///     participant CTX[CallCtlTx slot]
///
///     UI->>+handle_cmd: cmd_tx.send(Dial)
///     handle_cmd->>CTX: slot = Some(call_ctl_tx)
///     handle_cmd->>handle_cmd: spawn handle_outgoing()
///
///     UI->>handle_cmd: cmd_tx.send(Hangup)
///     handle_cmd->>CTX: slot.get()
///     CTX-->>handle_cmd: call_ctl_tx
///     handle_cmd->>call_loop: call_ctl_tx.send(Hangup)
/// ```
///
/// # Arguments
///
/// * `cmd`: UI 命令 (Dial/Hangup/Accept/Reject)
/// * `client`: SIP 客户端句柄 (Dial 时取 dialog_layer/contact)
/// * `credential`: Digest 鉴权凭据, 透传给主叫任务
/// * `state_sender`: dialog 状态通道发送端, 透传给主叫任务
/// * `call_ctl_tx`: 通话槽位 (Dial 占槽, 三键转发取槽)
/// * `routes`: re-INVITE 路由表 (Dial 登记 update_tx)
/// * `ui`: 事件回投句柄 (失败原因上报)
/// * `tap`: 视频帧回调, 透传给主叫任务
/// * `cfg`: SIP 服务器账号与本机媒体源配置
/// * `local`: 本机 IPv4 (SDP 与 contact 用)
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
#[allow(clippy::too_many_arguments)]
pub fn handle_cmd(
    cmd: WorkerCmd,
    client: &SipClient,
    credential: &Credential,
    state_sender: &rsipstack::dialog::dialog::DialogStateSender,
    call_ctl_tx: &CallCtlTx,
    routes: &CallRoutes,
    ui: &UiHandle,
    tap: &VideoFrameSink,
    cfg: &Arc<GuiSipConfig>,
    local: Ipv4Addr,
) {
    info!(?cmd, "current deal");
    match cmd {
        // Dial: prepare_dial 占槽后 spawn 主叫任务, 把 call_answer_ctl_rx 传入监听 Hangup
        WorkerCmd::Dial(number) => {
            // prepare_dial: 占线检查 + callee_uri 解析 + 创建 call_answer_ctl_rx 通道
            let Some((callee, call_answer_ctl_rx)) = prepare_dial(client, &number, call_ctl_tx, ui)
            else {
                return;
            };
            // 创建 re-INVITE 路由通道: update_tx 登记进 routes, update_rx 传通话任务
            let (update_tx, update_rx) = mpsc::channel(4);
            tokio::spawn(outgoing::handle_outgoing(
                client.dialog_layer.clone(),
                state_sender.clone(),
                call_ctl_tx.clone(),
                outgoing::OutgoingCtx {
                    callee,
                    contact: client.contact.clone(),
                    credential: credential.clone(),
                    number,
                    routes: routes.clone(),
                    update_tx,
                    base: CallBase {
                        cfg: cfg.clone(),
                        ui: ui.clone(),
                        tap: tap.clone(),
                        local,
                        call_answer_ctl_rx, // 监听 Hangup
                        update_rx,          // 监听 re-INVITE
                    },
                },
            ));
        }
        // Hangup/Accept/Reject: 从 slot 取出 call_ctl_tx, 转发命令给通话任务
        WorkerCmd::Hangup | WorkerCmd::Accept | WorkerCmd::Reject => {
            let ctl = call_ctl_tx.lock().clone();
            let Some(ctl) = ctl else {
                warn!("call_ctl_tx is none, no ongoing call");
                return;
            };
            if let Err(e) = ctl.try_send(cmd) {
                warn!(error = %e, "call control channel full or closed");
            }
        }
    }
}

/// Dial 前置: 占线检查 + URI 解析, 全过才占槽返回通话资源;
/// 任一步失败已报 UI, 返回 None
///
/// # Arguments
///
/// * `client`: SIP 客户端句柄
/// * `number`: 拨号号码
/// * `call_ctl_tx`: 通话槽位 (全过时在此占槽: 把 ctl_tx 存入 Arc<Mutex>)
/// * `ui`: 事件回投句柄 (失败原因上报)
///
/// returns: Option<DialPrep> — Some 时已占槽, caller 负责 spawn 通话任务
///
/// # Examples
///
/// ```
///
/// ```
fn prepare_dial(
    client: &SipClient,
    number: &str,
    call_ctl_tx: &CallCtlTx,
    ui: &UiHandle,
) -> Option<DialPrep> {
    // 占线检查: slot 有值说明有通话或来电响铃中, 拒绝新拨号
    let mut guard = call_ctl_tx.lock();
    if guard.is_some() {
        drop(guard);
        ui.post(UiEvent::Failed {
            reason: "占线".into(),
        });
        return None;
    }
    // 解析被叫 URI: 用户输入可能是 "1001" 或 "192.168.1.100"
    let callee = match client.callee_uri(number) {
        Ok(u) => u,
        Err(e) => {
            warn!(error = %e, %number, "callee URI parse failed");
            ui.post(UiEvent::Failed {
                reason: format!("号码无效: {number}"),
            });
            return None;
        }
    };
    // 创建控制通道: ctl_tx 存入 slot 供 UI 发 Hangup, ctl_rx 传给通话任务
    let (call_ctl_tx_to_send, call_answer_ctl_rx) = mpsc::channel(4);
    *guard = Some(call_ctl_tx_to_send); // 占槽
    drop(guard);
    Some((callee, call_answer_ctl_rx))
}
