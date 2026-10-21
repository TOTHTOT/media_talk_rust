//! 协商计划: 从 offer/answer 产出收端指纹与两路发送目标 (MediaPlan),
//! 以及 session 协商与本模块指纹比对共用的 codec 谓词 (防漂移).
//!
//! re-INVITE 重协商时按 MediaPlan 逐字段比对, 变了才重启对应管线
//! (比对逻辑在 mod.rs MediaHandles::renegotiate).

use std::net::SocketAddr;

use ipcam_core::{AudioCodec, VideoCodec};
use ipcam_gst::AudioDest;
use ipcam_sip::negotiate;
use ipcam_sip::sdp::{OfferMedias, PeerMedia};

/// 视频发送目标: 对端收视频地址, pt, 编码, 限宽.
/// 限宽 = min(本端配置, 对端 fmtp 声明的解码上限), 只收紧不放宽.
/// 参与 fingerprint 比对: 编码/限宽变了才重启发送链.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoDest {
    pub addr: SocketAddr,
    pub payload_type: u8,
    pub codec: VideoCodec,
    pub max_width: Option<u32>,
}

/// 一路协商的全部产物: 收端指纹 (音频/视频编码) + 两路发送目标.
/// 从 offer 协商时 picks 只跑一次 (start_incoming / renegotiate 共用),
/// 主叫侧从 answer 直接收窄构造 (见 start_outgoing)
#[derive(Debug, Clone, PartialEq)]
pub struct MediaPlan {
    /// 接收指纹: (音频编码, 视频编码). pt 不参与 — udpsrc 不按 pt
    /// 过滤, 端口是我们在 answer 里声明的固定值, 不会变
    pub recv: (Option<AudioCodec>, Option<VideoCodec>),
    /// 音频发送目标/指纹: 对端收音频地址/pt/编码
    pub send_audio: Option<AudioDest>,
    /// 视频发送目标/指纹: 对端收视频地址/pt/编码/限宽
    pub send_video: Option<VideoDest>,
}

/// 从 offer 跑一次 picks, 同时产出收端指纹和两路发送目标.
/// 发送目标地址/pt 取自 offer 对应媒体行与选中编码; 视频限宽取
/// min(cfg_max_width, 对端选中编码 fmtp 声明的解码上限) — 只收紧不放宽
///
/// # Arguments
///
/// * `offer`: 解析后的对端 SDP (音视频媒体列表 + 编码候选)
/// * `cfg_max_width`: 本机视频发送限宽配置, 0 表示不限
///
/// returns: MediaPlan — 收端指纹 + 两路发送目标 (不可发的路为 None)
///
/// # Examples
///
/// ```
///
/// ```
pub(crate) fn plan_from_offer(offer: &OfferMedias, cfg_max_width: u32) -> MediaPlan {
    let (audio, video) = negotiate::picks(offer, audio_pred, video_pred);
    let recv = (audio.map(|(_, c)| c), video.as_ref().map(|(_, c, _)| *c));
    let send_audio = audio.and_then(|(pt, codec)| {
        offer.audio.as_ref().map(|a| AudioDest {
            addr: SocketAddr::new(a.addr, a.port),
            payload_type: pt,
            codec,
        })
    });
    let send_video = video.and_then(|(pt, codec, fmtp)| {
        offer.video.as_ref().map(|v| VideoDest {
            addr: SocketAddr::new(v.addr, v.port),
            payload_type: pt,
            codec,
            max_width: ipcam_sip::sdp::clamp_max_width(
                (cfg_max_width > 0).then_some(cfg_max_width),
                fmtp.as_deref(),
            ),
        })
    });
    MediaPlan {
        recv,
        send_audio,
        send_video,
    }
}

/// 可收发音频谓词 (复用 ipcam-gst 发送白名单 G.711): session 协商与
/// 媒体层指纹比对共用这一个, 防漂移
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

