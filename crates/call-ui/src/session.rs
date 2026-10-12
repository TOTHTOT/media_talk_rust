//! SIP 后台 worker: 独立线程跑自己的 tokio runtime (GUI 主线程被 slint
//! 事件循环占着). 单线模型: 同时只有一路通话, CallSlot 占线时新来电
//! 直接回 486. 信令逻辑改写自 ipcam-sip 的 examples/answer.rs (被叫)
//! 和 examples/call.rs (主叫).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use ipcam_core::{AudioCodec, VideoCodec};
use ipcam_gst::{TrackSource, VideoFrameSink};
use ipcam_sip::sdp::{
    OfferMedias, PT_PCMA, PT_PCMU, build_av_answer, build_av_offer, parse_answer_all,
    parse_offer_all,
};
use ipcam_sip::{SipClient, SipClientConfig, run_dialog_state_loop};
use parking_lot::Mutex;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::DialogStateSender;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip as rsip;
use rsipstack::sip::headers::Header;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::UiEvent;
use crate::bridge::{UiHandle, WorkerCmd};
use crate::media;

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

/// 启动 SIP worker 线程: 独立线程跑自己的 tokio runtime, 注册常驻,
/// UI 命令逐条处理, 事件经 UiHandle 回投主线程. 内部任何失败 (runtime
/// 创建/run 出错/worker panic 由线程边界兜住) 只记日志, 不拖垮 GUI 进程;
/// 线程创建失败时返回一个立即退出的占位线程, 保持签名不 panic.
///
/// # Arguments
///
/// * `cfg`: SIP 服务器账号与本机媒体源配置
/// * `cmd_rx`: UI 命令接收端 (bridge::cmd_channel 的另一半); 通道关闭
///   (UI 窗口已关) 即触发 worker 收尾退出
/// * `ui`: 事件回投句柄, worker 各阶段经它 post UiEvent 更新界面
/// * `tap`: 视频帧回调, 透传给媒体层把对端画面送 GUI 上屏
///
/// returns: std::thread::JoinHandle<()> — worker 线程句柄, UI 退出时 join 它
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
) -> std::thread::JoinHandle<()> {
    let worker = move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                warn!(error = %e, "SIP worker: 创建 tokio runtime 失败");
                return;
            }
        };
        if let Err(e) = rt.block_on(run(cfg, cmd_rx, ui, tap)) {
            warn!(error = %e, "SIP worker 出错退出");
        }
    };
    match std::thread::Builder::new()
        .name("sip-worker".into())
        .spawn(worker)
    {
        Ok(h) => h,
        Err(e) => {
            // 保持签名不变: 返回一个立即退出的线程, UI 侧发命令会失败并提示
            warn!(error = %e, "SIP worker 线程创建失败");
            std::thread::spawn(|| {})
        }
    }
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
    let routes: CallRoutes = Arc::new(Mutex::new(HashMap::new()));
    let slot: CallSlot = Arc::new(Mutex::new(None));
    let cfg = Arc::new(cfg);

    // 来电入口: 占线直接 486, 否则占槽 + 登记 re-INVITE 路由 + spawn 通话任务
    let on_incoming_call = {
        let slot = slot.clone();
        let routes = routes.clone();
        let ui = ui.clone();
        let tap = tap.clone();
        let cfg = cfg.clone();
        move |dialog: InviteDialog| {
            let caller = caller_from_request(&dialog.initial_request());
            {
                let mut guard = slot.lock();
                if guard.is_some() {
                    warn!(%caller, "占线, 新来电回 486");
                    drop(guard);
                    if let Err(e) = dialog.reject(Some(rsip::StatusCode::BusyHere), None) {
                        warn!(error = %e, "486 拒接失败");
                    }
                    return;
                }
                let (ctl_tx, ctl_rx) = mpsc::channel(4);
                *guard = Some(ctl_tx);
                let (update_tx, update_rx) = mpsc::channel(4);
                routes.lock().insert(dialog.id(), update_tx);
                let (cfg, ui, tap, slot, routes) = (
                    cfg.clone(),
                    ui.clone(),
                    tap.clone(),
                    slot.clone(),
                    routes.clone(),
                );
                tokio::spawn(async move {
                    handle_incoming(dialog, local, cfg, ui, tap, ctl_rx, update_rx, slot, routes)
                        .await;
                });
            }
        }
    };
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
                // 注册循环退出 = 永久失败 (401/403/404) 或被取消;
                // 原因带给 UI 错误条, 不只留在日志里
                warn!(result = ?r, "register loop exited");
                ui.post(UiEvent::RegisterFailed(format!("注册循环退出: {r:?}")));
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
    Ok(())
}

