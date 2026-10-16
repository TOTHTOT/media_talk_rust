//! 主被叫共用的通话期逻辑: 已接通通话 loop (对端 BYE / 本地挂断 /
//! re-INVITE 重协商) 与通话任务收尾 (结果映射 UI 事件 + 清占线槽位).

use std::net::Ipv4Addr;

use anyhow::Result;
use rsipstack::dialog::invite_dialog::InviteDialog;
use tokio::sync::mpsc;
use tracing::warn;

use super::types::{CallCtlTx, RtpPorts, UpdateMsg, WorkerCmd};
use crate::app::UiEvent;
use crate::bridge::UiHandle;
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

/// 通话任务收尾: inner 结果映射成 UI 事件 (Ok → Terminated, Err →
/// warn 日志 + Failed), 最后清占线槽位释放呼叫能力. 路由摘除由调用
/// 方自理 (被叫手动 remove, 主叫靠 RouteGuard).
///
/// # Arguments
///
/// * `ui`: UI 事件回投句柄
/// * `slot`: 通话占线槽位
/// * `desc`: 通话描述 (失败日志用, 如 "incoming call" / "outgoing call to 1001")
/// * `res`: 通话 inner 流程结果, Ok 为正常结束原因, Err 为失败
///
/// returns: ()
pub(super) fn finish_call(ui: &UiHandle, slot: &CallCtlTx, desc: &str, res: Result<String>) {
    match res {
        Ok(reason) => ui.post(UiEvent::Terminated { reason }),
        Err(e) => {
            warn!(error = %e, "{desc} failed");
            ui.post(UiEvent::Failed {
                reason: e.to_string(),
            });
        }
    }
    *slot.lock() = None;
}
