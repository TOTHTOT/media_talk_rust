//! SDP 编码协商: 按 offer 偏好序挑选本端支持的编码并构造 answer.
//! 从 call-ui (session.rs/media.rs) 和 examples/answer.rs 的三份逐字
//! 拷贝收口而来; 支持什么编码由调用方以谓词传入 (ipcam-sip 不反向
//! 依赖 ipcam-gst, 分层不破).

use std::net::IpAddr;

use ipcam_core::{AudioCodec, VideoCodec};
use sdp_rs::MediaDescription;
use sdp_rs::lines::Attribute;
use sdp_rs::lines::attribute::Rtpmap;
use sdp_rs::lines::media::{Media, MediaType, ProtoType};
use tracing::warn;

use crate::sdp::{
    OfferMedia, OfferMedias, audio_media_description, session_skeleton, video_media_description,
};

/// 协商结果: (选中的音频 pt+编码, 选中的视频 pt+编码+fmtp, answer 文本)
pub type NegotiatedAnswer = (
    Option<(u8, AudioCodec)>,
    Option<(u8, VideoCodec, Option<String>)>,
    String,
);

/// 编码协商 picks: 只含选中的编码, 用于 fingerprint 比对决定是否重启管线.
/// 不含 answer body — 调用方按需自行 build_answer.
pub type NegotiatePicks = (
    Option<(u8, AudioCodec)>,
    Option<(u8, VideoCodec, Option<String>)>,
);

/// 音频按对端 offer 的偏好序挑第一个谓词接受的编码; 全不匹配返回 None.
///
/// # Arguments
///
/// * `offer`: 解析后的对端 offer
/// * `pred`: 编码名 → 本端支持则 Some(AudioCodec), 不支持 None
///
/// returns: Option<(u8, AudioCodec)> — (offer 里的 pt, 编码)
///
/// # Examples
///
/// ```
///
/// ```
pub fn pick_audio(
    offer: &OfferMedias,
    pred: impl Fn(&str) -> Option<AudioCodec>,
) -> Option<(u8, AudioCodec)> {
    let a = offer.audio.as_ref()?;
    let picked = a
        .codecs
        .iter()
        .find_map(|c| pred(&c.codec).map(|ac| (c.payload_type, ac)));
    if picked.is_none() {
        warn!(codecs = ?a.codecs, "no supported audio codec in offer");
    }
    picked
}

/// 视频同上, 多带回选中编码的 fmtp (answer 要回显, 见 build_answer).
///
/// # Arguments
///
/// * `offer`: 解析后的对端 offer
/// * `pred`: 编码名 → 本端支持则 Some(VideoCodec), 不支持 None
///
/// returns: Option<(u8, VideoCodec, Option<String>)> — (pt, 编码, fmtp)
///
/// # Examples
///
/// ```
///
/// ```
pub fn pick_video(
    offer: &OfferMedias,
    pred: impl Fn(&str) -> Option<VideoCodec>,
) -> Option<(u8, VideoCodec, Option<String>)> {
    let v = offer.video.as_ref()?;
    let picked = v
        .codecs
        .iter()
        .find_map(|c| pred(&c.codec).map(|vc| (c.payload_type, vc, c.fmtp.clone())));
    if picked.is_none() {
        warn!(codecs = ?v.codecs, "no supported video codec in offer");
    }
    picked
}

/// pick_audio + pick_video 的组合, 用于 fingerprint 比对.
pub fn picks(
    offer: &OfferMedias,
    audio_pred: impl Fn(&str) -> Option<AudioCodec>,
    video_pred: impl Fn(&str) -> Option<VideoCodec>,
) -> NegotiatePicks {
    (pick_audio(offer, audio_pred), pick_video(offer, video_pred))
}

