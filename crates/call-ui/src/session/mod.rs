//! SIP 后台 worker: 独立线程跑自己的 tokio runtime (GUI 主线程被 slint
//! 事件循环占着). 单线模型: 同时只有一路通话, CallSlot 占线时新来电
//! 直接回 486. 信令逻辑改写自 ipcam-sip 的 examples/answer.rs (被叫)
//! 和 examples/call.rs (主叫).
//!
//! 本模块是 worker 主循环与共享配置/类型; 被叫一路在 incoming, 主叫
//! 一路在 outgoing.

mod incoming;
mod outgoing;

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ipcam_gst::{TrackSource, VideoFrameSink};
use ipcam_sip::{SipClient, SipClientConfig, run_dialog_state_loop};
use parking_lot::Mutex;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::{DialogStateReceiver, DialogStateSender};
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip as rsip;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::UiEvent;
use crate::bridge::{UiHandle, WorkerCmd};

/// 被叫侧接收端口 (同 answer.rs, 单路固定即可); pub(crate) 给 media 层起接收器
pub(crate) const AUDIO_RTP_PORT: u16 = 40000;
pub(crate) const VIDEO_RTP_PORT: u16 = 40002;

/// 来电振铃等待用户接听的最长时间, 超时自动拒接, 避免对端挂死在 ringing
const INCOMING_TIMEOUT: Duration = Duration::from_secs(30);

/// SIP 配置: 服务器账号 + 本机媒体源, 由 UI 入口装配
pub struct GuiSipConfig {
    /// SIP 服务器地址 (注册与呼叫都走它)
    pub server: SocketAddr,
    /// SIP 账号 (同时作为注册用户名)
    pub username: String,
    /// 账号密码 (digest 鉴权)
    pub password: String,
    /// 注册有效期 (秒), 续期按 75% 提前
    pub expires: u32,
    /// 强制绑定的本机 IPv4 网卡; None 走 ipcam_sip::local_ipv4() 自动选
    pub local_ip: Option<Ipv4Addr>,
    /// 通话时回传给对端的音频源 (麦克风/文件)
    pub audio_src: TrackSource,
    /// 通话时发给对端的视频源 (相机/文件/RTSP)
    pub video_src: TrackSource,
    /// 视频限宽 (对端解码能力, 超宽源降采样); 0 = 不限
    pub max_width: u32,
}

/// 通话控制: UI 的 Hangup/Accept/Reject 经它进通话任务
enum CallCtl {
    Hangup,
    Accept,
    Reject,
}

/// 当前通话的控制通道, Some 即占线
type CallSlot = Arc<Mutex<Option<mpsc::Sender<CallCtl>>>>;
type UpdateMsg = (rsip::Request, oneshot::Sender<Option<Vec<u8>>>);
/// 进行中的通话路由表: dialog id -> 通话任务的 re-INVITE 通道
type CallRoutes = Arc<Mutex<HashMap<DialogId, mpsc::Sender<UpdateMsg>>>>;
/// prepare_dial 全过返回的通话资源: (被叫 URI, RTP 端口对, 占位 socket 对, 控制通道接收端)
type DialPrep = (
    rsip::Uri,
    (u16, u16),
    (std::net::UdpSocket, std::net::UdpSocket),
    mpsc::Receiver<CallCtl>,
);

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
                // 建不起来 worker 直接结束; 上层 UI 会因命令通道无响应报错
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
    DialogStateSender,
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
async fn run(
    cfg: GuiSipConfig,
    mut cmd_rx: UnboundedReceiver<WorkerCmd>,
    ui: UiHandle,
    tap: VideoFrameSink,
) -> Result<()> {
    let (client, credential, state_sender, state_receiver, local) = setup(&cfg, &ui).await?;

    let routes: CallRoutes = Arc::new(Mutex::new(HashMap::new()));
    let slot: CallSlot = Arc::new(Mutex::new(None));
    let cfg = Arc::new(cfg);

    // 来电入口: 占线直接 486, 否则占槽 + 登记 re-INVITE 路由 + spawn 通话任务
    let on_incoming_call = make_incoming_handler(
        slot.clone(),
        routes.clone(),
        ui.clone(),
        tap.clone(),
        cfg.clone(),
        local,
    );
    let on_update = {
        let routes = routes.clone();
        move |id: DialogId, req: rsip::Request| forward_update(routes.clone(), id, req)
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
    // reg 是否已在 select 里跑完 (跑完再 await 会 double-await panic)
    let mut reg_done = false;
    loop {
        tokio::select! {
            r = &mut reg => {
                // 注册循环退出 = 永久失败 (401/403/404, Err 带原因) 或被取消
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
                    // UI 窗口关了, worker 收尾退出
                    info!("UI command channel closed, worker exiting");
                    break;
                };
                handle_cmd(cmd, &client, &credential, &state_sender, &slot, &ui, &tap, &cfg, local);
            }
        }
    }

    shutdown(&client, reg, reg_done, &slot).await;
    Ok(())
}

