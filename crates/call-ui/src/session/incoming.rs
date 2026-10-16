//! 被叫一路通话: ringing → 等用户接听/拒接/对端取消/超时 → accept 起
//! 媒体 → 通话 loop (对端 BYE / 本地挂断 / re-INVITE 重协商). 信令逻辑
//! 改写自 ipcam-sip 的 examples/answer.rs.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use anyhow::Result;
use ipcam_gst::VideoFrameSink;
use ipcam_sip::negotiate;
use ipcam_sip::sdp::{OfferMedias, parse_offer_all};
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip as rsip;
use rsipstack::sip::headers::Header;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::call_loop::call_loop;
use super::types::INCOMING_TIMEOUT;
use super::types::{
    CallCtlTx, CallRoutes, GuiSipConfig, RtpPorts, UpdateMsg, WorkerCmd, caller_from_request,
};
use crate::app::UiEvent;
use crate::bridge::UiHandle;
use crate::media;

/// 被叫一路通话: ringing → 等用户接听/拒接/对端取消/超时 → accept 起
/// 媒体 → 通话 loop (对端 BYE / 本地挂断 / re-INVITE 重协商).
/// 无论哪条路径退出, 收尾都要: 停媒体、摘路由、清 call_ctl_tx、通知 UI.
///
/// # Arguments
///
/// * `dialog`: 来电 InviteDialog
/// * `ctx`: 被叫通话上下文 (含 SIP 配置/UI 句柄/控制通道)
/// * `call_ctl_tx`: 通话控制通道发送端 (退出时清空释放占位)
/// * `routes`: 进行中通话路由表 (退出时摘除)
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
pub(super) async fn handle_incoming(
    dialog: InviteDialog,
    mut ctx: IncomingCtx,
    call_ctl_tx: CallCtlTx,
    routes: CallRoutes,
) {
    let res = incoming_inner(&dialog, &mut ctx).await;
    match res {
        Ok(reason) => ctx.ui.post(UiEvent::Terminated { reason }),
        Err(e) => {
            warn!(error = %e, "incoming call failed");
            ctx.ui.post(UiEvent::Failed {
                reason: e.to_string(),
            });
        }
    }
    routes.lock().remove(&dialog.id());
    *call_ctl_tx.lock() = None;
}

/// 被叫通话上下文: 在 spawn 时打包, 整个通话生命周期内共享
pub(crate) struct IncomingCtx {
    pub(super) cfg: Arc<GuiSipConfig>,
    pub(super) ui: UiHandle,
    pub(super) tap: VideoFrameSink,
    pub(super) local: Ipv4Addr,
    pub(super) ctl_rx: mpsc::Receiver<WorkerCmd>,
    pub(super) update_rx: mpsc::Receiver<UpdateMsg>,
}

/// 来电等待用户操作的决定: 接听 / 拒接 (带 SIP 状态码) / 对端取消
#[derive(Debug)]
enum Decision {
    Accept,
    Reject(rsip::StatusCode),
    PeerCancelled,
}

/// 先振铃 (不带 SDP, 无早期媒体), post Incoming 通知 UI, 然后等用户
/// 决定, 不立即 accept; 超时自动拒接, 避免对端挂死在 ringing
///
/// # Arguments
///
/// * `dialog`: 来电 InviteDialog
/// * `ui`: 事件回投句柄
/// * `caller`: 主叫号码 (UI 事件与超时日志用)
/// * `ctl_rx`: 用户操作控制通道
///
/// returns: Result<Decision>
///
/// # Examples
///
/// ```
///
/// ```
async fn wait_user_decision(
    dialog: &InviteDialog,
    ui: &UiHandle,
    caller: &str,
    ctl_rx: &mut mpsc::Receiver<WorkerCmd>,
) -> Result<Decision> {
    dialog.ringing(None, None)?;
    info!("post to ui, incoming from {caller}");
    ui.post(UiEvent::Incoming {
        caller: caller.to_string(),
    });

    let decision = tokio::select! {
        _ = dialog.cancel_token().cancelled() => Decision::PeerCancelled,
        ctl = ctl_rx.recv() => match ctl {
            Some(WorkerCmd::Accept) => Decision::Accept,
            // Reject 是用户拒接; Hangup/None (worker 退出) 兜底也走拒接
            _ => Decision::Reject(rsip::StatusCode::Decline),
        },
        _ = tokio::time::sleep(INCOMING_TIMEOUT) => {
            warn!(%caller, "incoming call timed out");
            Decision::Reject(rsip::StatusCode::TemporarilyUnavailable)
        }
    };
    Ok(decision)
}