/// 从 answer 的已选中音频 codec 提取发送参数.
/// 只在 answer 的 codec 属于本机可发送白名单时返回 Some, 否则 None.
pub(crate) fn peer_audio_send_params(peer: &PeerMedia) -> Option<AudioDest> {
    ipcam_gst::sendable_audio_codec(&peer.codec).map(|codec| AudioDest {
        addr: peer.addr,
        payload_type: peer.payload_type,
        codec,
    })
}

/// 从 answer 的已选中视频 codec 提取发送参数.
/// answer 只选了 codec, 地址/pt 取自 PeerMedia; 限宽取
/// min(cfg_max_width, 对端 fmtp 声明的解码上限) — 只收紧不放宽
///
/// # Arguments
///
/// * `peer`: 对端 answer 解析出的单路媒体 (地址/pt/编码/fmtp)
/// * `cfg_max_width`: 本机视频发送限宽配置, 0 表示不限
///
/// returns: Option<VideoDest> — 发送目标, 编码不认识时 None (该路不发视频)
///
/// # Examples
///
/// ```
///
/// ```
pub(crate) fn peer_video_send_params(peer: &PeerMedia, cfg_max_width: u32) -> Option<VideoDest> {
    let codec = VideoCodec::from_name(&peer.codec);
    match codec {
        VideoCodec::Unknown => None,
        _ => Some(VideoDest {
            addr: peer.addr,
            payload_type: peer.payload_type,
            codec,
            max_width: ipcam_sip::sdp::clamp_max_width(
                (cfg_max_width > 0).then_some(cfg_max_width),
                peer.fmtp.as_deref(),
            ),
        }),
    }
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

    /// 音频发送目标: 地址取 offer 媒体的 addr/port, pt/编码取选中项
    #[test]
    fn plan_send_audio_from_offer() {
        let offer = OfferMedias {
            audio: Some(offer_media(&[(8, "PCMA")])),
            video: None,
        };
        let plan = plan_from_offer(&offer, 640);
        assert_eq!(plan.recv, (Some(AudioCodec::G711A), None));
        assert_eq!(
            plan.send_audio,
            Some(AudioDest {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 5000),
                payload_type: 8,
                codec: AudioCodec::G711A,
            })
        );
        assert_eq!(plan.send_video, None);
    }

    /// 视频发送目标: 地址/pt/编码三要素 + 限宽 (min(配置, 对端 fmtp 声明));
    /// 无视频路时 None
    #[test]
    fn plan_send_video_from_offer() {
        let offer = OfferMedias {
            audio: None,
            video: Some(offer_media(&[(97, "H264")])),
        };
        // 对端没写 fmtp → 限宽用配置值
        let plan = plan_from_offer(&offer, 640);
        assert_eq!(plan.recv, (None, Some(VideoCodec::H264)));
        assert_eq!(
            plan.send_video,
            Some(VideoDest {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 5000),
                payload_type: 97,
                codec: VideoCodec::H264,
                max_width: Some(640),
            })
        );

        let plan = plan_from_offer(&OfferMedias::default(), 640);
        assert_eq!(plan.recv, (None, None));
        assert_eq!(plan.send_video, None);
    }

    /// 对端 fmtp 声明低 level (L2.2 → D1 720 宽) 时, 限宽收紧到对端能力;
    /// 对端声明高 level (L3.1 → 720p) 时保持配置值, 不放宽
    #[test]
    fn plan_send_video_clamps_to_peer_capability() {
        let mut media = offer_media(&[(96, "H264")]);
        media.codecs[0].fmtp = Some("profile-level-id=42c016".into());
        let offer = OfferMedias {
            audio: None,
            video: Some(media),
        };
        let max_width = plan_from_offer(&offer, 1280)
            .send_video
            .expect("video picked")
            .max_width;
        assert_eq!(max_width, Some(720), "peer D1 capability must tighten");

        let mut media = offer_media(&[(96, "H264")]);
        media.codecs[0].fmtp = Some("profile-level-id=42e01f".into());
        let offer = OfferMedias {
            audio: None,
            video: Some(media),
        };
        let max_width = plan_from_offer(&offer, 640)
            .send_video
            .expect("video picked")
            .max_width;
        assert_eq!(max_width, Some(640), "peer 720p must not widen config");
    }
}
