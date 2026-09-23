//! ingest 方向: 从 IP 相机拉 RTSP 流, 通过 webrtcsink 再发布到浏览器.
//! 管线组装与会话生命周期在 session.rs, 配置/统计/tap 各自独立成文件.

pub(crate) mod config;
mod session;
pub(crate) mod stats;
pub(crate) mod tap;

pub(crate) use session::start;
