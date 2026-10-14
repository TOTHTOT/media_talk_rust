//! 媒体面: 按 SDP 协商结果起停 RTP 收发管线, 改写自 ipcam-sip 的
//! examples/answer.rs (CallMedia 指纹重启) 和 examples/call.rs
//! (PeerMedias → AudioDest/RtpDest 装配).
//!
//! 收端: 音频解码进 autoaudiosink (playback), 视频解码后经 video_tap
//! 吐 RGBA 帧给 slint 纹理 (取代 autovideosink). 发端: 按 GuiSipConfig
//! 的 audio_src/video_src 采集回传, 视频发送链只有 H264 (x264enc),
//! 对端协商成其他编码时该路只收不发. 单路失败只 warn 不抛错,
//! 通话不因一路媒体挂掉 (同 answer.rs 原则).

use std::net::SocketAddr;

use ipcam_core::{AudioCodec, VideoCodec};
use ipcam_gst::{
    AudioDest, RtpDest, RtpReceiver, RtpRecvConfig, RtpSendConfig, RtpSender, TrackSource,
    VideoFrameSink, start_rtp_receiver, start_rtp_sender,
};
use ipcam_sip::negotiate;
use ipcam_sip::sdp::{OfferMedias, PeerMedias};
use tracing::{info, warn};

use crate::session::{AUDIO_RTP_PORT, GuiSipConfig, VIDEO_RTP_PORT};

/// 视频发送指纹: (对端收视频地址, pt, 编码). 编码参与比对 — 从 H264
/// 切成 H265 时发送链建不出来, 指纹变了才触发重启逻辑重新判定
type VideoDest = (SocketAddr, u8, VideoCodec);

/// 一路通话的媒体句柄: 接收器 + 发送器 + 协商指纹. re-INVITE 时按指纹
/// 比对 (recv 指纹 = 编码对; send 指纹 = 地址/pt/编码), 变了才重启
/// 对应管线, 没变只回 answer 不动管线 (同 answer.rs CallMedia)
pub struct MediaHandles {
    receiver: Option<RtpReceiver>,
    sender: Option<RtpSender>,
    /// 接收指纹: (音频编码, 视频编码). pt 不参与 — udpsrc 不按 pt
    /// 过滤, 端口是我们在 answer 里声明的固定值, 不会变
    recv_fingerprint: (Option<AudioCodec>, Option<VideoCodec>),
    /// 音频发送指纹: 对端收音频地址/pt/编码
    send_audio_fingerprint: Option<AudioDest>,
    /// 视频发送指纹: 对端收视频地址/pt/编码
    send_video_fingerprint: Option<VideoDest>,
    /// 本机接收端口 (音频, 视频): 被叫是固定常量, 主叫是 offer 里
    /// 声明的端口, 接收器重启 (re-INVITE 改编码) 时用
    recv_ports: (u16, u16),
    /// 本机回传源配置, renegotiate 重启发送器时用
    audio_src: TrackSource,
    video_src: TrackSource,
    max_width: u32,
    /// 视频帧 tap: VideoFrameSink 是 Arc, 接收器重启时 clone 复用
    tap: VideoFrameSink,
}

impl MediaHandles {
    /// 按指纹起接收器和发送器. 单路不可用时该路 None (只收不发 /
    /// 只发不收), 不抛错.
    fn new(
        recv_ports: (u16, u16),
        cfg: &GuiSipConfig,
        tap: VideoFrameSink,
        recv_fingerprint: (Option<AudioCodec>, Option<VideoCodec>),
        send_audio_fingerprint: Option<AudioDest>,
        send_video_fingerprint: Option<VideoDest>,
    ) -> Self {
        let receiver = start_receiver(
            tap.clone(),
            recv_ports,
            recv_fingerprint.0,
            recv_fingerprint.1,
        );
        let sender = start_sender(
            &cfg.audio_src,
            &cfg.video_src,
            cfg.max_width,
            send_audio_fingerprint,
            send_video_fingerprint,
        );
        Self {
            receiver,
            sender,
            recv_fingerprint,
            send_audio_fingerprint,
            send_video_fingerprint,
            recv_ports,
            audio_src: cfg.audio_src.clone(),
            video_src: cfg.video_src.clone(),
            max_width: cfg.max_width,
            tap,
        }
    }

