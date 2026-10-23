//! GStreamer-based RTSP → WebRTC republishing engine.
//!
//! Pulls an RTSP stream from an IP camera and republishes it to browsers
//! through `webrtcsink` (rswebrtc): the pipeline is
//! `rtspsrc → depay → parse → webrtcsink`, and every session registers
//! itself as a named producer on the process-wide signalling server
//! started by [`ensure_signalling_server`]. Session state and counters
//! are observable through [`GstStreamHandle`].
//!
//! The GStreamer pipeline requires GStreamer + pkg-config on the build
//! host; the bindings are an unconditional dependency of this crate.

use std::sync::OnceLock;

use gstreamer as gst;
use gstreamer::prelude::*;
use thiserror::Error;

mod gstutil;
mod ingest;
mod pkt_stats;
mod rtp_recv;
mod rtp_send;

pub use ingest::config::{AudioOutput, GstStreamConfig, ReconnectPolicy};
pub use ingest::stats::{GstStreamHandle, StreamState, StreamStats, StreamTerminal, TerminalHook};
pub use ingest::tap::{AudioChunkSink, RawAudioChunk, RawTaps, RawVideoFrame, VideoFrameSink};
pub use ipcam_core::AudioCodec;
pub use pkt_stats::PktStats;
pub use rtp_recv::{RtpReceiver, RtpRecvConfig, start_rtp_receiver};
pub use rtp_send::{
    AudioDest, RtpDest, RtpSendConfig, RtpSender, TrackSource, parse_track_source,
    sendable_audio_codec, start_rtp_sender,
};

fn ensure_init_internal() -> Result<(), GstStreamError> {
    static DONE: OnceLock<Result<(), GstStreamError>> = OnceLock::new();
    DONE.get_or_init(|| gst::init().map_err(|e| GstStreamError::Init(format!("gst init: {e}"))))
        .clone()
}

/// 探测 GStreamer 元件是否可用 (内部先确保 gst 已初始化).
/// 供二进制入口在启动期做缺件早报, 如 serve 依赖 webrtcsink
/// (gst-plugins-rs), 设备没装时直接报错而不是等流起来才挂
pub fn element_available(name: &str) -> bool {
    ensure_init_internal().is_ok() && gst::ElementFactory::find(name).is_some()
}

#[derive(Debug, Clone, Error)]
pub enum GstStreamError {
    /// Static configuration error, reported by [`validate`].
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    /// GStreamer init / missing element (e.g. `rtspsrc` from plugins-good).
    #[error("init failed: {0}")]
    Init(String),
    /// Connection failure (auth failures included; the message carries
    /// 401/Unauthorized wording to stay compatible with `probe`).
    #[error("connect failed: {0}")]
    Connect(String),
    /// Runtime bus ERROR during playback.
    #[error("stream error: {0}")]
    Stream(String),
    /// Video/audio branch link failure.
    #[error("link error: {0}")]
    Link(String),
}

/// Validate a stream configuration without touching the network.
///
/// # Arguments
///
/// * `cfg`: 流配置
///
/// returns: Result<(), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub fn validate(cfg: &GstStreamConfig) -> Result<(), GstStreamError> {
    ingest::config::validate(cfg)
}

/// gio::Socket 的 Send+Sync 包装: GSocket 官方文档明确 threadsafe
/// (https://docs.gtk.org/gio/class.Socket.html), 但 gio 0.22 的 Rust
/// 绑定漏标了 (同包的 SocketAddress 倒是标了). udpsrc/udpsink 共享
/// socket 要跨 worker 线程和 GStreamer 流线程传递, 必须过 Send+Sync
/// 边界, 这里按官方文档补标
#[derive(Debug, Clone)]
pub struct SharedUdpSocket(gio::Socket);

unsafe impl Send for SharedUdpSocket {}
unsafe impl Sync for SharedUdpSocket {}

impl SharedUdpSocket {
    /// 取内部的 gio::Socket (设置 udpsrc/udpsink 的 "socket" 属性用)
    pub fn as_gio(&self) -> &gio::Socket {
        &self.0
    }
}