/// 用户已点接听: 解析 offer + 协商 answer + accept; 失败一律先回 488
/// 再报错, 对端 INVITE 事务不挂到 32s 超时
///
/// # Arguments
///
/// * `dialog`: 来电 InviteDialog
/// * `local`: 本机 IPv4
/// * `caller`: 主叫号码 (Confirmed 事件用)
/// * `ui`: 事件回投句柄
/// * `rtp_ports`: 本机 RTP 接收端口 (音频, 视频)
///
/// returns: Result<(OfferMedias, String)>
///
/// # Examples
///
/// ```
///
/// ```
fn accept_with_answer(
    dialog: &InviteDialog,
    local: Ipv4Addr,
    caller: &str,
    ui: &UiHandle,
    rtp_ports: RtpPorts,
) -> Result<(OfferMedias, String)> {
    let offer = match parse_offer_all(dialog.initial_request().body()) {
        Ok(o) => o,
        Err(e) => {
            let _ = dialog.reject(Some(rsip::StatusCode::NotAcceptableHere), None);
            return Err(e);
        }
    };
    info!(?offer, "incoming offer");
    let (audio_pt, video, answer) = negotiate::build_answer(
        &offer,
        IpAddr::V4(local),
        rtp_ports.audio,
        rtp_ports.video,
        media::audio_pred,
        media::video_pred,
    );
    let headers = vec![Header::ContentType("application/sdp".into())];
    if let Err(e) = dialog.accept(Some(headers), Some(answer.clone().into_bytes())) {
        let _ = dialog.reject(Some(rsip::StatusCode::NotAcceptableHere), None);
        return Err(e.into());
    }
    info!("call accepted, answer SDP:\n{answer}");
    ui.post(UiEvent::Confirmed {
        peer: caller.to_string(),
    });

    if audio_pt.is_none() && video.is_none() {
        warn!("no supported media in offer, call kept up without media");
    }
    Ok((offer, answer))
}

/// 被叫通话内部流程: 等用户决策 -> 协商 answer -> 起媒体 -> 通话 loop.
/// Ok(reason) 是正常结束 (对端取消/本地拒接/挂断/对端 BYE),
/// Err 是失败 (信令或 SDP 错误), 由调用方分别映射 Terminated/Failed.
///
/// # Arguments
///
/// * `dialog`: 来电 InviteDialog
/// * `ctx`: 被叫通话上下文 (含 SIP 配置/UI 句柄/控制通道)
///
/// returns: Result<String> — Ok 为结束原因, Err 为信令/媒体层错误
///
/// # Examples
///
/// ```
///
/// ```
async fn incoming_inner(dialog: &InviteDialog, ctx: &mut IncomingCtx) -> Result<String> {
    let caller = caller_from_request(&dialog.initial_request());
    info!(%caller, "incoming call");

    let decision = wait_user_decision(dialog, &ctx.ui, &caller, &mut ctx.ctl_rx).await?;
    info!(decision_result = ?decision);
    match decision {
        Decision::PeerCancelled => return Ok("peer cancelled".into()),
        Decision::Reject(code) => {
            if let Err(e) = dialog.reject(Some(code), None) {
                warn!(error = %e, "reject failed");
            }
            return Ok("local rejected".into());
        }
        Decision::Accept => {}
    }

    let (offer, _) = accept_with_answer(dialog, ctx.local, &caller, &ctx.ui, ctx.cfg.rtp_ports)?;
    let mut media = media::start_incoming(&offer, &ctx.cfg, &ctx.tap);

    let reason = call_loop(
        dialog,
        &mut media,
        &mut ctx.ctl_rx,
        &mut ctx.update_rx,
        ctx.local,
        ctx.cfg.rtp_ports,
    )
    .await?;
    if let Some(m) = media.take() {
        m.stop();
    }
    Ok(reason)
}
