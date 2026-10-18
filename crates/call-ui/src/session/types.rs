//! 共享类型与常量: session 模块内跨 worker/incoming/outgoing 共用.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use ipcam_gst::{TrackSource, VideoFrameSink};
pub use ipcam_sip::{CallRoutes, UpdateMsg};
use parking_lot::Mutex;
use rsipstack::sip as rsip;
use tokio::sync::mpsc;
use tracing::warn;

use crate::bridge::UiHandle;
pub use crate::bridge::WorkerCmd;

/// 被叫侧 RTP 端口默认值 (音频, 视频); 可通过 GuiSipConfig.rtp_ports 自定义
pub const DEFAULT_AUDIO_RTP_PORT: u16 = 40000;
pub const DEFAULT_VIDEO_RTP_PORT: u16 = 40002;

/// RTP 端口对: 本机用于接收 RTP 的音频/视频端口.
/// 语义是"被叫首选端口": 主叫永远用临时端口 (见 `bind_recv_sockets`),
/// 被叫优先用它, 被占时回退临时端口
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// 一路通话的 RTP socket 对 (音频, 视频): udpsrc/udpsink 共享同一
/// socket — 对称 RTP, 发送源端口 == 接收端口 == SDP 声明端口, 过媒体
/// 锚定服务器 (按发送源 latch 转发) 的互通前提, 见 ipcam_gst::bind_udp_socket.
/// 从信令期 (端口写进 offer/answer) 一直持有到通话结束
pub struct RecvSockets {
    pub audio: ipcam_gst::SharedUdpSocket,
    pub video: ipcam_gst::SharedUdpSocket,
}

