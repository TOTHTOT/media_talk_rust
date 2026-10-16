//! 共享类型与常量: session 模块内跨 worker/incoming/outgoing 共用.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use ipcam_gst::{TrackSource, VideoFrameSink};
use parking_lot::Mutex;
use rsipstack::dialog::DialogId;
use rsipstack::sip as rsip;
use tokio::sync::{mpsc, oneshot};

use crate::bridge::UiHandle;
pub use crate::bridge::WorkerCmd;

/// 被叫侧 RTP 端口默认值 (音频, 视频); 可通过 GuiSipConfig.rtp_ports 自定义
pub const DEFAULT_AUDIO_RTP_PORT: u16 = 40000;
pub const DEFAULT_VIDEO_RTP_PORT: u16 = 40002;

/// RTP 端口对: 本机用于接收 RTP 的音频/视频端口.
#[derive(Debug, Clone, Copy)]
pub struct RtpPorts {
    pub audio: u16,
    pub video: u16,
}

impl RtpPorts {
    pub const DEFAULT: Self = Self {
        audio: DEFAULT_AUDIO_RTP_PORT,
        video: DEFAULT_VIDEO_RTP_PORT,
    };
}

/// 来电振铃等待用户接听的最长时间, 超时自动拒接, 避免对端挂死在 ringing
pub const INCOMING_TIMEOUT: Duration = Duration::from_secs(30);

/// SIP 配置: 服务器账号 + 本机媒体源, 由 UI 入口装配.
/// `rtp_ports` 允许自定义, 默认 (40000, 40002).
#[derive(Debug)]
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
    /// 被叫侧 RTP 端口 (音频, 视频); 默认 RtpPorts::DEFAULT
    pub rtp_ports: RtpPorts,
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
            rtp_ports: RtpPorts::DEFAULT,
        }
    }
}

/// 当前通话的控制通道, Some 即占线
pub type CallCtlTx = Arc<Mutex<Option<mpsc::Sender<WorkerCmd>>>>;
pub type UpdateMsg = (rsip::Request, oneshot::Sender<Option<Vec<u8>>>);
/// 进行中的通话路由表: dialog id -> 通话任务的 re-INVITE 通道
pub type CallRoutes = Arc<Mutex<HashMap<DialogId, mpsc::Sender<UpdateMsg>>>>;
/// prepare_dial 全过返回的通话资源: (被叫 URI, 控制通道接收端)
pub type DialPrep = (rsip::Uri, mpsc::Receiver<WorkerCmd>);

/// 一路通话的公共上下文: 主叫 (OutgoingCtx 内嵌) 被叫 (直接当作 ctx)
/// 共用, 在 spawn 通话任务时打包, 整个通话生命周期内共享
pub(crate) struct CallBase {
    /// SIP 配置 (服务器/账号/媒体源/RTP 端口)
    pub cfg: Arc<GuiSipConfig>,
    /// UI 事件回投句柄
    pub ui: UiHandle,
    /// 视频帧回调 (对端画面送 GUI 上屏)
    pub tap: VideoFrameSink,
    /// 本机 IPv4
    pub local: Ipv4Addr,
    /// 用户操作控制通道 (接听/拒接/挂断)
    pub ctl_rx: mpsc::Receiver<WorkerCmd>,
    /// re-INVITE 更新消息通道 (forward_update 按 dialog id 路由过来)
    pub update_rx: mpsc::Receiver<UpdateMsg>,
}

/// 从 INVITE 的 From 头提取主叫号码 (sip:<user>@host 的 user 段)
pub fn caller_from_request(req: &rsip::Request) -> String {
    use rsipstack::sip::HeadersExt;
    req.from_header()
        .ok()
        .and_then(|h| h.uri().ok())
        .and_then(|u| u.auth.map(|a| a.user))
        .unwrap_or_else(|| "unknown".into())
}
