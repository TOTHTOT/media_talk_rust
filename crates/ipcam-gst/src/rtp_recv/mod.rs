//! RTP 接收器: 从 UDP 端口接收 RTP 流并保存到文件.
//!
//! 用于 SIP 通话场景 (answer.rs), 被动接收对端发送的音视频 RTP 包.
//!
//! # 管线拓扑
//!
//! 视频: `udpsrc(port) → rtph264depay → h264parse → filesink`
//!
//! 音频: `udpsrc(port) → rtppcmadepay → filesink`
//!
//! # 设计约束
//!
//! - `udpsrc` 绑定 `0.0.0.0` 接受任意来源的 RTP 包
//! - `filesink` 以 append 模式打开, 支持断点续写
//! - 不做解码, 直接保存编码后的 H.264 / G.711 原始数据

pub mod config;
pub mod receiver;

pub use config::RtpRecvConfig;
pub use receiver::{RtpReceiver, start_rtp_receiver};
