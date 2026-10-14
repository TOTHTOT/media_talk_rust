//! 主叫一路通话: INVITE → 非 200 即失败 → 200 解析 answer 起媒体 → 通话
//! loop (对端 BYE / 本地挂断). 信令逻辑改写自 ipcam-sip 的 examples/call.rs.

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

use super::{CallCtl, CallSlot, GuiSipConfig};
use crate::app::UiEvent;
use crate::bridge::UiHandle;
use crate::media;

/// 主叫一路通话: INVITE → 非 200 即失败 → 200 解析 answer 起媒体 → 通话
/// loop (对端 BYE / 本地挂断). re-INVITE 不经路由表 (主叫不登记),
/// forward_update 兜底回不带 SDP 的 200 — 本端媒体不变, 同 call.rs.
///
/// # Arguments
///
/// * `dialog_layer`: Dialog 层句柄
/// * `state_sender`: Dialog 状态发送通道
/// * `callee`: 被叫 URI
/// * `contact`: 本机 Contact URI
/// * `credential`: Digest 鉴权凭据
/// * `number`: 拨号号码
/// * `local`: 本机 IPv4
/// * `rtp_ports`: 本机 RTP 端口 (音频, 视频)
/// * `rtp_sockets`: 占位 socket (保 offer 端口)
/// * `cfg`: SIP 配置
/// * `ui`: 事件回投句柄
/// * `tap`: 视频帧回调
/// * `ctl_rx`: 用户操作控制通道
/// * `slot`: 通话槽位
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
// reason: 通话上下文参数打包传递, 照 call.rs 的 call_until_hangup 结构
// (占位 socket 等一次性资源必须按值移交, 无法从共享上下文取)
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_outgoing(
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

/// # Arguments
///
/// * `dialog_layer`: Dialog 层句柄
/// * `state_sender`: Dialog 状态发送通道
/// * `callee`: 被叫 URI
/// * `contact`: 本机 Contact URI
/// * `credential`: Digest 鉴权凭据
/// * `number`: 拨号号码
/// * `local`: 本机 IPv4
/// * `rtp_ports`: 本机 RTP 端口 (音频, 视频)
/// * `rtp_sockets`: 占位 socket (保 offer 端口)
/// * `cfg`: SIP 配置
/// * `ui`: 事件回投句柄
/// * `tap`: 视频帧回调
/// * `ctl_rx`: 用户操作控制通道
///
/// returns: Result<String>
///
/// # Examples
///
/// ```
///
/// ```
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
    let resp = resp.ok_or_else(|| anyhow!("INVITE has no final response (timeout)"))?;
    info!(code = %resp.status_code, "INVITE final response");
    if resp.status_code != rsip::StatusCode::OK {
        return Err(anyhow!("call rejected: {}", resp.status_code));
    }

    // do_invite 要等到最终响应, 期间用户按挂断命令还压在 ctl 通道里;
    // 接通后立即补 BYE, 不进通话
    if matches!(ctl_rx.try_recv(), Ok(CallCtl::Hangup)) {
        dialog.bye_with_headers(None).await?;
        return Ok("local cancelled".into());
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
                break "peer hung up".to_string();
            }
            ctl = ctl_rx.recv() => {
                match ctl {
                    Some(CallCtl::Hangup) | None => {
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