/// 从 offer 协商编码并构建 answer SDP (初始 accept 和 re-INVITE 共用).
/// 按 offer 裁剪 m-line: offer 没有的媒体路 answer 不带; offer 有但
/// 谓词全不匹配的路端口置 0 (RFC 3264 拒路, 明确告知拒收); 两路都
/// 选中时输出与 build_av_answer 逐字节一致 (互操作不变, 有测试钉死).
/// answer 的 pt/fmtp 回显 offer 里选中的编码 (RFC 3264).
///
/// # Arguments
///
/// * `offer`: 解析后的对端 offer
/// * `local_ip`: 本机 IP (answer 的 o=/c= 地址)
/// * `audio_port`: 本机音频 RTP 接收端口
/// * `video_port`: 本机视频 RTP 接收端口
/// * `audio_pred`: 音频编码谓词
/// * `video_pred`: 视频编码谓词
///
/// returns: NegotiatedAnswer — (音频选中, 视频选中, answer 文本)
///
/// # Examples
///
/// ```
///
/// ```
pub fn build_answer(
    offer: &OfferMedias,
    local_ip: IpAddr,
    audio_port: u16,
    video_port: u16,
    audio_pred: impl Fn(&str) -> Option<AudioCodec>,
    video_pred: impl Fn(&str) -> Option<VideoCodec>,
) -> NegotiatedAnswer {
    let (audio, video) = picks(offer, &audio_pred, &video_pred);
    // answer 的 sess_version 要大于 offer 的 ("1"), 同 build_audio_answer
    let mut sdp = session_skeleton(local_ip, "2");
    if let Some(oa) = &offer.audio {
        match audio {
            Some((pt, _)) => sdp
                .media_descriptions
                .push(audio_media_description(audio_port, &[pt])),
            None => sdp
                .media_descriptions
                .push(rejected_media(MediaType::Audio, oa)),
        }
    }
    if let Some(ov) = &offer.video {
        match &video {
            Some((pt, _, fmtp)) => sdp.media_descriptions.push(video_media_description(
                video_port,
                *pt,
                fmtp.as_deref(),
            )),
            None => sdp
                .media_descriptions
                .push(rejected_media(MediaType::Video, ov)),
        }
    }
    (audio, video, sdp.to_string())
}