/// UI 命令处理: Dial 占线检查 + spawn 主叫任务; 通话三键转发进 ctl 通道
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
            let mut guard = slot.lock();
            if guard.is_some() {
                drop(guard);
                // UI 只会在 Idle 拨号, 走到这里是和来电抢了槽
                ui.post(UiEvent::Failed {
                    reason: "占线".into(),
                });
                return;
            }
            let callee = match client.callee_uri(&number) {
                Ok(u) => u,
                Err(e) => {
                    warn!(error = %e, %number, "被叫 URI 解析失败");
                    ui.post(UiEvent::Failed {
                        reason: format!("号码无效: {number}"),
                    });
                    return;
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
                    return;
                }
            };
            let rtp_ports = match (rtp_sockets.0.local_addr(), rtp_sockets.1.local_addr()) {
                (Ok(a), Ok(v)) => (a.port(), v.port()),
                _ => {
                    ui.post(UiEvent::Failed {
                        reason: "RTP 端口读取失败".into(),
                    });
                    return;
                }
            };
            let (ctl_tx, ctl_rx) = mpsc::channel(4);
            *guard = Some(ctl_tx);
            drop(guard);
            tokio::spawn(handle_outgoing(
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
                warn!(error = %e, "通话控制通道已满或已关闭");
            }
        }
    }
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

/// 被叫一路通话: ringing → 等用户接听/拒接/对端取消/超时 → accept 起
/// 媒体 → 通话 loop (对端 BYE / 本地挂断 / re-INVITE 重协商).
/// 无论哪条路径退出, 收尾都要: 停媒体、摘路由、清 slot、通知 UI.
// reason: 通话上下文参数打包传递, 照 answer.rs 的 process_call 结构;
// 内层 incoming_inner 与它共享同一份上下文, 抽结构体收益不明显
#[allow(clippy::too_many_arguments)]
async fn handle_incoming(
    dialog: InviteDialog,
    local: Ipv4Addr,
    cfg: Arc<GuiSipConfig>,
    ui: UiHandle,
    tap: VideoFrameSink,
    ctl_rx: mpsc::Receiver<CallCtl>,
    update_rx: mpsc::Receiver<UpdateMsg>,
    slot: CallSlot,
    routes: CallRoutes,
) {
    let res = incoming_inner(&dialog, local, &cfg, &ui, &tap, ctl_rx, update_rx).await;
    match res {
        Ok(reason) => ui.post(UiEvent::Terminated { reason }),
        Err(e) => {
            warn!(error = %e, "incoming call failed");
            ui.post(UiEvent::Failed {
                reason: e.to_string(),
            });
        }
    }
    routes.lock().remove(&dialog.id());
    *slot.lock() = None;
}

