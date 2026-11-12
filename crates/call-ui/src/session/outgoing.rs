//! 主叫一路通话: INVITE -> non-200 means failure -> 200 parse answer and start
//! media -> call loop (peer BYE / local hangup / re-INVITE renegotiate).
//! Signalling logic adapted from ipcam-sip examples/call.rs.

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use ipcam_sip::sdp::{PT_OPUS, PT_PCMA, PT_PCMU, build_av_offer, parse_answer_all};
use rsipstack::dialog::DialogId;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::{DialogState, DialogStateSender};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::sip as rsip;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::call_loop::{call_loop, finish_call};
use super::ice::{self, LocalIce};
use super::types::{CallBase, CallCtlTx, CallRoutes, RecvSockets, RtpPorts, UpdateMsg, WorkerCmd};
use crate::app::UiEvent;
use crate::media;

/// 主叫一路通话: INVITE -> non-200 means failure -> 200 parse answer and start
/// media -> call loop (peer BYE / local hangup / re-INVITE renegotiate).
///
/// # Arguments
///
/// * `dialog_layer`: SIP dialog 层, 构造 INVITE 用
/// * `state_sender`: dialog 状态发送端
/// * `slot`: 通话槽位 (占线保护)
/// * `ctx`: 主叫通话上下文
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
pub(super) async fn handle_outgoing(
    dialog_layer: Arc<DialogLayer>,
    state_sender: DialogStateSender,
    slot: CallCtlTx,
    mut ctx: OutgoingCtx,
) {
    let res = outgoing_inner(dialog_layer, state_sender, &mut ctx).await;
    finish_call(
        &ctx.base.ui,
        &slot,
        &format!("outgoing call to {}", ctx.number),
        res,
    );
}

/// Outgoing call context: packed at spawn, shared across the entire call lifecycle.
// dialog_layer / state_sender are only used for do_invite, passed separately.
pub(crate) struct OutgoingCtx {
    pub(super) callee: rsip::Uri,
    pub(super) contact: rsip::Uri,
    pub(super) credential: Credential,
    pub(super) number: String,
    /// re-INVITE 路由表: INVITE 成功后按 dialog id 登记, 对端 (如
    /// Linphone 通话中开视频) 发来的 re-INVITE 才能转进通话 loop
    pub(super) routes: CallRoutes,
    /// re-INVITE 通道发送端 (登记进 routes), forward_update 经它找本任务
    pub(super) update_tx: mpsc::Sender<UpdateMsg>,
    /// 主被叫公共部分 (配置/UI/tap/本机 IP/控制与更新通道)
    pub(super) base: CallBase,
}

/// re-INVITE 路由登记 guard: drop 时自动从路由表摘除, 保证任意退出路径
/// (被拒/失败/BYE/本地挂断) 都不留死路由
struct RouteGuard {
    routes: CallRoutes,
    id: DialogId,
}

impl Drop for RouteGuard {
    fn drop(&mut self) {
        self.routes.lock().unwrap().remove(&self.id);
    }
}

/// 主叫在 INVITE 前准备好的媒体传输方式.
enum OutgoingTransport {
    /// 普通 RTP: socket 已绑定, offer 里的端口就是它们.
    Rtp {
        ports: RtpPorts,
        sockets: RecvSockets,
    },
    /// ICE: agent 已收集候选, offer 带 ICE; 失败时回退绑定同一批端口.
    Ice { ports: RtpPorts, local: LocalIce },
}

/// INVITE 前的媒体准备结果.
struct OutgoingOffer {
    bytes: Vec<u8>,
    transport: OutgoingTransport,
}

/// 准备 offer: ICE 开启时先建 agent 收集候选并写进 SDP, 否则绑定普通
/// RTP socket. ICE 任一步失败都回退普通 RTP, 不让呼叫建立卡死.
async fn prepare_offer(ctx: &OutgoingCtx) -> Result<OutgoingOffer> {
    if ctx.base.cfg.ice.enabled {
        let local = ice::prepare(
            ctx.base.local,
            ctx.base.cfg.ice.stun.clone(),
            true, // 主叫 = offerer = controlling
        )
        .await?;
        let ports = local.ports();
        let plain = build_av_offer(
            IpAddr::V4(ctx.base.local),
            ports.audio,
            ports.video,
            &[PT_PCMU, PT_PCMA, PT_OPUS],
        )
        .to_string();
        let text = local.offer_sdp(&plain)?;
        info!(%ctx.callee, ?ports, "calling with ICE, SDP offer:\n{text}");
        return Ok(OutgoingOffer {
            bytes: text.into_bytes(),
            transport: OutgoingTransport::Ice { ports, local },
        });
    }

    let (ports, sockets) = super::types::bind_recv_sockets(ctx.base.local, None)?;
    let offer = build_av_offer(
        IpAddr::V4(ctx.base.local),
        ports.audio,
        ports.video,
        &[PT_PCMU, PT_PCMA],
    );
    info!(%ctx.callee, ?ports, "calling, SDP offer:\n{offer}");
    Ok(OutgoingOffer {
        bytes: offer.to_string().into_bytes(),
        transport: OutgoingTransport::Rtp { ports, sockets },
    })
}

