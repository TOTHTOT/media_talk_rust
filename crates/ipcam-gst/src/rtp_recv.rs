//! RTP 接收器: 从 UDP 端口接收 RTP 流, 音视频合并保存为 mp4.
//!
//! 用于 SIP 通话场景 (answer.rs), 被动接收对端发送的音视频 RTP 包.
//!
//! # 管线拓扑
//!
//! 视频: `udpsrc(port) → rtph264depay → h264parse → mp4mux.video_%u`
//!
//! 音频: `udpsrc(port) → rtppcmxdepay → mulawdec/alawdec → opusenc → mp4mux.audio_%u`
//!       (mp4 不认 G.711 载荷, 转码成 opus)
//!
//! # 设计约束
//!
//! - `udpsrc` 绑定 `0.0.0.0` 接受任意来源的 RTP 包
//! - mp4 的 moov 索引只在 EOS 时写入, 必须走 `RtpReceiver::stop()` 收尾

mod config;
mod receiver;

pub use config::RtpRecvConfig;
pub use receiver::{RtpReceiver, start_rtp_receiver};