/// 返回值: Ok(reason) 是正常结束 (对端取消/本地拒接/挂断/对端 BYE),
/// Err 是失败 (信令或 SDP 错误), 由调用方分别映射 Terminated/Failed
// reason: 与外层 handle_incoming 共享同一份通话上下文, 拆分只为把
// 收尾 (停媒体/摘路由/清 slot/通知 UI) 收敛到外层, 参数随上下文走
#[allow(clippy::too_many_arguments)]
async fn incoming_inner(
    dialog: &InviteDialog,
    local: Ipv4Addr,
    cfg: &GuiSipConfig,
    ui: &UiHandle,
    tap: &VideoFrameSink,
    mut ctl_rx: mpsc::Receiver<CallCtl>,
    mut update_rx: mpsc::Receiver<UpdateMsg>,
) -> Result<String> {
    let caller = caller_from_request(&dialog.initial_request());
    info!(%caller, "incoming call");

    // 先振铃 (不带 SDP, 无早期媒体), 然后等用户决定, 不立即 accept
    dialog.ringing(None, None)?;
    ui.post(UiEvent::Incoming {
        caller: caller.clone(),
    });

    enum Decision {
        Accept,
        Reject(rsip::StatusCode),
        PeerCancelled,
    }
    let decision = tokio::select! {
        _ = dialog.cancel_token().cancelled() => Decision::PeerCancelled,
        ctl = ctl_rx.recv() => match ctl {
            Some(CallCtl::Accept) => Decision::Accept,
            // Reject 是用户拒接; Hangup/None (worker 退出) 兜底也走拒接
            _ => Decision::Reject(rsip::StatusCode::Decline),
        },
        _ = tokio::time::sleep(INCOMING_TIMEOUT) => {
            warn!(%caller, "来电超时未接听");
            Decision::Reject(rsip::StatusCode::TemporarilyUnavailable)
        }
    };
    match decision {
        Decision::PeerCancelled => return Ok("对端取消".into()),
        Decision::Reject(code) => {
            if let Err(e) = dialog.reject(Some(code), None) {
                warn!(error = %e, "拒接失败");
            }
            return Ok("本地拒接".into());
        }
        Decision::Accept => {}
    }

    // 用户已点接听, 之后的失败要给对端一个最终响应 (606), 否则对端
    // INVITE 事务挂到 32s 超时
    let offer = match parse_offer_all(dialog.initial_request().body()) {
        Ok(o) => o,
        Err(e) => {
            let _ = dialog.reject(Some(rsip::StatusCode::NotAcceptable), None);
            return Err(e);
        }
    };
    info!(?offer, "incoming offer");
    let (audio_pt, video, answer) = negotiate_answer(&offer, IpAddr::V4(local));
    let headers = vec![Header::ContentType("application/sdp".into())];
    if let Err(e) = dialog.accept(Some(headers), Some(answer.clone().into_bytes())) {
        let _ = dialog.reject(Some(rsip::StatusCode::NotAcceptable), None);
        return Err(e.into());
    }
    info!("call accepted, answer SDP:\n{answer}");
    ui.post(UiEvent::Confirmed {
        peer: caller.clone(),
    });

    if audio_pt.is_none() && video.is_none() {
        warn!("no supported media in offer, call kept up without media");
    }
    let mut media = media::start_incoming(&offer, cfg, tap);

    // 通话 loop: 对端 BYE → cancel_token; 本地挂断 → BYE;
    // re-INVITE → 重协商, 媒体指纹变了由 media 层重启管线
    let reason = loop {
        tokio::select! {
            _ = dialog.cancel_token().cancelled() => {
                break "对端挂断".to_string();
            }
            ctl = ctl_rx.recv() => {
                match ctl {
                    Some(CallCtl::Hangup) | None => {
                        dialog.bye_with_headers(None).await?;
                        break "本地挂断".to_string();
                    }
                    // 通话中 Accept/Reject 无意义 (迟到命令), 忽略
                    _ => {}
                }
            }
            msg = update_rx.recv() => {
                let Some((req, reply)) = msg else { break "路由通道关闭".to_string() };
                let body = match parse_offer_all(req.body()) {
                    Ok(new_offer) => {
                        let (_, _, answer) = negotiate_answer(&new_offer, IpAddr::V4(local));
                        info!("re-INVITE accepted, answer SDP:\n{answer}");
                        if let Some(m) = media.as_mut() {
                            m.renegotiate(&new_offer);
                        }
                        Some(answer.into_bytes())
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to parse re-INVITE offer, reply 200 without SDP");
                        None
                    }
                };
                let _ = reply.send(body);
            }
        }
    };
    if let Some(m) = media.take() {
        m.stop();
    }
    Ok(reason)
}