    /// re-INVITE 重新协商 (照 answer.rs CallMedia::renegotiate): 编码
    /// 变了重启接收器 (tap 复用); 对端收包地址/pt/编码变了重启发送器
    pub fn renegotiate(&mut self, offer: &OfferMedias) {
        let new_recv = (
            negotiate::pick_audio(offer, audio_pred).map(|(_, c)| c),
            negotiate::pick_video(offer, video_pred).map(|(_, c, _)| c),
        );
        if new_recv != self.recv_fingerprint {
            info!(old = ?self.recv_fingerprint, new = ?new_recv, "codec changed, restarting receiver");
            if let Some(r) = self.receiver.take() {
                r.stop();
            }
            self.receiver =
                start_receiver(self.tap.clone(), self.recv_ports, new_recv.0, new_recv.1);
            self.recv_fingerprint = new_recv;
        }
        let new_audio = audio_send_params(offer);
        let new_video = video_send_params(offer);
        if new_audio != self.send_audio_fingerprint || new_video != self.send_video_fingerprint {
            info!(
                audio_changed = new_audio != self.send_audio_fingerprint,
                video_changed = new_video != self.send_video_fingerprint,
                "send dest changed, restarting sender"
            );
            if let Some(s) = self.sender.take() {
                s.stop();
            }
            self.sender = start_sender(
                &self.audio_src,
                &self.video_src,
                self.max_width,
                new_audio,
                new_video,
            );
            self.send_audio_fingerprint = new_audio;
            self.send_video_fingerprint = new_video;
        }
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

/// 可收发音频谓词 (复用 ipcam-gst 发送白名单 G.711): session 协商与
/// 本模块指纹比对共用这一个, 防漂移
pub(crate) fn audio_pred(name: &str) -> Option<AudioCodec> {
    ipcam_gst::sendable_audio_codec(name)
}

/// 可收视频谓词: H264/H265
pub(crate) fn video_pred(name: &str) -> Option<VideoCodec> {
    match VideoCodec::from_name(name) {
        vc @ (VideoCodec::H264 | VideoCodec::H265) => Some(vc),
        _ => None,
    }
}

/// 从 offer 提取音频回传参数 (对端收音频地址, pt, 编码), 同时充当
/// 发送指纹: re-INVITE 之后任一字段变了就重启发送器 (地址变了 udpsink
/// 找不到对端, pt/编码变了 RTP 头和编码链对不上)
fn audio_send_params(offer: &OfferMedias) -> Option<AudioDest> {
    let a = offer.audio.as_ref()?;
    negotiate::pick_audio(offer, audio_pred).map(|(pt, codec)| AudioDest {
        addr: SocketAddr::new(a.addr, a.port),
        payload_type: pt,
        codec,
    })
}

/// 从 offer 提取视频回传参数 (对端收视频地址, pt, 编码), 同时充当
/// 发送指纹. 编码取 pick_video 选中的那个 (H264/H265)
fn video_send_params(offer: &OfferMedias) -> Option<VideoDest> {
    let v = offer.video.as_ref()?;
    negotiate::pick_video(offer, video_pred)
        .map(|(pt, codec, _)| (SocketAddr::new(v.addr, v.port), pt, codec))
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
    ports: (u16, u16),
    audio: Option<AudioCodec>,
    video: Option<VideoCodec>,
) -> Option<RtpReceiver> {
    if audio.is_none() && video.is_none() {
        // RtpRecvConfig 要求至少一路媒体, 无媒体可收就不起管线
        return None;
    }
    match start_rtp_receiver(RtpRecvConfig {
        path: None,
        playback: true,
        video_tap: Some(tap),
        video: video.map(|c| (ports.1, c)),
        audio: audio.map(|c| (ports.0, c)),
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
/// * `max_width`: 视频限宽 (对端解码能力); 0 = 不限
/// * `audio`: 音频发送目标 (地址/pt/编码), None 表示不发音频
/// * `video`: 视频发送目标 (地址/pt/编码), None 表示不发视频
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
    max_width: u32,
    audio: Option<AudioDest>,
    video: Option<VideoDest>,
) -> Option<RtpSender> {
    let video = video.and_then(|(addr, payload_type, codec)| match codec {
        VideoCodec::H264 => Some((
            video_src.clone(),
            RtpDest {
                addr,
                payload_type,
                max_width: (max_width > 0).then_some(max_width),
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
    match start_rtp_sender(RtpSendConfig {
        audio: audio.map(|d| (audio_src.clone(), d)),
        video,
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
    cfg: &GuiSipConfig,
    tap: &VideoFrameSink,
) -> Option<MediaHandles> {
    let recv_fingerprint = (
        negotiate::pick_audio(offer, audio_pred).map(|(_, c)| c),
        negotiate::pick_video(offer, video_pred).map(|(_, c, _)| c),
    );
    Some(MediaHandles::new(
        (AUDIO_RTP_PORT, VIDEO_RTP_PORT),
        cfg,
        tap.clone(),
        recv_fingerprint,
        audio_send_params(offer),
        video_send_params(offer),
    ))
}

/// 主叫侧起媒体: peers 是对端 answer 解析结果, rtp_ports 是 offer 里
/// 声明的本机接收端口 (占位 socket 已由调用方释放). 发送 pt/编码必须
/// 用 answer 里的值 — answer 收窄成什么就发什么 (RFC 3264, 同 call.rs)
///
/// # Arguments
///
/// * `peers`: 对端 answer 解析出的音视频协商结果
/// * `rtp_ports`: 本机接收端口 (音频, 视频), 与 offer 里声明的一致
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
    rtp_ports: (u16, u16),
    cfg: &GuiSipConfig,
    tap: &VideoFrameSink,
) -> Option<MediaHandles> {
    // 收端编码: answer 收窄成什么就收什么, 不支持的那路不收
    let recv_audio = peers
        .audio
        .as_ref()
        .and_then(|p| ipcam_gst::sendable_audio_codec(&p.codec));
    let recv_video = peers
        .video
        .as_ref()
        .and_then(|p| match VideoCodec::from_name(&p.codec) {
            c @ (VideoCodec::H264 | VideoCodec::H265) => Some(c),
            other => {
                warn!(codec = ?other, "answer picked a video codec we cannot receive");
                None
            }
        });
    let send_audio = peers.audio.as_ref().and_then(|p| {
        ipcam_gst::sendable_audio_codec(&p.codec)
            .map(|codec| AudioDest {
                addr: p.addr,
                payload_type: p.payload_type,
                codec,
            })
            .or_else(|| {
                warn!(codec = %p.codec, "answer picked an audio codec we cannot send, skip audio");
                None
            })
    });
    let send_video = peers
        .video
        .as_ref()
        .map(|p| (p.addr, p.payload_type, VideoCodec::from_name(&p.codec)));
    Some(MediaHandles::new(
        rtp_ports,
        cfg,
        tap.clone(),
        (recv_audio, recv_video),
        send_audio,
        send_video,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipcam_sip::sdp::{OfferCodec, OfferMedia};
    use std::net::{IpAddr, Ipv4Addr};

    /// 构造一路 offer 媒体: 按给定 (pt, 编码名) 列表填 codecs
    fn offer_media(codecs: &[(u8, &str)]) -> OfferMedia {
        OfferMedia {
            addr: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)),
            port: 5000,
            codecs: codecs
                .iter()
                .map(|(pt, name)| OfferCodec {
                    payload_type: *pt,
                    codec: (*name).to_string(),
                    clock_rate: 8000,
                    encoding_params: None,
                    fmtp: None,
                })
                .collect(),
        }
    }

    /// 音频按对端偏好序挑第一个我们支持的: opus 打头也要跳过选 PCMU
    #[test]
    fn pick_audio_skips_unsupported_head() {
        let offer = OfferMedias {
            audio: Some(offer_media(&[(111, "opus"), (0, "PCMU"), (8, "PCMA")])),
            video: None,
        };
        assert_eq!(
            negotiate::pick_audio(&offer, audio_pred),
            Some((0, AudioCodec::G711U))
        );
    }

    /// offer 里全是不支持的音频编码时返回 None
    #[test]
    fn pick_audio_none_when_all_unsupported() {
        let offer = OfferMedias {
            audio: Some(offer_media(&[(111, "opus")])),
            video: None,
        };
        assert_eq!(negotiate::pick_audio(&offer, audio_pred), None);
    }

    /// 视频跳过 VP8 选 H264; 全不支持时整路拒收
    #[test]
    fn pick_video_h264_only() {
        let offer = OfferMedias {
            audio: None,
            video: Some(offer_media(&[(96, "VP8"), (97, "H264")])),
        };
        let picked = negotiate::pick_video(&offer, video_pred);
        assert!(matches!(picked, Some((97, VideoCodec::H264, None))));

        let offer = OfferMedias {
            audio: None,
            video: Some(offer_media(&[(96, "VP8")])),
        };
        assert_eq!(negotiate::pick_video(&offer, video_pred), None);
    }

    /// 音频发送指纹: 地址取 offer 媒体的 addr/port, pt/编码取选中项
    #[test]
    fn audio_send_params_from_offer() {
        let offer = OfferMedias {
            audio: Some(offer_media(&[(8, "PCMA")])),
            video: None,
        };
        let dest = audio_send_params(&offer);
        assert_eq!(
            dest,
            Some(AudioDest {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 5000),
                payload_type: 8,
                codec: AudioCodec::G711A,
            })
        );
    }

    /// 视频发送指纹: 地址/pt/编码三要素齐全; 无视频路时 None
    #[test]
    fn video_send_params_from_offer() {
        let offer = OfferMedias {
            audio: None,
            video: Some(offer_media(&[(97, "H264")])),
        };
        assert_eq!(
            video_send_params(&offer),
            Some((
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 5000),
                97,
                VideoCodec::H264
            ))
        );

        let offer = OfferMedias::default();
        assert_eq!(video_send_params(&offer), None);
    }
}