/// RFC 3264 拒路: 端口置 0, fmt 回显 offer 第一个 pt (codecs 非空由
/// parse_fmt_codecs 保证). 不带 sendrecv — 拒收的路不声明方向
fn rejected_media(ty: MediaType, offered: &OfferMedia) -> MediaDescription {
    let first = &offered.codecs[0];
    MediaDescription {
        media: Media {
            media: ty,
            port: 0,
            num_of_ports: None,
            proto: ProtoType::RtpAvp,
            fmt: first.payload_type.to_string(),
        },
        info: None,
        connections: vec![],
        bandwidths: vec![],
        key: None,
        attributes: vec![Attribute::Rtpmap(Rtpmap {
            payload_type: first.payload_type as u32,
            encoding_name: first.codec.clone(),
            clock_rate: first.clock_rate,
            encoding_params: first.encoding_params,
        })],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdp::{build_av_answer, parse_offer_all};
    use std::net::Ipv4Addr;

    /// 全音视频 offer: 音频 [opus,PCMU,PCMA], 视频 [VP8,H264(fmtp)]
    const AV_OFFER: &str = concat!(
        "v=0\r\n",
        "o=- 1 1 IN IP4 192.168.1.50\r\n",
        "s=-\r\n",
        "c=IN IP4 192.168.1.50\r\n",
        "t=0 0\r\n",
        "m=audio 30000 RTP/AVP 111 0 8\r\n",
        "a=rtpmap:111 opus/48000/2\r\n",
        "a=rtpmap:0 PCMU/8000\r\n",
        "a=rtpmap:8 PCMA/8000\r\n",
        "m=video 30002 RTP/AVP 96 97\r\n",
        "a=rtpmap:96 VP8/90000\r\n",
        "a=rtpmap:97 H264/90000\r\n",
        "a=fmtp:97 profile-level-id=42801F\r\n",
    );
    /// 仅音频 offer (老门禁无摄像头场景)
    const AUDIO_ONLY_OFFER: &str = concat!(
        "v=0\r\n",
        "o=- 1 1 IN IP4 192.168.1.50\r\n",
        "s=-\r\n",
        "c=IN IP4 192.168.1.50\r\n",
        "t=0 0\r\n",
        "m=audio 30000 RTP/AVP 0\r\n",
        "a=rtpmap:0 PCMU/8000\r\n",
    );

    fn g711_pred(name: &str) -> Option<AudioCodec> {
        match name {
            "PCMU" => Some(AudioCodec::G711U),
            "PCMA" => Some(AudioCodec::G711A),
            _ => None,
        }
    }
    fn h26x_pred(name: &str) -> Option<VideoCodec> {
        match VideoCodec::from_name(name) {
            vc @ (VideoCodec::H264 | VideoCodec::H265) => Some(vc),
            _ => None,
        }
    }
    fn offer(s: &str) -> OfferMedias {
        parse_offer_all(s.as_bytes()).expect("offer must parse")
    }
    fn local() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
    }

    /// 按对端偏好序挑第一个谓词接受的: opus 打头跳过选 PCMU
    #[test]
    fn pick_audio_prefers_offer_order() {
        assert_eq!(
            pick_audio(&offer(AV_OFFER), g711_pred),
            Some((0, AudioCodec::G711U))
        );
    }

    #[test]
    fn pick_audio_none_when_all_unsupported() {
        assert_eq!(pick_audio(&offer(AV_OFFER), |_| None), None);
    }

    /// 视频跳过 VP8 选 H264, fmtp 随选中项回显
    #[test]
    fn pick_video_skips_vp8_and_echoes_fmtp() {
        let picked = pick_video(&offer(AV_OFFER), h26x_pred);
        assert_eq!(
            picked,
            Some((97, VideoCodec::H264, Some("profile-level-id=42801F".into())))
        );
    }

    /// 回归钉死: 两路都选中时与旧 build_av_answer 逐字节一致 (互操作不变)
    #[test]
    fn build_answer_full_av_matches_legacy() {
        let simple = offer(concat!(
            "v=0\r\n",
            "o=- 1 1 IN IP4 192.168.1.50\r\n",
            "s=-\r\n",
            "c=IN IP4 192.168.1.50\r\n",
            "t=0 0\r\n",
            "m=audio 30000 RTP/AVP 0\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
            "m=video 30002 RTP/AVP 96\r\n",
            "a=rtpmap:96 H264/90000\r\n",
            "a=fmtp:96 profile-level-id=42e01f\r\n",
        ));
        let (_, _, answer) = build_answer(&simple, local(), 40000, 40002, g711_pred, h26x_pred);
        let legacy = build_av_answer(
            local(),
            40000,
            40002,
            0,
            96,
            Some("profile-level-id=42e01f"),
        );
        assert_eq!(answer, legacy.to_string());
    }

    /// offer 没有视频路 → answer 不带 m=video (旧行为会无中生有)
    #[test]
    fn build_answer_audio_only_offer_omits_video() {
        let (_, _, answer) = build_answer(
            &offer(AUDIO_ONLY_OFFER),
            local(),
            40000,
            40002,
            g711_pred,
            h26x_pred,
        );
        assert!(!answer.contains("m=video"), "answer:\n{answer}");
        assert!(
            answer.contains("m=audio 40000 RTP/AVP 0"),
            "answer:\n{answer}"
        );
    }

    /// offer 有音频路但全不支持 → 该路端口置 0 (RFC 3264 拒路)
    #[test]
    fn build_answer_rejects_unsupported_audio_with_port_zero() {
        let o = offer(concat!(
            "v=0\r\n",
            "o=- 1 1 IN IP4 192.168.1.50\r\n",
            "s=-\r\n",
            "c=IN IP4 192.168.1.50\r\n",
            "t=0 0\r\n",
            "m=audio 30000 RTP/AVP 111\r\n",
            "a=rtpmap:111 opus/48000/2\r\n",
        ));
        let (audio, _, answer) = build_answer(&o, local(), 40000, 40002, g711_pred, h26x_pred);
        assert_eq!(audio, None);
        assert!(
            answer.contains("m=audio 0 RTP/AVP 111"),
            "answer:\n{answer}"
        );
    }

    /// 视频全不支持同样置 0; 音频正常选中
    #[test]
    fn build_answer_rejects_unsupported_video_with_port_zero() {
        let (audio, video, answer) =
            build_answer(&offer(AV_OFFER), local(), 40000, 40002, g711_pred, |_| None);
        assert_eq!(audio, Some((0, AudioCodec::G711U)));
        assert_eq!(video, None);
        assert!(answer.contains("m=video 0 RTP/AVP 96"), "answer:\n{answer}");
    }
}