/// 主叫一路通话: INVITE → 非 200 即失败 → 200 解析 answer 起媒体 →
/// 通话 loop (对端 BYE / 本地挂断). re-INVITE 不经路由表 (主叫不登记),
/// forward_update 兜底回不带 SDP 的 200 — 本端媒体不变, 同 call.rs.
// reason: 通话上下文参数打包传递, 照 call.rs 的 call_until_hangup 结构
// (占位 socket 等一次性资源必须按值移交, 无法从共享上下文取)
#[allow(clippy::too_many_arguments)]
async fn handle_outgoing(
    dialog_layer: Arc<DialogLayer>,
    state_sender: DialogStateSender,
    callee: rsip::Uri,
    contact: rsip::Uri,
    credential: Credential,
    number: String,
    local: Ipv4Addr,
    rtp_ports: (u16, u16),
    // 占位 socket: 保 offer 端口到 INVITE 完成, 进这里后释放给媒体层
    rtp_sockets: (std::net::UdpSocket, std::net::UdpSocket),
    cfg: Arc<GuiSipConfig>,
    ui: UiHandle,
    tap: VideoFrameSink,
    ctl_rx: mpsc::Receiver<CallCtl>,
    slot: CallSlot,
) {
    let res = outgoing_inner(
        dialog_layer,
        state_sender,
        callee,
        contact,
        credential,
        &number,
        local,
        rtp_ports,
        rtp_sockets,
        &cfg,
        &ui,
        &tap,
        ctl_rx,
    )
    .await;
    match res {
        Ok(reason) => ui.post(UiEvent::Terminated { reason }),
        Err(e) => {
            warn!(error = %e, %number, "outgoing call failed");
            ui.post(UiEvent::Failed {
                reason: e.to_string(),
            });
        }
    }
    *slot.lock() = None;
}

// reason: 与外层 handle_outgoing 共享同一份通话上下文, 拆分只为把
// 收尾 (清 slot/通知 UI) 收敛到外层, 参数随上下文走
#[allow(clippy::too_many_arguments)]
async fn outgoing_inner(
    dialog_layer: Arc<DialogLayer>,
    state_sender: DialogStateSender,
    callee: rsip::Uri,
    contact: rsip::Uri,
    credential: Credential,
    number: &str,
    local: Ipv4Addr,
    rtp_ports: (u16, u16),
    rtp_sockets: (std::net::UdpSocket, std::net::UdpSocket),
    cfg: &GuiSipConfig,
    ui: &UiHandle,
    tap: &VideoFrameSink,
    mut ctl_rx: mpsc::Receiver<CallCtl>,
) -> Result<String> {
    let (audio_port, video_port) = rtp_ports;
    let offer = build_av_offer(
        IpAddr::V4(local),
        audio_port,
        video_port,
        &[PT_PCMU, PT_PCMA],
    );
    info!(%callee, audio_port, video_port, "calling, SDP offer:\n{offer}");

    let invite_option = InviteOption {
        callee,
        caller: contact.clone(),
        contact,
        credential: Some(credential),
        offer: Some(offer.to_string().into_bytes()),
        ..Default::default()
    };
    // 407/401 challenge 由 rsipstack 用 credential 自动处理 (同 call.rs)
    let (dialog, resp) = dialog_layer.do_invite(invite_option, state_sender).await?;
    let resp = resp.ok_or_else(|| anyhow!("INVITE 无最终响应 (超时)"))?;
    info!(code = %resp.status_code, "INVITE final response");
    if resp.status_code != rsip::StatusCode::OK {
        return Err(anyhow!("呼叫被拒: {}", resp.status_code));
    }

    // do_invite 要等到最终响应, 期间用户按挂断命令还压在 ctl 通道里;
    // 接通后立即补 BYE, 不进通话
    if matches!(ctl_rx.try_recv(), Ok(CallCtl::Hangup)) {
        dialog.bye_with_headers(None).await?;
        return Ok("本地取消".into());
    }

    let peers = parse_answer_all(resp.body())?;
    ui.post(UiEvent::Confirmed {
        peer: number.to_string(),
    });

    // 占位 socket 任务完成, 释放端口给媒体层 (同 call.rs)
    drop(rtp_sockets);
    let media = media::start_outgoing(&peers, rtp_ports, cfg, tap);
    info!("call established");

    let reason = loop {
        tokio::select! {
            _ = dialog.cancel_token().cancelled() => {
                break "对端挂断".to_string();
            }
            ctl = ctl_rx.recv() => {
                match ctl {
                    Some(CallCtl::Hangup) | None => {
                        dialog.bye_with_headers(None).await?;
                        break "本地挂断".to_string();
                    }
                    _ => {}
                }
            }
        }
    };
    if let Some(m) = media {
        m.stop();
    }
    Ok(reason)
}

