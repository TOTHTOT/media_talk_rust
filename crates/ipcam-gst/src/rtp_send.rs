//! 多源 → RTP 发送器 (SIP 通话联调用): 每路媒体独立指定源 (文件/RTSP/
//! 本机相机/麦克风), 解码后统一重编码发出.
//!
//! 链形 (每轨独立):
//!   视频: 源 → [解码] → queue → videoconvert
//!         → [videoscale → capsfilter(width≤max_width, 保持宽高比)]
//!         → x264enc → rtph264pay(config-interval=1) → udpsink
//!   音频: 源 → [解码] → queue → audioconvert → audioresample
//!         → capsfilter(8kHz/mono) → alawenc|mulawenc
//!         → rtppcmapay|rtppcmupay → udpsink
//!
//! 设计约束 (门口机黑屏三轮修复的经验, 全部固化在发送链里):
//! - x264enc aud=false + option-string slice-max-size=1300: 无 AUD,
//!   每个 NAL 小于 MTU, mode 0 对端不需要认 FU-A 分片
//! - rtph264pay config-interval=1: SPS/PPS 秒级周期重发, 对端中途
//!   开始收也能解
//! - udpsink sync=true: 按 buffer 时间戳限速, 否则以最快速度泼出去
//! - 发送 pt/音频编码必须取对端 answer 里的值 (RFC 3264)

mod chain;
mod config;
mod sender;
mod source;

pub use config::{AudioDest, RtpDest, RtpSendConfig, sendable_audio_codec};
pub use sender::{RtpSender, start_rtp_sender};
pub use source::{TrackSource, parse_track_source};
