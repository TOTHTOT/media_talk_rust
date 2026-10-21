//! 媒体面: 按 SDP 协商结果起停 RTP 收发管线, 改写自 ipcam-sip 的
//! examples/answer.rs (CallMedia 指纹重启) 和 examples/call.rs
//! (PeerMedias → AudioDest/RtpDest 装配).
//!
//! 收端: 音频解码进 autoaudiosink (playback), 视频解码后经 video_tap
//! 吐 RGBA 帧给 slint 纹理 (取代 autovideosink). 发端: 按 GuiSipConfig
//! 的 audio_src/video_src 采集回传, 视频发送链只有 H264 (x264enc),
//! 对端协商成其他编码时该路只收不发. 单路失败只 warn 不抛错,
//! 通话不因一路媒体挂掉 (同 answer.rs 原则).
//!
//! 协商计划 (MediaPlan/VideoDest/codec 谓词) 在子模块 plan.

mod plan;

use std::net::{IpAddr, Ipv4Addr};

use ipcam_core::{AudioCodec, VideoCodec};
use ipcam_gst::{
    AudioDest, RtpDest, RtpReceiver, RtpRecvConfig, RtpSendConfig, RtpSender, TrackSource,
    VideoFrameSink, start_rtp_receiver, start_rtp_sender,
};
use ipcam_sip::negotiate;
use ipcam_sip::sdp::{OfferMedias, PeerMedias, parse_offer_all};
use tracing::{info, warn};

use crate::session::types::{GuiSipConfig, RecvSockets, RtpPorts};

use plan::{MediaPlan, VideoDest, peer_audio_send_params, peer_video_send_params, plan_from_offer};
pub(crate) use plan::{audio_pred, video_pred};

/// 一路通话的媒体句柄: 接收器 + 发送器 + 协商方案. re-INVITE 时按
/// MediaPlan 比对 (recv 编码对 / send 地址/pt/编码), 变了才重启
/// 对应管线, 没变只回 answer 不动管线 (同 answer.rs CallMedia)
pub struct MediaHandles {
    receiver: Option<RtpReceiver>,
    sender: Option<RtpSender>,
    /// 当前生效的协商方案 (指纹)
    plan: MediaPlan,
    /// 本机接收端口 (音频, 视频): 被叫是固定常量, 主叫是 offer 里
    /// 声明的端口, 接收器重启 (re-INVITE 改编码) 时用
    recv_ports: RtpPorts,
    /// 对称 RTP 共享 socket (音频, 视频): udpsrc/udpsink 同绑一对,
    /// 端口在 dialog 存续期不变, renegotiate 重启管线时复用
    sockets: RecvSockets,
    /// 本机回传源配置, renegotiate 重启发送器时用
    audio_src: TrackSource,
    video_src: TrackSource,
    max_width: u32,
    /// 视频帧 tap: VideoFrameSink 是 Arc, 接收器重启时 clone 复用
    tap: VideoFrameSink,
}

impl MediaHandles {
    /// 按协商方案起接收器和发送器. 单路不可用时该路 None (只收不发 /
    /// 只发不收), 不抛错.
    ///
    /// # Arguments
    ///
    /// * `recv_ports`: 本机 RTP 接收端口 (音频, 视频)
    /// * `cfg`: 本机媒体源与视频限宽配置
    /// * `tap`: 视频帧回调 (GUI 纹理通路), 接收器重启时 Arc clone 复用
    /// * `plan`: 协商产物 (收端指纹 + 两路发送目标)
    /// * `sockets`: 对称 RTP 共享 socket (音频, 视频), udpsrc/udpsink 复用
    ///
    /// returns: MediaHandles — 媒体句柄 (两路都不可用时是空句柄, 通话保持)
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    fn new(
        recv_ports: RtpPorts,
        cfg: &GuiSipConfig,
        tap: VideoFrameSink,
        plan: MediaPlan,
        sockets: RecvSockets,
    ) -> Self {
        info!("start build gst pipeline");
        let receiver = start_receiver(
            tap.clone(),
            recv_ports,
            plan.recv.0,
            plan.recv.1,
            &sockets,
            cfg.max_width,
        );
        let sender = start_sender(
            &cfg.audio_src,
            &cfg.video_src,
            plan.send_audio,
            plan.send_video.clone(),
            &sockets,
        );
        Self {
            receiver,
            sender,
            plan,
            recv_ports,
            sockets,
            audio_src: cfg.audio_src.clone(),
            video_src: cfg.video_src.clone(),
            max_width: cfg.max_width,
            tap,
        }
    }

