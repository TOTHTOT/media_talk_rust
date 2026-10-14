//! 主叫一路通话: INVITE -> non-200 means failure -> 200 parse answer and start
//! media -> call loop (peer BYE / local hangup). Signalling logic adapted from
//! ipcam-sip examples/call.rs.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use ipcam_gst::VideoFrameSink;
use ipcam_sip::sdp::{PT_PCMA, PT_PCMU, build_av_offer, parse_answer_all};
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::DialogStateSender;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::sip as rsip;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::types::{CallSlot, GuiSipConfig, WorkerCmd};
use crate::app::UiEvent;
use crate::bridge::UiHandle;
use crate::media;

/// 主叫一路通话: INVITE -> non-200 means failure -> 200 parse answer and start
/// media -> call loop (peer BYE / local hangup). re-INVITE does not go through
/// the route table (caller does not register), forward_update falls back to 200
/// without SDP -- local media unchanged, same as call.rs.
pub(super) async fn handle_outgoing(
    dialog_layer: Arc<DialogLayer>,
    state_sender: DialogStateSender,
    slot: CallSlot,
    mut ctx: OutgoingCtx,
) {
    let res = outgoing_inner(dialog_layer, state_sender, &mut ctx).await;
    match res {
        Ok(reason) => ctx.ui.post(UiEvent::Terminated { reason }),
        Err(e) => {
            warn!(error = %e, %ctx.number, "outgoing call failed");
            ctx.ui.post(UiEvent::Failed {
                reason: e.to_string(),
            });
        }
    }
    *slot.lock() = None;
}

/// Outgoing call context: packed at spawn, shared across the entire call lifecycle.
// dialog_layer / state_sender are only used for do_invite, passed separately.
pub(crate) struct OutgoingCtx {
    pub(super) callee: rsip::Uri,
    pub(super) contact: rsip::Uri,
    pub(super) credential: Credential,
    pub(super) number: String,
    pub(super) local: Ipv4Addr,
    // Placeholder sockets: hold offer ports until INVITE completes, released here to media layer
    pub(super) rtp_sockets: (std::net::UdpSocket, std::net::UdpSocket),
    pub(super) cfg: Arc<GuiSipConfig>,
    pub(super) ui: UiHandle,
    pub(super) tap: VideoFrameSink,
    pub(super) ctl_rx: mpsc::Receiver<WorkerCmd>,
}

async fn outgoing_inner(
    dialog_layer: Arc<DialogLayer>,
    state_sender: DialogStateSender,
    ctx: &mut OutgoingCtx,
) -> Result<String> {
    let rtp_ports = (
        ctx.rtp_sockets.0.local_addr().unwrap().port(),
        ctx.rtp_sockets.1.local_addr().unwrap().port(),
    );
    let (audio_port, video_port) = rtp_ports;

    let offer = build_av_offer(
        IpAddr::V4(ctx.local),
        audio_port,
        video_port,
        &[PT_PCMU, PT_PCMA],
    );
    info!(%ctx.callee, audio_port, video_port, "calling, SDP offer:\n{offer}");

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

    if matches!(ctx.ctl_rx.try_recv(), Ok(WorkerCmd::Hangup)) {
        dialog.bye_with_headers(None).await?;
        return Ok("local cancelled".into());
    }

    let peers = parse_answer_all(resp.body())?;
    ctx.ui.post(UiEvent::Confirmed {
        peer: ctx.number.clone(),
    });

    // sockets dropped when this scope ends (no drop() needed)
    let media = media::start_outgoing(&peers, rtp_ports, &ctx.cfg, &ctx.tap);
    info!("call established");

    let reason = loop {
        tokio::select! {
            _ = dialog.cancel_token().cancelled() => {
                break "peer hung up".to_string();
            }
            ctl = ctx.ctl_rx.recv() => {
                match ctl {
                    Some(WorkerCmd::Hangup) | None => {
                        dialog.bye_with_headers(None).await?;
                        break "local hung up".to_string();
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
