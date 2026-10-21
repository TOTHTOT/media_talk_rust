//! SIP 后台 worker: 独立线程跑自己的 tokio runtime (GUI 主线程被 slint
//! 事件循环占着). 单线模型: 同时只有一路通话, CallCtlTx 占线时新来电
//! 直接回 486. 信令逻辑改写自 ipcam-sip 的 examples/answer.rs (被叫)
//! 和 examples/call.rs (主叫).
//!
//! 模块布局:
//! - `types.rs`: 共享类型与常量 (GuiSipConfig / WorkerCmd / CallCtlTx / CallRoutes / DialPrep)
//! - `worker.rs`: worker 生命周期与主循环 (spawn / setup / run / shutdown)
//! - `dispatch.rs`: 通话入口分发与槽位管理 (make_incoming_handler / handle_cmd / prepare_dial)
//! - `call_loop.rs`: 主被叫共用的已接通通话 loop (BYE / 挂断 / re-INVITE 重协商)
//! - `incoming.rs`: 被叫通话流程
//! - `outgoing.rs`: 主叫通话流程

mod call_loop;
mod dispatch;
mod incoming;
mod outgoing;
pub(crate) mod types;
mod worker;

pub use dispatch::{handle_cmd, make_incoming_handler};
pub use ipcam_sip::forward_update;
pub use types::{
    CallCtlTx, CallRoutes, DEFAULT_AUDIO_RTP_PORT, DEFAULT_VIDEO_RTP_PORT, DialPrep, GuiSipConfig,
    RtpPorts, UpdateMsg,
};
pub use worker::spawn;
