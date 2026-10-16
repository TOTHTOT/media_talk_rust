//! 主叫一路通话: INVITE -> non-200 means failure -> 200 parse answer and start
//! media -> call loop (peer BYE / local hangup / re-INVITE renegotiate).
//! Signalling logic adapted from ipcam-sip examples/call.rs.

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use ipcam_sip::sdp::{PT_PCMA, PT_PCMU, build_av_offer, parse_answer_all};
use rsipstack::dialog::DialogId;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::DialogStateSender;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::sip as rsip;
use tokio::sync::mpsc;
use tracing::info;

use super::call_loop::{call_loop, finish_call};
use super::types::{CallBase, CallCtlTx, CallRoutes, UpdateMsg, WorkerCmd};
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
    let offer = build_av_offer(
        IpAddr::V4(ctx.base.local),
        ctx.base.cfg.rtp_ports.audio,
        ctx.base.cfg.rtp_ports.video,
        &[PT_PCMU, PT_PCMA],
    );
    info!(%ctx.callee, rtp_ports = ?ctx.base.cfg.rtp_ports, "calling, SDP offer:\n{offer}");

    let invite_option = InviteOption {
        callee: ctx.callee.clone(),
        caller: ctx.contact.clone(),
        contact: ctx.contact.clone(),
        credential: Some(ctx.credential.clone()),
        offer: Some(offer.to_string().into_bytes()),
        ..Default::default()
    };
    let (dialog, resp) = dialog_layer.do_invite(invite_option, state_sender).await?;
    let resp = resp.ok_or_else(|| anyhow!("INVITE has no final response (timeout)"))?;
    info!(code = %resp.status_code, "INVITE final response");
    if resp.status_code != rsip::StatusCode::OK {
        return Err(anyhow!("call rejected: {}", resp.status_code));
    }

    // 登记 re-INVITE 路由: 对端 (如 Linphone 接通后开视频) 发来的
    // re-INVITE 才能经 forward_update 转进下面的通话 loop
    let _route = {
        let id = dialog.id();
        ctx.routes
            .lock()
            .unwrap()
            .insert(id.clone(), ctx.update_tx.clone());
        RouteGuard {
            routes: ctx.routes.clone(),
            id,
        }
    };

    if matches!(ctx.base.ctl_rx.try_recv(), Ok(WorkerCmd::Hangup)) {
        dialog.bye_with_headers(None).await?;
        return Ok("local cancelled".into());
    }

    let peers = parse_answer_all(resp.body())?;
    ctx.base.ui.post(UiEvent::Confirmed {
        peer: ctx.number.clone(),
    });

    let mut media = media::start_outgoing(
        &peers,
        &ctx.base.cfg.rtp_ports,
        &ctx.base.cfg,
        &ctx.base.tap,
    );
    info!("call established");

    let reason = call_loop(
        &dialog,
        &mut media,
        &mut ctx.base.ctl_rx,
        &mut ctx.base.update_rx,
        ctx.base.local,
        ctx.base.cfg.rtp_ports,
    )
    .await?;
    if let Some(m) = media {
        m.stop();
    }
    Ok(reason)
}