/// 主叫通话内部流程: 构造 SDP offer -> INVITE -> 解析 answer -> 起媒体 -> 通话 loop.
/// 对端 BYE / 本地 Hangup / route channel 关闭任一路均正常返回结束原因.
///
/// # Arguments
///
/// * `dialog_layer`: SIP dialog 层, 构造 INVITE 用
/// * `state_sender`: dialog 状态发送端, 供 run_dialog_state_loop 接收事件用
/// * `ctx`: 主叫通话上下文 (含被叫 URI / 凭据 / RTP socket / 配置)
///
/// returns: Result<String> — Ok 为结束原因 ("peer hung up" / "local hung up" /
/// "local cancelled"), Err 为信令或媒体层错误
///
/// # Examples
///
/// ```
///
/// ```
async fn outgoing_inner(
    dialog_layer: Arc<DialogLayer>,
    state_sender: DialogStateSender,
    ctx: &mut OutgoingCtx,
) -> Result<String> {
    // 媒体准备: ICE 开启时先 gather 并写进 offer; 否则普通 RTP socket
    // 从 INVITE 前绑好一直活到通话结束 (udpsrc/udpsink 共享, 对称 RTP,
    // 过媒体锚定服务器的互通前提). ICE 回退时再按 offer 端口补绑.
    let setup = prepare_offer(ctx).await?;
    let invite_option = InviteOption {
        callee: ctx.callee.clone(),
        caller: ctx.contact.clone(),
        contact: ctx.contact.clone(),
        credential: Some(ctx.credential.clone()),
        offer: Some(setup.bytes.clone()),
        ..Default::default()
    };
    let (dialog, mut invite) = dialog_layer.do_invite_async(invite_option, state_sender)?;

    // 振铃期间 (Trying/Early) 监听本地挂断: 收到 Hangup 发 CANCEL 中止
    // INVITE 让对端停止振铃; 接通后的挂断走通话 loop 的 BYE.
    // do_invite (同步版) 会一直阻塞到最终响应, 振铃期收不到挂断命令.
    let (dialog_id, resp) = loop {
        tokio::select! {
            r = &mut invite => break r??,
            ctl = ctx.base.call_answer_ctl_rx.recv() => {
                match ctl {
                    Some(WorkerCmd::Hangup) | None => {
                        dialog.cancel().await?;
                        // cancel 只对 Trying/Early 生效; 挂断与 200 OK 同时
                        // 到达的竞态下已接通, 补 BYE
                        if matches!(dialog.state(), DialogState::Confirmed(..)) {
                            dialog.bye_with_headers(None).await?;
                        }
                        return Ok("local cancelled".into());
                    }
                    // 振铃期 Accept/Reject 无意义 (迟到命令), 忽略继续等
                    _ => {}
                }
            }
        }
    };
    let resp = resp.ok_or_else(|| anyhow!("INVITE has no final response (timeout)"))?;
    info!(code = %resp.status_code, "INVITE final response");
    if resp.status_code != rsip::StatusCode::OK {
        return Err(anyhow!("call rejected: {}", resp.status_code));
    }

    // 登记 re-INVITE 路由 (用 INVITE 完成后的 confirmed dialog id, 与
    // dialog 层 re-key 一致): 对端 (如 Linphone 接通后开视频) 发来的
    // re-INVITE 才能经 forward_update 转进下面的通话 loop
    let _route = {
        ctx.routes
            .lock()
            .unwrap()
            .insert(dialog_id.clone(), ctx.update_tx.clone());
        RouteGuard {
            routes: ctx.routes.clone(),
            id: dialog_id,
        }
    };

    let peers = parse_answer_all(resp.body())?;
    ctx.base.ui.post(UiEvent::Confirmed {
        peer: ctx.number.clone(),
    });

    let (rtp_ports, mut media) = match setup.transport {
        OutgoingTransport::Rtp { ports, sockets } => (
            ports,
            media::start_outgoing(&peers, &ports, sockets, &ctx.base.cfg, &ctx.base.tap),
        ),
        OutgoingTransport::Ice { ports, local } => {
            let mut media = None;
            if ice::answer_supports_ice(&peers) {
                let answer = String::from_utf8_lossy(resp.body());
                media = Some(media::CallMedia::Ice(
                    ice::activate(local, answer.as_ref(), &ctx.base.cfg, &ctx.base.tap).await?,
                ));
            } else {
                warn!("answer did not select Opus/H264 for ICE, falling back to plain RTP");
                drop(local);
            }
            let ports = if media.is_none() {
                // 回退: agent 已关闭, 端口已释放; 绑定 offer 里声明的同一批
                // 端口, 对端仍按原 SDP 发包.
                let (bound, sockets) =
                    super::types::bind_recv_sockets(ctx.base.local, Some(ports))?;
                media =
                    media::start_outgoing(&peers, &bound, sockets, &ctx.base.cfg, &ctx.base.tap);
                bound
            } else {
                ports
            };
            (ports, media)
        }
    };
    info!("call established");

    let reason = call_loop(
        &dialog,
        &mut media,
        &mut ctx.base.call_answer_ctl_rx,
        &mut ctx.base.update_rx,
        ctx.base.local,
        rtp_ports,
    )
    .await?;
    if let Some(m) = media {
        m.stop();
    }
    Ok(reason)
}