    /// re-INVITE 重新协商 (照 answer.rs CallMedia::renegotiate): 编码
    /// 变了重启接收器 (tap 复用); 对端收包地址/pt/编码变了重启发送器
    pub fn renegotiate(&mut self, offer: &OfferMedias) {
        let new_plan = plan_from_offer(offer, self.max_width);
        if new_plan.recv != self.plan.recv {
            info!(old = ?self.plan.recv, new = ?new_plan.recv, "codec changed, restarting receiver");
            if let Some(r) = self.receiver.take() {
                r.stop();
            }
            self.receiver = start_receiver(
                self.tap.clone(),
                self.recv_ports,
                new_plan.recv.0,
                new_plan.recv.1,
                &self.sockets,
                self.max_width,
            );
        }
        let send_changed = new_plan.send_audio != self.plan.send_audio
            || new_plan.send_video != self.plan.send_video;
        if send_changed {
            info!(
                audio_changed = new_plan.send_audio != self.plan.send_audio,
                video_changed = new_plan.send_video != self.plan.send_video,
                "send dest changed, restarting sender"
            );
            if let Some(s) = self.sender.take() {
                s.stop();
            }
            self.sender = start_sender(
                &self.audio_src,
                &self.video_src,
                new_plan.send_audio,
                new_plan.send_video.clone(),
                &self.sockets,
            );
        }
        self.plan = new_plan;
    }

    /// 先停发送器再停接收器, 顺序不能反 — 发送器没停会继续往外发
    /// RTP, 对端以为我们没挂断
    pub fn stop(mut self) {
        if let Some(s) = self.sender.take() {
            s.stop();
        }
        if let Some(r) = self.receiver.take() {
            r.stop();
        }
    }
}

/// 处理 re-INVITE 新 offer: 按当前配置重新协商 answer, 并驱动媒体层按
/// 指纹比对重启管线; 主叫/被叫通话 loop 共用. offer 解析失败返回 None
/// (调用方回不带 SDP 的 200, 不动本地媒体).
///
/// # Arguments
///
/// * `offer_body`: re-INVITE 请求体 (SDP offer)
/// * `local`: 本机 IPv4 (answer 的 c= 行)
/// * `rtp_ports`: 本机 RTP 接收端口 (音频, 视频), 必须与初始协商一致
/// * `media`: 媒体句柄, 协商结果变了由它重启对应管线
///
/// returns: Option<Vec<u8>> — answer SDP body, 解析失败返回 None
///
/// # Examples
///
/// ```
///
/// ```
pub fn reinvite_answer(
    offer_body: &[u8],
    local: Ipv4Addr,
    rtp_ports: RtpPorts,
    media: &mut Option<MediaHandles>,
) -> Option<Vec<u8>> {
    let new_offer = match parse_offer_all(offer_body) {
        Ok(o) => o,
        Err(e) => {
            warn!(error = %e, "failed to parse re-INVITE offer, reply 200 without SDP");
            return None;
        }
    };
    let (_, _, answer) = negotiate::build_answer(
        &new_offer,
        IpAddr::V4(local),
        rtp_ports.audio,
        rtp_ports.video,
        audio_pred,
        video_pred,
    );
    info!("re-INVITE accepted, answer SDP:\n{answer}");
    if let Some(m) = media.as_mut() {
        m.renegotiate(&new_offer);
    }
    Some(answer.into_bytes())
}