/// 收尾: 进行中通话补 BYE (给通话任务留 BYE 窗口), 停 client, 等注销跑完
///
/// # Arguments
///
/// * `client`: SIP 客户端句柄
/// * `reg`: 注册循环 future (已在 select 里跑完则 reg_done 为 true)
/// * `reg_done`: 注册循环是否已完成 (完成再 await 会 double-await panic)
/// * `slot`: 通话槽位 (有通话则先送 Hangup)
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
    slot: &CallSlot,
) {
    info!("stopping");
    // 进行中通话补 BYE: cmd 通道断开直接 stop 会让 runtime drop 掐掉
    // 通话任务, 对端不知道挂断; 先送 Hangup 给通话任务, 留出 BYE 的窗口
    // (best-effort: 拨号 pending 期的 BYE 要等最终响应, 可能盖不住)
    {
        let ctl = slot.lock().clone();
        if let Some(ctl) = ctl {
            let _ = ctl.try_send(CallCtl::Hangup);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    client.stop();
    // 正常退出路径等注册循环把 best-effort 注销 (Expires:0) 跑完
    if !reg_done {
        let _ = reg.await;
    }
}

/// 来电入口工厂: 占线直接 486, 否则占槽 + 登记 re-INVITE 路由 + spawn 通话任务
///
/// # Arguments
///
/// * `slot`: 通话槽位
/// * `routes`: 进行中通话路由表
/// * `ui`: 事件回投句柄
/// * `tap`: 视频帧回调
/// * `cfg`: SIP 配置
/// * `local`: 本机 IPv4
///
/// returns: impl Fn(InviteDialog) + Send + Sync — run_dialog_state_loop 的来电回调
///
/// # Examples
///
/// ```
///
/// ```
fn make_incoming_handler(
    slot: CallSlot,
    routes: CallRoutes,
    ui: UiHandle,
    tap: VideoFrameSink,
    cfg: Arc<GuiSipConfig>,
    local: Ipv4Addr,
) -> impl Fn(InviteDialog) + Send + Sync {
    move |dialog: InviteDialog| {
        let caller = caller_from_request(&dialog.initial_request());
        {
            let mut guard = slot.lock();
            if guard.is_some() {
                warn!(%caller, "busy, rejecting new incoming call with 486");
                drop(guard);
                if let Err(e) = dialog.reject(Some(rsip::StatusCode::BusyHere), None) {
                    warn!(error = %e, "486 reject failed");
                }
                return;
            }
            let (ctl_tx, ctl_rx) = mpsc::channel(4);
            *guard = Some(ctl_tx);
            let (update_tx, update_rx) = mpsc::channel(4);
            routes.lock().insert(dialog.id(), update_tx);

            // Fn 闭包不能 move 捕获变量, 先 clone 再 move clone 进 async
            let cfg = cfg.clone();
            let ui = ui.clone();
            let tap = tap.clone();
            let slot = slot.clone();
            let routes = routes.clone();
            tokio::spawn(async move {
                incoming::handle_incoming(
                    dialog,
                    incoming::IncomingCtx {
                        cfg,
                        ui,
                        tap,
                        local,
                        ctl_rx,
                        update_rx,
                    },
                    slot,
                    routes,
                )
                .await;
            });
        }
    }
}

/// UI 命令处理: Dial 占线检查 + spawn 主叫任务; 通话三键转发进 ctl 通道.
///
/// # Arguments
///
/// * `cmd`: UI 命令 (Dial/Hangup/Accept/Reject)
/// * `client`: SIP 客户端句柄
/// * `credential`: Digest 鉴权凭据
/// * `state_sender`: Dialog 状态发送通道
/// * `slot`: 当前通话控制槽
/// * `ui`: 事件回投句柄
/// * `tap`: 视频帧回调
/// * `cfg`: SIP 配置 (服务器/账号/媒体源)
/// * `local`: 本机 IPv4
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
// reason: 命令处理要触达 worker 全部上下文 (client/槽位/媒体源), 打包成
// 结构体只有这一个调用点, 反而掩盖各参数的实际用途
#[allow(clippy::too_many_arguments)]
fn handle_cmd(
    cmd: WorkerCmd,
    client: &SipClient,
    credential: &Credential,
    state_sender: &DialogStateSender,
    slot: &CallSlot,
    ui: &UiHandle,
    tap: &VideoFrameSink,
    cfg: &Arc<GuiSipConfig>,
    local: Ipv4Addr,
) {
    match cmd {
        WorkerCmd::Dial(number) => {
            let Some((callee, rtp_ports, rtp_sockets, ctl_rx)) =
                prepare_dial(client, &number, local, slot, ui)
            else {
                return;
            };
            tokio::spawn(outgoing::handle_outgoing(
                client.dialog_layer.clone(),
                state_sender.clone(),
                callee,
                client.contact.clone(),
                credential.clone(),
                number,
                local,
                rtp_ports,
                rtp_sockets,
                cfg.clone(),
                ui.clone(),
                tap.clone(),
                ctl_rx,
                slot.clone(),
            ));
        }
        WorkerCmd::Hangup | WorkerCmd::Accept | WorkerCmd::Reject => {
            let ctl = slot.lock().clone();
            let Some(ctl) = ctl else {
                // 无通话: 命令迟到 (比如挂断后界面又点了一次), 忽略
                return;
            };
            let msg = match cmd {
                WorkerCmd::Hangup => CallCtl::Hangup,
                WorkerCmd::Accept => CallCtl::Accept,
                WorkerCmd::Reject => CallCtl::Reject,
                WorkerCmd::Dial(_) => unreachable!(),
            };
            if let Err(e) = ctl.try_send(msg) {
                warn!(error = %e, "call control channel full or closed");
            }
        }
    }
}

/// Dial 前置: 占线检查 + URI 解析 + 占位 socket 绑定, 全过才占槽返回
/// 通话资源; 任一步失败已报 UI, 返回 None
///
/// # Arguments
///
/// * `client`: SIP 客户端句柄
/// * `number`: 拨号号码
/// * `local`: 本机 IPv4
/// * `slot`: 通话槽位 (全过时在此占槽)
/// * `ui`: 事件回投句柄 (失败原因上报)
///
/// returns: Option<(被叫 URI, RTP 端口对, 占位 socket 对, 控制通道接收端)>
///
/// # Examples
///
/// ```
///
/// ```
fn prepare_dial(
    client: &SipClient,
    number: &str,
    local: Ipv4Addr,
    slot: &CallSlot,
    ui: &UiHandle,
) -> Option<DialPrep> {
    let mut guard = slot.lock();
    if guard.is_some() {
        drop(guard);
        // UI 只会在 Idle 拨号, 走到这里是和来电抢了槽
        ui.post(UiEvent::Failed {
            reason: "占线".into(),
        });
        return None;
    }
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
    // 占位 socket 保端口 (同 call.rs): INVITE 完成前收到 RTP 不会
    // ICMP unreachable; 绑失败属于立即失败, 直接报 UI 不占槽
    let rtp_sockets = match (
        std::net::UdpSocket::bind((local, 0)),
        std::net::UdpSocket::bind((local, 0)),
    ) {
        (Ok(a), Ok(v)) => (a, v),
        _ => {
            ui.post(UiEvent::Failed {
                reason: "RTP 端口绑定失败".into(),
            });
            return None;
        }
    };
    let rtp_ports = match (rtp_sockets.0.local_addr(), rtp_sockets.1.local_addr()) {
        (Ok(a), Ok(v)) => (a.port(), v.port()),
        _ => {
            ui.post(UiEvent::Failed {
                reason: "RTP 端口读取失败".into(),
            });
            return None;
        }
    };
    let (ctl_tx, ctl_rx) = mpsc::channel(4);
    *guard = Some(ctl_tx);
    drop(guard);
    Some((callee, rtp_ports, rtp_sockets, ctl_rx))
}

/// re-INVITE/UPDATE 转发 (原样搬自 answer.rs): 按 dialog id 找通话任务
/// 要 answer SDP; 找不到或 2s 没应答就回不带 SDP 的 200.
async fn forward_update(routes: CallRoutes, id: DialogId, req: rsip::Request) -> Option<Vec<u8>> {
    let tx = routes.lock().get(&id).cloned();
    let Some(tx) = tx else {
        // 主叫 dialog 不登记路由 (本端媒体不变), 同 call.rs 回 200 不带 SDP
        warn!(dialog = %id, "re-INVITE for unknown dialog, reply 200 without SDP");
        return None;
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if tx.send((req, reply_tx)).await.is_err() {
        warn!(dialog = %id, "call task gone, reply 200 without SDP");
        return None;
    }
    match tokio::time::timeout(Duration::from_secs(2), reply_rx).await {
        Ok(Ok(body)) => body,
        _ => {
            warn!(dialog = %id, "call task did not answer re-INVITE in time");
            None
        }
    }
}

/// 从 INVITE 的 From 头提取主叫号码 (sip:<user>@host 的 user 段)
fn caller_from_request(req: &rsip::Request) -> String {
    use rsipstack::sip::HeadersExt;
    req.from_header()
        .ok()
        .and_then(|h| h.uri().ok())
        .and_then(|u| u.auth.map(|a| a.user))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造带指定 From 头的 INVITE 请求
    fn invite_with_from(from: &str) -> rsip::Request {
        let mut headers = rsip::Headers::default();
        headers.push(rsip::Header::From(from.into()));
        rsip::Request {
            method: rsip::Method::Invite,
            uri: "sip:9998@192.168.11.17:5062".try_into().expect("合法 URI"),
            version: rsip::Version::V2,
            headers,
            body: vec![],
        }
    }

    #[test]
    fn caller_id_from_from_header() {
        let req = invite_with_from("<sip:1001@192.168.11.17:5062>;tag=abc");
        assert_eq!(caller_from_request(&req), "1001");
    }

    #[test]
    fn caller_id_with_display_name() {
        let req = invite_with_from("\"门口机\" <sip:door-01@192.168.11.17>;tag=xyz");
        assert_eq!(caller_from_request(&req), "door-01");
    }

    #[test]
    fn caller_id_missing_from_is_unknown() {
        let req = rsip::Request {
            method: rsip::Method::Invite,
            uri: "sip:9998@192.168.11.17:5062".try_into().expect("合法 URI"),
            version: rsip::Version::V2,
            headers: Default::default(),
            body: vec![],
        };
        assert_eq!(caller_from_request(&req), "unknown");
    }
}
