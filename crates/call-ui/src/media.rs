//! 媒体面空壳 (Task 5 填实现): 本任务只定义 session.rs 需要的接口形状,
//! 让 worker 的调用点能编译. start_* 一律返回 None (不起任何管线),
//! renegotiate/stop 是空操作.

use ipcam_gst::VideoFrameSink;
use ipcam_sip::sdp::{OfferMedias, PeerMedias};

use crate::session::GuiSipConfig;

/// 一路通话的媒体句柄 (接收器 + 发送器), Task 5 填真实管线
pub struct MediaHandles;

impl MediaHandles {
    /// re-INVITE 重新协商: 编码/对端地址变了重启对应管线 (Task 5)
    pub fn renegotiate(&mut self, _offer: &OfferMedias) {}

    /// 停全部管线 (先发送后接收, Task 5 填)
    pub fn stop(self) {}
}

/// 被叫侧起媒体: 按协商结果起 RTP 接收器 (解码送 tap 上屏) + 本机源回传
pub fn start_incoming(
    _offer: &OfferMedias,
    _cfg: &GuiSipConfig,
    _tap: &VideoFrameSink,
) -> Option<MediaHandles> {
    None
}

/// 主叫侧起媒体: peers 是对端 answer 解析结果, rtp_ports 是 offer 里
/// 声明的本机接收端口 (占位 socket 已由调用方释放)
pub fn start_outgoing(
    _peers: &PeerMedias,
    _rtp_ports: (u16, u16),
    _cfg: &GuiSipConfig,
    _tap: &VideoFrameSink,
) -> Option<MediaHandles> {
    None
}