/// 起 RTP 接收器: 音频解码后进 autoaudiosink (扬声器), 视频解码后经
/// tap 吐 RGBA 帧给 GUI 上屏 (取代 autovideosink).
///
/// 失败只 warn 返回 None, 通话继续 (只发不收), 同 answer.rs 原则.
/// 重启接收器 (re-INVITE 改编码) 时调用方负责先 stop 旧的再调本函数.
///
/// # Arguments
///
/// * `tap`: 视频帧回调 (GUI 纹理通路), 重启时 Arc clone 复用
/// * `ports`: 本机接收端口 (音频, 视频)
/// * `audio`: 要收的音频编码, None 表示音频路不建
/// * `video`: 要收的视频编码, None 表示视频路不建
/// * `sockets`: 对称 RTP 共享 socket, 启用的那几路 udpsrc 直接复用
/// * `max_width`: tap 显示限宽, 0 表示不限 (透传成 None)
///
/// returns: Option<RtpReceiver> — 运行中的接收器句柄, stop 即拆管线
///
/// # Examples
///
/// ```
///
/// ```
fn start_receiver(
    tap: VideoFrameSink,
    ports: RtpPorts,
    audio: Option<AudioCodec>,
    video: Option<VideoCodec>,
    sockets: &RecvSockets,
    max_width: u32,
) -> Option<RtpReceiver> {
    if audio.is_none() && video.is_none() {
        warn!("audio or video is null, skip build recv gst pipeline");
        // RtpRecvConfig 要求至少一路媒体, 无媒体可收就不起管线
        return None;
    }
    match start_rtp_receiver(RtpRecvConfig {
        path: None,
        playback: true,
        video_tap: Some(tap),
        // 显示通路限宽复用 max_width (0 = 不限): 屏幕 widget 就那么大,
        // 全尺寸帧只是白搬 (memcpy + 纹理上传带宽)
        tap_max_width: (max_width > 0).then_some(max_width),
        video: video.map(|c| (ports.video, c)),
        audio: audio.map(|c| (ports.audio, c)),
        video_socket: video.map(|_| sockets.video.clone()),
        audio_socket: audio.map(|_| sockets.audio.clone()),
    }) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!(error = %e, "failed to start RTP receiver, send only");
            None
        }
    }
}

/// 起 RTP 发送器: 按本机媒体源 (mic/camera/file) 回传音视频. 视频
/// 发送链只有 H264 (x264enc), 对端协商成 H265 时该路只收不发; 单路
/// 不可用只 warn, 两路都不可用不起管线返回 None.
///
/// # Arguments
///
/// * `audio_src`: 音频回传源 (麦克风/文件)
/// * `video_src`: 视频回传源 (相机/文件/RTSP)
/// * `audio`: 音频发送目标 (地址/pt/编码), None 表示不发音频
/// * `video`: 视频发送目标 (地址/pt/编码/限宽), None 表示不发视频
/// * `sockets`: 对称 RTP 共享 socket, 启用的那几路 udpsink 经它发
///   (源端口 == SDP 声明的接收端口)
///
/// returns: Option<RtpSender> — 运行中的发送器句柄, stop 即拆管线
///
/// # Examples
///
/// ```
///
/// ```
fn start_sender(
    audio_src: &TrackSource,
    video_src: &TrackSource,
    audio: Option<AudioDest>,
    video: Option<VideoDest>,
    sockets: &RecvSockets,
) -> Option<RtpSender> {
    let video = video.and_then(|dest| match dest.codec {
        VideoCodec::H264 => Some((
            video_src.clone(),
            RtpDest {
                addr: dest.addr,
                payload_type: dest.payload_type,
                max_width: dest.max_width,
            },
        )),
        other => {
            warn!(codec = ?other, "video send chain is H264 only, receive only for video");
            None
        }
    });
    if audio.is_none() && video.is_none() {
        return None;
    }
    let (has_audio, has_video) = (audio.is_some(), video.is_some());
    match start_rtp_sender(RtpSendConfig {
        audio: audio.map(|d| (audio_src.clone(), d)),
        video,
        audio_socket: has_audio.then(|| sockets.audio.clone()),
        video_socket: has_video.then(|| sockets.video.clone()),
    }) {
        Ok(s) => {
            info!("rtp sender started");
            Some(s)
        }
        Err(e) => {
            warn!(error = %e, "failed to start RTP sender, receive only");
            None
        }
    }
}

