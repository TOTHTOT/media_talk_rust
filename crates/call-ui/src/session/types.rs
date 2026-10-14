//! 共享类型与常量: session 模块内跨 worker/incoming/outgoing 共用.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use ipcam_gst::TrackSource;
use parking_lot::Mutex;
use rsipstack::dialog::DialogId;
use rsipstack::sip as rsip;
use tokio::sync::{mpsc, oneshot};

pub use crate::bridge::WorkerCmd;

/// 被叫侧 RTP 端口默认值 (音频, 视频); 可通过 GuiSipConfig.rtp_ports 自定义
pub(crate) const DEFAULT_AUDIO_RTP_PORT: u16 = 40000;
pub(crate) const DEFAULT_VIDEO_RTP_PORT: u16 = 40002;

/// 来电振铃等待用户接听的最长时间, 超时自动拒接, 避免对端挂死在 ringing
pub const INCOMING_TIMEOUT: Duration = Duration::from_secs(30);

/// SIP 配置: 服务器账号 + 本机媒体源, 由 UI 入口装配.
/// `rtp_ports` 允许自定义, 默认 (40000, 40002).
pub struct GuiSipConfig {
    /// SIP 服务器地址 (注册与呼叫都走它)
    pub server: SocketAddr,
    /// SIP 账号 (同时作为注册用户名)
    pub username: String,
    /// 账号密码 (digest 鉴权)
    pub password: String,
    /// 注册有效期 (秒), 续期按 75% 提前
    pub expires: u32,
    /// 强制绑定的本机 IPv4 网卡; None 走 ipcam_sip::local_ipv4() 自动选
    pub local_ip: Option<Ipv4Addr>,
    /// 通话时回传给对端的音频源 (麦克风/文件)
    pub audio_src: TrackSource,
    /// 通话时发给对端的视频源 (相机/文件/RTSP)
    pub video_src: TrackSource,
    /// 视频限宽 (对端解码能力, 超宽源降采样); 0 = 不限
    pub max_width: u32,
    /// 被叫侧 RTP 端口 (音频, 视频); 默认 (40000, 40002)
    pub rtp_ports: Option<(u16, u16)>,
}

impl Default for GuiSipConfig {
    fn default() -> Self {
        Self {
            server: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5060)),
            username: Default::default(),
            password: Default::default(),
            expires: 3600,
            local_ip: Default::default(),
            audio_src: TrackSource::Mic,
            video_src: TrackSource::LocalCamera { device: None },
            max_width: Default::default(),
            rtp_ports: Some((DEFAULT_AUDIO_RTP_PORT, DEFAULT_VIDEO_RTP_PORT)),
        }
    }
}

/// 当前通话的控制通道, Some 即占线
pub type CallSlot = Arc<Mutex<Option<mpsc::Sender<WorkerCmd>>>>;
pub type UpdateMsg = (rsip::Request, oneshot::Sender<Option<Vec<u8>>>);
/// 进行中的通话路由表: dialog id -> 通话任务的 re-INVITE 通道
pub type CallRoutes = Arc<Mutex<HashMap<DialogId, mpsc::Sender<UpdateMsg>>>>;
/// prepare_dial 全过返回的通话资源: (被叫 URI, 占位 socket 对, 控制通道接收端)
// rtp_ports 在 handle_outgoing 内从 rtp_sockets.local_addr().port() 算出
pub type DialPrep = (
    rsip::Uri,
    (std::net::UdpSocket, std::net::UdpSocket),
    mpsc::Receiver<WorkerCmd>,
);

/// 从 INVITE 的 From 头提取主叫号码 (sip:<user>@host 的 user 段)
pub fn caller_from_request(req: &rsip::Request) -> String {
    use rsipstack::sip::HeadersExt;
    req.from_header()
        .ok()
        .and_then(|h| h.uri().ok())
        .and_then(|u| u.auth.map(|a| a.user))
        .unwrap_or_else(|| "unknown".into())
}