/// 协商结果: (选中的音频 pt+编码, 选中的视频 pt+编码+fmtp, answer 文本)
type NegotiatedAnswer = (
    Option<(u8, AudioCodec)>,
    Option<(u8, VideoCodec, Option<String>)>,
    String,
);

/// 音频只接 G.711 (同 answer.rs: 复用 ipcam_gst::sendable_audio_codec
/// 的白名单), 按对端 offer 的偏好序挑第一个我们支持的
fn pick_audio(offer: &OfferMedias) -> Option<(u8, AudioCodec)> {
    let a = offer.audio.as_ref()?;
    let picked = a
        .codecs
        .iter()
        .find_map(|c| ipcam_gst::sendable_audio_codec(&c.codec).map(|ac| (c.payload_type, ac)));
    if picked.is_none() {
        warn!(codecs = ?a.codecs, "no supported audio codec in offer");
    }
    picked
}

/// 视频只接 H264/H265, 其他整路拒收, 但 SDP 仍带 audio 给对端一个能
/// 落地的 answer (同 answer.rs)
fn pick_video(offer: &OfferMedias) -> Option<(u8, VideoCodec, Option<String>)> {
    let v = offer.video.as_ref()?;
    let picked = v
        .codecs
        .iter()
        .find_map(|c| match VideoCodec::from_name(&c.codec) {
            vc @ (VideoCodec::H264 | VideoCodec::H265) => {
                Some((c.payload_type, vc, c.fmtp.clone()))
            }
            _ => None,
        });
    if picked.is_none() {
        warn!(codecs = ?v.codecs, "no supported video codec in offer");
    }
    picked
}

/// 从 offer 协商编码并构建 answer SDP (初始 accept 和 re-INVITE 共用,
/// 同 answer.rs): answer 的 pt/fmtp 回显 offer 里选中的编码 (RFC 3264)
fn negotiate_answer(offer: &OfferMedias, local_ip: IpAddr) -> NegotiatedAnswer {
    let audio_pt = pick_audio(offer);
    let video = pick_video(offer);
    let answer = build_av_answer(
        local_ip,
        AUDIO_RTP_PORT,
        VIDEO_RTP_PORT,
        audio_pt.map(|(pt, _)| pt).unwrap_or(PT_PCMA),
        video.as_ref().map(|(pt, _, _)| *pt).unwrap_or(96),
        video.as_ref().and_then(|(_, _, fmtp)| fmtp.as_deref()),
    );
    (audio_pt, video, answer.to_string())
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
