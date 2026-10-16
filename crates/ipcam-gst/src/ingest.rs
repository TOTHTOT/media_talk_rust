//! ingest 方向: 从 IP 相机拉 RTSP 流, 通过 webrtcsink 再发布到浏览器.
//! 文件划分:
//!   - session.rs: 入口 + 会话线程 + 重连状态机
//!   - pipeline.rs: 管线构建 (rtspsrc + webrtcsink)
//!   - track.rs:   track 链接 (video/audio pad-added 处理)
//!   - bus.rs:     bus 监控 + 统计 ticker
//!   - config.rs:  配置类型
//!   - stats.rs:   句柄与状态信号
//!   - tap.rs:     原始帧回调

pub(crate) mod bus;
pub(crate) mod config;
pub(crate) mod pipeline;
mod session;
pub(crate) mod stats;
pub(crate) mod tap;
pub(crate) mod track;

pub(crate) use session::start;
