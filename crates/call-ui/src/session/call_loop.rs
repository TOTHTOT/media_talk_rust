//! 已接通通话的共享 loop: 主叫被叫接通之后做的事完全一样 — 等对端
//! BYE / 本地挂断 / re-INVITE 重协商 (do_invite 与来电入口返回的同为
//! InviteDialog, 无需抽象).

use std::net::Ipv4Addr;

use anyhow::Result;
use rsipstack::dialog::invite_dialog::InviteDialog;
use tokio::sync::mpsc;

use super::types::{RtpPorts, UpdateMsg, WorkerCmd};
use crate::media;

/// 通话 loop: 对端 BYE → cancel_token; 本地挂断 → BYE;
/// re-INVITE → 重协商, 媒体指纹变了由 media 层重启管线.
///
/// # Arguments
///
/// * `dialog`: 已接通的 InviteDialog
/// * `media`: 媒体句柄 (re-INVITE 重协商时按需重启管线)
/// * `ctl_rx`: 用户操作控制通道
/// * `update_rx`: re-INVITE 更新消息通道
/// * `local`: 本机 IPv4 (re-INVITE answer 的 c= 行)
/// * `rtp_ports`: 本机 RTP 接收端口
///
/// returns: Result<String> — 结束原因 (peer hung up / local hung up / route channel closed)
pub(super) async fn call_loop(
    dialog: &InviteDialog,
    media: &mut Option<media::MediaHandles>,
    ctl_rx: &mut mpsc::Receiver<WorkerCmd>,
    update_rx: &mut mpsc::Receiver<UpdateMsg>,
    local: Ipv4Addr,
    rtp_ports: RtpPorts,
) -> Result<String> {
    let reason = loop {
        tokio::select! {
            _ = dialog.cancel_token().cancelled() => {
                break "peer hung up".to_string();
            }
            ctl = ctl_rx.recv() => {
                match ctl {
                    Some(WorkerCmd::Hangup) | None => {
                        dialog.bye_with_headers(None).await?;
                        break "local hung up".to_string();
                    }
                    // 通话中 Accept/Reject 无意义 (迟到命令), 忽略
                    _ => {}
                }
            }
            msg = update_rx.recv() => {
                let Some((req, reply)) = msg else { break "route channel closed".to_string() };
                let _ = reply.send(media::reinvite_answer(req.body(), local, rtp_ports, media));
            }
        }
    };
    Ok(reason)
}
