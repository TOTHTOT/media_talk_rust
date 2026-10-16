//! SIP worker 主循环与命令处理: spawn/setup/run/shutdown/handle_cmd/prepare_dial/forward_update.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ipcam_gst::VideoFrameSink;
use ipcam_sip::{CallRoutes, SipClient, SipClientConfig, forward_update, run_dialog_state_loop};
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::DialogStateReceiver;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip as rsip;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::UiEvent;
use crate::bridge::{UiHandle, WorkerCmd};

use super::incoming;
use super::outgoing;
use super::types::{CallBase, CallCtlTx, DialPrep, GuiSipConfig, caller_from_request};

/// 启动 SIP worker 线程: 独立线程跑自己的 tokio runtime, 注册常驻,
/// UI 命令逐条处理, 事件经 UiHandle 回投主线程.
///
/// # Arguments
///
/// * `cfg`: SIP 服务器账号与本机媒体源配置
/// * `cmd_rx`: UI 命令接收端 (bridge::cmd_channel 的另一半); 通道关闭
///   (UI 窗口已关) 即触发 worker 收尾退出
/// * `ui`: 事件回投句柄, worker 各阶段经它 post UiEvent 更新界面
/// * `tap`: 视频帧回调, 透传给媒体层把对端画面送 GUI 上屏
///
/// returns: anyhow::Result<std::thread::JoinHandle<()>> — worker 线程句柄,
/// UI 退出时 join 它; Err 是线程创建失败
///
/// # Examples
///
/// ```
///
/// ```
pub fn spawn(
    cfg: GuiSipConfig,
    cmd_rx: UnboundedReceiver<WorkerCmd>,
    ui: UiHandle,
    tap: VideoFrameSink,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let worker = move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                warn!(error = %e, "SIP worker: failed to create tokio runtime");
                return;
            }
        };
        if let Err(e) = rt.block_on(run(cfg, cmd_rx, ui, tap)) {
            warn!(error = %e, "SIP worker error, exiting");
        }
    };
    let handle = std::thread::Builder::new()
        .name("sip-worker".into())
        .spawn(worker)
        .map_err(|e| anyhow::anyhow!("SIP worker 线程创建失败: {e}"))?;
    Ok(handle)
}

/// 启动准备: 选网卡 + 构造 SipClient + spawn endpoint + 建 dialog 状态通道;
/// client 构造失败 post RegisterFailed 后返回 Err
///
/// # Arguments
///
/// * `cfg`: SIP 服务器账号与本机媒体源配置
/// * `ui`: 事件回投句柄 (client 构造失败上报)
///
/// returns: Result<(SipClient, Digest 鉴权凭据, dialog 状态发送端, 状态接收端, 本机 IPv4)>
///
/// # Examples
///
/// ```
///
/// ```
async fn setup(
    cfg: &GuiSipConfig,
    ui: &UiHandle,
) -> Result<(
    SipClient,
    Credential,
    rsipstack::dialog::dialog::DialogStateSender,
    DialogStateReceiver,
    Ipv4Addr,
)> {
    let local = match cfg.local_ip {
        Some(ip) => ip,
        None => ipcam_sip::local_ipv4()?,
    };
    let client_addr = SocketAddr::V4(SocketAddrV4::new(local, 0));
    let config = SipClientConfig::new(
        cfg.username.clone(),
        cfg.username.clone(),
        cfg.password.clone(),
        cfg.server,
        client_addr,
        Some(cfg.expires),
    );
    let credential = config.to_credential();
    let client = match SipClient::new(config, CancellationToken::new()).await {
        Ok(c) => c,
        Err(e) => {
            ui.post(UiEvent::RegisterFailed(e.to_string()));
            return Err(e);
        }
    };
    let _endpoint = client.spawn_endpoint();

    let (state_sender, state_receiver) = client.dialog_layer.new_dialog_state_channel();
    Ok((client, credential, state_sender, state_receiver, local))
}

