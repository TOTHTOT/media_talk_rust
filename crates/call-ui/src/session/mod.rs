//! SIP 后台 worker: 独立线程跑自己的 tokio runtime (GUI 主线程被 slint
//! 事件循环占着). 单线模型: 同时只有一路通话, CallCtlTx 占线时新来电
//! 直接回 486. 信令逻辑改写自 ipcam-sip 的 examples/answer.rs (被叫)
//! 和 examples/call.rs (主叫).
//!
//! 模块布局:
//! - `types.rs`: 共享类型与常量 (GuiSipConfig / WorkerCmd / CallCtlTx / CallRoutes / DialPrep)
//! - `worker.rs`: 主循环与命令处理 (spawn / setup / run / shutdown / handle_cmd / prepare_dial / forward_update)
//! - `incoming.rs`: 被叫通话流程
//! - `outgoing.rs`: 主叫通话流程

mod incoming;
mod outgoing;
pub(crate) mod types;
mod worker;

pub use types::{CallCtlTx, CallRoutes, DialPrep, GuiSipConfig, UpdateMsg};
pub use worker::{forward_update, handle_cmd, make_incoming_handler, spawn};
