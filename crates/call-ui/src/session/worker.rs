//! SIP worker 生命周期与主循环: spawn/setup/run/shutdown — 独立线程跑自己的
//! tokio runtime, 主循环 select 注册状态/入向事务/dialog 状态/UI 命令,
//! 退出时给进行中通话补 BYE 再停 client. 来电与命令的入口分发在 dispatch.rs.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ipcam_gst::VideoFrameSink;
use ipcam_sip::{CallRoutes, SipClient, SipClientConfig, forward_update, run_dialog_state_loop};
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::DialogStateReceiver;
use rsipstack::sip as rsip;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::UiEvent;
use crate::bridge::{UiHandle, WorkerCmd};

use super::dispatch::{handle_cmd, make_incoming_handler};
use super::types::{CallCtlTx, GuiSipConfig};

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
    let mut client_status = client.status();
    let mut status_open = true;
    let mut reg_done = false;
    loop {
        tokio::select! {
            r = client_status.changed(), if status_open => {
                match r {
                    Ok(()) => {
                        if client_status.borrow_and_update().registered {
                            ui.post(UiEvent::Registered);
                        }
                    }
                    Err(e) => {
                        warn!(error=?e, "client status change failed, info");
                        status_open = false
                    },
                }
            }
            r = &mut reg => {
                warn!(result = ?r, "register loop exited");
                let reason = match r {
                    Err(e) => format!("{e}"),
                    // 正常退出无系统错误: detail 留空, 前端只显示 kind 文案
                    Ok(()) => String::new(),
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