/// 为本路通话绑定 RTP socket 并确定接收端口.
///
/// `preferred: Some` (被叫) 先试配置端口, 被占 (典型: 同机还跑着别的
/// 通话进程) 回退临时端口; `None` (主叫) 直接临时端口.
///
/// 为什么主叫不能先试配置端口: 同机对跑 answer 示例时它对端也是
/// 40000/40002, 双方声明相同接收端口会让两路发送互相投错 socket —
/// 后绑方靠 SO_REUSEADDR 能绑上, 但 Windows 把单播 UDP 全投给先绑的
/// socket, 表现就是收端 stats 永远 0 包 (2026-10-18 实测复现). 主叫
/// 用临时端口后两进程各绑各的, 从根上避开.
pub fn bind_recv_sockets(
    local: Ipv4Addr,
    preferred: Option<RtpPorts>,
) -> std::io::Result<(RtpPorts, RecvSockets)> {
    let bind_pair = |audio_port: u16, video_port: u16| {
        let audio = ipcam_gst::bind_udp_socket(std::net::IpAddr::V4(local), audio_port)?;
        let video = ipcam_gst::bind_udp_socket(std::net::IpAddr::V4(local), video_port)?;
        let ports = RtpPorts {
            audio: ipcam_gst::udp_socket_port(&audio)?,
            video: ipcam_gst::udp_socket_port(&video)?,
        };
        Ok::<_, std::io::Error>((ports, RecvSockets { audio, video }))
    };
    if let Some(preferred) = preferred {
        match bind_pair(preferred.audio, preferred.video) {
            Ok(ok) => return Ok(ok),
            Err(e) => {
                let (ports, sockets) = bind_pair(0, 0)?;
                warn!(error = %e, ?preferred, fallback = ?ports, "preferred RTP ports unavailable, fall back to ephemeral");
                return Ok((ports, sockets));
            }
        }
    }
    bind_pair(0, 0)
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
    /// 被叫侧 RTP 首选端口 (音频, 视频); 默认 RtpPorts::DEFAULT.
    /// 主叫不用它 (永远临时端口), 被叫被占时回退临时端口, 见 `bind_recv_sockets`
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

/// 当前通话的控制通道发送端 (存入槽位, UI 发 Hangup 时取出发送).
/// Some = 通话中/响铃中, None = 空闲. WorkerCmd::Hangup → call_ctl_tx.try_send()
pub type CallCtlTx = Arc<Mutex<Option<mpsc::Sender<WorkerCmd>>>>;

/// Dial 预检查返回: (被叫 URI, 控制通道接收端) 给 handle_outgoing 用.
/// 被叫 URI 来自 prepare_dial 的 callee_uri() 转换.
///
/// ```mermaid
/// sequenceDiagram
///     participant UI
///     participant handle_cmd
///     participant prepare_dial
///     participant CTX[CallCtlTx slot]
///
///     UI->>+handle_cmd: cmd_tx.send(Dial)
///     handle_cmd->>+prepare_dial: prepare_dial()
///     prepare_dial->>CTX: lock()
///     prepare_dial->>CTX: slot = Some(call_ctl_tx)
///     prepare_dial-->>-handle_cmd: (callee, call_answer_ctl_rx)
///     Note over handle_cmd: spawn handle_outgoing(call_answer_ctl_rx)
/// ```
pub type DialPrep = (rsip::Uri, mpsc::Receiver<WorkerCmd>);

/// 一路通话的公共上下文: 主叫 (OutgoingCtx 内嵌) 被叫 (直接当作 ctx)
/// 共用, 在 spawn 通话任务时打包, 整个通话生命周期内共享
///
/// ```mermaid
/// sequenceDiagram
///     participant handle_cmd
///     participant CTX[CallCtlTx slot]
///     participant call_loop
///     participant RSL[run_dialog_state_loop]
///
///     Note over handle_cmd: Dial 时
///     handle_cmd->>CTX: slot = Some(call_ctl_tx)
///     handle_cmd->>call_loop: spawn(call_answer_ctl_rx, update_rx)
///
///     Note over CTX: Hangup 时
///     CTX-->>call_loop: call_ctl_tx.send(Hangup)
///     call_loop->>call_loop: dialog.bye_with_headers()
///
///     Note over RSL: re-INVITE 时
///     RSL->>call_loop: update_rx.send((req, reply_tx))
///     call_loop->>RSL: reply_tx.send(answer SDP)
/// ```
pub(crate) struct CallBase {
    /// SIP 配置 (服务器/账号/媒体源/RTP 端口)
    pub cfg: Arc<GuiSipConfig>,
    /// UI 事件回投句柄
    pub ui: UiHandle,
    /// 视频帧回调 (对端画面送 GUI 上屏)
    pub tap: VideoFrameSink,
    /// 本机 IPv4 (用于 re-INVITE answer 的 c= 行)
    pub local: Ipv4Addr,
    /// 用户操作控制通道接收端: Hangup 时 call_ctl_tx 取出发送, 这里接收.
    /// 被叫等待时收到 Hangup → Reject; 通话中收到 Hangup → BYE
    pub call_answer_ctl_rx: mpsc::Receiver<WorkerCmd>,
    /// re-INVITE 更新消息通道接收端: forward_update 经 CallRoutes 路由,
    /// 把请求发到 update_rx, 通话任务处理后经 oneshot 回 answer SDP
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

#[cfg(test)]
mod tests {
    use super::*;

    const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

    /// 主叫 (preferred=None): 总是拿到非 0 的临时端口, 音/视频不同;
    /// socket 活着期间端口确实被占住
    #[test]
    fn outgoing_always_gets_ephemeral_ports() {
        let (ports, _sockets) = bind_recv_sockets(LOOPBACK, None).unwrap();
        assert_ne!(ports.audio, 0);
        assert_ne!(ports.video, 0);
        assert_ne!(ports.audio, ports.video);
        assert!(
            std::net::UdpSocket::bind((LOOPBACK, ports.audio)).is_err(),
            "socket must keep the audio port occupied"
        );
    }

    /// 被叫首选端口空闲时按配置绑; 端口值原样返回
    #[test]
    fn incoming_binds_preferred_when_free() {
        // 先问 OS 要两个临时端口再立刻释放, 得到两个当前大概率空闲的
        // 端口号当"配置值", 避免测试钉死端口撞车
        let (free, _tmp) = bind_recv_sockets(LOOPBACK, None).unwrap();
        drop(_tmp);
        let (ports, _sockets) = bind_recv_sockets(LOOPBACK, Some(free)).unwrap();
        assert_eq!(ports, free);
    }

    /// 被叫首选端口已被占 (同机另一个通话进程) 时回退临时端口,
    /// 且回退端口不等于被占的配置值
    #[test]
    fn incoming_falls_back_when_preferred_occupied() {
        let (occupied, _blockers) = bind_recv_sockets(LOOPBACK, None).unwrap();
        let (ports, _sockets) = bind_recv_sockets(LOOPBACK, Some(occupied)).unwrap();
        assert_ne!(ports.audio, occupied.audio);
        assert_ne!(ports.video, occupied.video);
        assert_ne!(ports.audio, ports.video);
    }
}