/// 绑定一个 UDP GSocket, 供 udpsrc/udpsink 的 "socket" 属性共享 —
/// 对称 RTP: 发送源端口 == 接收端口 == SDP 声明端口. 经过媒体锚定的
/// 服务器 (B2BUA 中继, 如 FreeSWITCH/rtpengine 按发送源地址 latch
/// 转发) 时这是互通前提: 源端口和声明端口不一致, 对端媒体会被中继
/// 投到无人监听的临时端口, 收端永远 0 包 (2026-10-18 实测复现).
///
/// 绑非 0 端口时不带 SO_REUSEADDR (allow_reuse=false): 独占语义, 端口
/// 冲突真实报错, 便于探测 (GStreamer 自建 socket 默认 reuse, 同机两个
/// 进程能静默绑同一端口, 然后 Windows 把单播包全投给先绑者). 绑 0 让
/// OS 分配临时端口, 用 [`udp_socket_port`] 查实际值.
///
/// # Arguments
///
/// * `ip`: 绑定的本机地址
/// * `port`: 端口, 0 = OS 分配临时端口
///
/// returns: io::Result<SharedUdpSocket> — 已绑定且设为非阻塞的 socket
pub fn bind_udp_socket(ip: std::net::IpAddr, port: u16) -> std::io::Result<SharedUdpSocket> {
    use gio::prelude::*;
    let family = match ip {
        std::net::IpAddr::V4(_) => gio::SocketFamily::Ipv4,
        std::net::IpAddr::V6(_) => gio::SocketFamily::Ipv6,
    };
    let sock = gio::Socket::new(family, gio::SocketType::Datagram, gio::SocketProtocol::Udp)
        .map_err(std::io::Error::other)?;
    let addr = gio::InetSocketAddress::new(&gio::InetAddress::from(ip), port);
    sock.bind(&addr, false).map_err(std::io::Error::other)?;
    // GStreamer 自建 socket 会设非阻塞; 外部传入的它不设, 补上
    sock.set_blocking(false);
    Ok(SharedUdpSocket(sock))
}

/// 查询已绑定 UDP socket 的实际本地端口 (配合 bind 0 的临时端口分配)
///
/// # Arguments
///
/// * `sock`: [`bind_udp_socket`] 绑好的 socket
///
/// returns: io::Result<u16> — 本地端口
pub fn udp_socket_port(sock: &SharedUdpSocket) -> std::io::Result<u16> {
    use gio::prelude::*;
    let addr = sock.0.local_address().map_err(std::io::Error::other)?;
    let inet = addr
        .downcast::<gio::InetSocketAddress>()
        .map_err(|_| std::io::Error::other("local address is not inet"))?;
    Ok(inet.port())
}

/// Anchor pipeline hosting the process-wide WebRTC signalling server.
static SIGNALLING_ANCHOR: OnceLock<gst::Pipeline> = OnceLock::new();

/// Start the shared WebRTC signalling server (default 0.0.0.0:8443) if
/// it is not running yet. The server lives on an anchor `webrtcsink`
/// held in a static for the process lifetime; every streaming session
/// then connects to it as a producer via `signalling_host/port`.
/// Idempotent — safe to call from every server startup.
///
/// returns: Result<(), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub fn ensure_signalling_server() -> Result<(), GstStreamError> {
    ensure_init_internal()?;
    if SIGNALLING_ANCHOR.get().is_some() {
        return Ok(());
    }
    let pipeline = gst::Pipeline::new();
    let ws = gst::ElementFactory::make("webrtcsink")
        .build()
        .map_err(|e| GstStreamError::Init(format!("missing element `webrtcsink`: {e}")))?;
    ws.set_property("run-signalling-server", true);
    ws.set_property_from_str("meta", "meta,name=signalling-anchor");
    pipeline
        .add(&ws)
        .map_err(|e| GstStreamError::Init(format!("anchor pipeline add webrtcsink: {e}")))?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| GstStreamError::Init(format!("anchor pipeline set Playing: {e}")))?;
    // Lost a race with a concurrent caller: tear the duplicate down.
    if SIGNALLING_ANCHOR.set(pipeline.clone()).is_err() {
        let _ = pipeline.set_state(gst::State::Null);
    }
    Ok(())
}

/// Start a streaming session: build the RTSP → webrtcsink pipeline and
/// enter the background event loop. The stream becomes visible to WebRTC
/// consumers under `cfg.stream_name` once the pipeline reaches Playing.
///
/// # Arguments
///
/// * `cfg`: 流配置
///
/// returns: Result<GstStreamHandle, GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub fn start(cfg: GstStreamConfig) -> Result<GstStreamHandle, GstStreamError> {
    start_with_taps(cfg, RawTaps::default())
}

/// Like [`start`], additionally teeing decoded frames into `taps`
/// (native GUI / analytics). Each set tap adds a decode branch; with no
/// taps this is identical to [`start`].
///
/// # Arguments
///
/// * `cfg`: 流配置
/// * `taps`: 原始帧回调 (视频/音频解码后 tee 分出)
///
/// returns: Result<GstStreamHandle, GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub fn start_with_taps(
    cfg: GstStreamConfig,
    taps: RawTaps,
) -> Result<GstStreamHandle, GstStreamError> {
    validate(&cfg)?;
    ingest::start(cfg, taps)
}