/// worker 主循环: 注册/入向分发/dialog 状态循环常驻, UI 命令逐条处理.
/// 三个常驻 future 只创建一次, loop 里 select 复用 (每次重建
/// incoming_transactions 会丢包, state_receiver 更是 move 语义).
///
/// # Arguments
///
/// * `cfg`: SIP 服务器账号与本机媒体源配置
/// * `cmd_rx`: UI 命令接收端, channel 关闭时 worker 退出
/// * `ui`: 事件回投句柄
/// * `tap`: 视频帧回调
///
/// returns: Result<()> — setup 失败 / 任意常驻循环出错则返回 Err
///
/// # Examples
///
/// ```
///
/// ```
async fn run(
    cfg: GuiSipConfig,
    mut cmd_rx: UnboundedReceiver<WorkerCmd>,
    ui: UiHandle,
    tap: VideoFrameSink,
) -> Result<()> {
    let (client, credential, state_sender, state_receiver, local) = setup(&cfg, &ui).await?;

    let routes: CallRoutes = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let call_ctl_tx: CallCtlTx = Arc::new(parking_lot::Mutex::new(None));
    let cfg = Arc::new(cfg);

    let on_incoming_call = make_incoming_handler(
        call_ctl_tx.clone(),
        routes.clone(),
        ui.clone(),
        tap.clone(),
        cfg.clone(),
        local,
    );
    let on_update = {
        let routes = routes.clone();
        move |id: rsipstack::dialog::DialogId, req: rsip::Request| {
            forward_update(routes.clone(), id, req)
        }
    };

    info!(server = %cfg.server, username = %cfg.username, "SIP worker started");

    let mut reg = Box::pin(client.process_register());
    let mut incoming = Box::pin(client.run_incoming_loop(&state_sender));
    let mut state_loop = Box::pin(run_dialog_state_loop(
        client.dialog_layer.clone(),
        state_receiver,
        on_incoming_call,
        on_update,
    ));
    let mut reg_done = false;
    loop {
        tokio::select! {
            r = &mut reg => {
                warn!(result = ?r, "register loop exited");
                let reason = match r {
                    Err(e) => format!("{e}"),
                    Ok(()) => "注册循环退出".into(),
                };
                ui.post(UiEvent::RegisterFailed(reason));
                reg_done = true;
                break;
            }
            r = &mut incoming => {
                if let Err(e) = r {
                    warn!(error = %e, "incoming request loop error");
                }
                break;
            }
            r = &mut state_loop => {
                if let Err(e) = r {
                    warn!(error = %e, "dialog state loop error");
                }
                break;
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    info!("UI command channel closed, worker exiting");
                    break;
                };
                handle_cmd(cmd, &client, &credential, &state_sender, &call_ctl_tx, &routes, &ui, &tap, &cfg, local);
            }
        }
    }

    shutdown(&client, reg, reg_done, &call_ctl_tx).await;
    Ok(())
}

/// 收尾: 进行中通话补 BYE (给通话任务留 BYE 窗口), 停 client, 等注销跑完
///
/// # Arguments
///
/// * `client`: SIP 客户端句柄
/// * `reg`: 注册循环 future (已在 select 里跑完则 reg_done 为 true)
/// * `reg_done`: 注册循环是否已完成 (完成再 await 会 double-await panic)
/// * `call_ctl_tx`: 通话槽位 (有通话则先送 Hangup)
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
async fn shutdown<'a>(
    client: &'a SipClient,
    reg: std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>,
    reg_done: bool,
    call_ctl_tx: &CallCtlTx,
) {
    info!("stopping");
    {
        let ctl = call_ctl_tx.lock().clone();
        if let Some(ctl) = ctl {
            let _ = ctl.try_send(WorkerCmd::Hangup);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    client.stop();
    if !reg_done {
        let _ = reg.await;
    }
}

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
            routes.lock().unwrap().insert(dialog.id(), update_tx);

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
            tokio::spawn(crate::session::outgoing::handle_outgoing(
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