/// 被叫侧起媒体: 按协商结果起 RTP 接收器 (解码送 tap 上屏) + 本机源
/// 回传 (音频目标从 offer 协商, 视频目标从 offer.video 选中编码)
///
/// # Arguments
///
/// * `offer`: 解析后的对端 SDP (音视频媒体列表 + 编码候选)
/// * `recv_ports`: 本机 RTP 接收端口 (音频, 视频), 必须是 answer SDP
///   里声明的那批 (session 层已绑好共享 socket)
/// * `sockets`: 对称 RTP 共享 socket (音频, 视频), udpsrc/udpsink 复用
/// * `cfg`: 本机媒体源与视频限宽配置
/// * `tap`: 视频帧回调, 接收器视频路经它上屏, 重启时复用
///
/// returns: Option<MediaHandles> — 媒体句柄 (两路都不可用时是空句柄, 通话保持)
///
/// # Examples
///
/// ```
///
/// ```
pub fn start_incoming(
    offer: &OfferMedias,
    recv_ports: RtpPorts,
    sockets: RecvSockets,
    cfg: &GuiSipConfig,
    tap: &VideoFrameSink,
) -> Option<MediaHandles> {
    Some(MediaHandles::new(
        recv_ports,
        cfg,
        tap.clone(),
        plan_from_offer(offer, cfg.max_width),
        sockets,
    ))
}

/// 主叫侧起媒体: peers 是对端 answer 解析结果, rtp_ports 本机接收端口.
/// 发送 pt/编码必须用 answer 里的值 — answer 收窄成什么就发什么 (RFC 3264, 同 call.rs)
///
/// # Arguments
///
/// * `peers`: 对端 answer 解析出的音视频协商结果
/// * `rtp_ports`: 本机 RTP 接收端口 (音频, 视频)
/// * `cfg`: 本机媒体源与视频限宽配置
/// * `tap`: 视频帧回调, 接收器视频路经它上屏
///
/// returns: Option<MediaHandles> — 媒体句柄 (两路都不可用时是空句柄, 通话保持)
///
/// # Examples
///
/// ```
///
/// ```
pub fn start_outgoing(
    peers: &PeerMedias,
    rtp_ports: &RtpPorts,
    sockets: RecvSockets,
    cfg: &GuiSipConfig,
    tap: &VideoFrameSink,
) -> Option<MediaHandles> {
    // 收端编码: answer 收窄成什么就收什么, 不支持的那路不收
    let recv_audio = peers.audio.as_ref().and_then(|p| audio_pred(&p.codec));
    let recv_video = peers.video.as_ref().and_then(|p| {
        video_pred(&p.codec).or_else(|| {
            warn!(codec = %p.codec, "answer picked a video codec we cannot receive");
            None
        })
    });
    // 发端参数: answer 选中的 codec 必须在白名单, 否则跳过该路
    let send_audio = peers
        .audio
        .as_ref()
        .and_then(peer_audio_send_params)
        .or_else(|| {
            if let Some(p) = peers.audio.as_ref() {
                warn!(codec = %p.codec, "answer picked an audio codec we cannot send, skip audio");
            }
            None
        });
    let send_video = peers
        .video
        .as_ref()
        .and_then(|p| peer_video_send_params(p, cfg.max_width));
    Some(MediaHandles::new(
        *rtp_ports,
        cfg,
        tap.clone(),
        MediaPlan {
            recv: (recv_audio, recv_video),
            send_audio,
            send_video,
        },
        sockets,
    ))
}
