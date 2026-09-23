//! SIP 客户端库: 注册/续期/注销 (client), 来电事务分发与 dialog 状态
//! 循环 (incoming), SDP offer/answer 构建与解析 (sdp). 基于 rsipstack.

mod client;
mod incoming;
pub mod sdp;

pub use client::{SipClient, SipClientConfig, local_ipv4};
pub use incoming::run_dialog_state_loop;
