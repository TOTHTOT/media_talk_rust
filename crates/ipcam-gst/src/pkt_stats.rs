//! RTP 包计数器: send/recv 共用, 在 payloader/depay 的 src pad 上
//! 安装 probe, 每经过一个包计数器 +1, 用来回答"数据有没有流动".

use std::sync::atomic::AtomicU64;

/// RTP 包计数 (payloader/depay src pad 上的 probe 累加)
#[derive(Default, Debug)]
pub struct PktStats {
    pub video_pkts: AtomicU64,
    pub audio_pkts: AtomicU64,
}

impl PktStats {
    pub fn new() -> Self {
        Self::default()
    }
}
