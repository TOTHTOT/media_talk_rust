//! SIP + ICE 会话准备: 音频/可选视频 agent 收集/喂入/连通等待与 SDP 组装.
//!
//! SIP 路径的 ICE 使用标准媒体 SDP (`m=audio`/`m=video` 内嵌
//! `a=ice-ufrag`/`a=ice-pwd`/`a=candidate`), 对端不回 ICE 时由调用方
//! 回退普通 RTP. 音频必须协商成 Opus; 视频可选. payload type 按对端
//! offer/answer 动态取 (Linphone 默认 Opus 96, 本端 offer 用 111).

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use anyhow::{Context as _, Result};
use ipcam_core::{AudioCodec, VideoCodec};
use ipcam_gst::VideoFrameSink;
use ipcam_ice::IceAgent;
use ipcam_ice::blob::IceBlob;
use ipcam_ice::sdp::{self, RemoteMedia};
use ipcam_sip::negotiate;
use ipcam_sip::sdp::{OfferMedias, PT_H264, PT_OPUS, PeerMedias};
use tracing::info;

use super::types::{GuiSipConfig, RtpPorts};
use crate::media::{IceCallMedia, IceCodecs};

/// 建立阶段候选收集上限: 超时也接受已收集到的 host 候选, 不让 STUN
/// 不可达阻塞呼叫建立.
const GATHER_TIMEOUT: Duration = Duration::from_secs(3);
/// ICE 连通性检查上限.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 本端 agent 与 gather 结果. Drop 时关闭 agent (媒体句柄必须先停).
/// 音频 agent 必选; 对端 offer/answer 没有可用 H264 时视频 agent 为 None.
pub struct LocalIce {
    video_agent: Option<IceAgent>,
    audio_agent: Option<IceAgent>,
    pub video_blob: Option<IceBlob>,
    pub audio_blob: IceBlob,
    pub video_port: u16,
    pub audio_port: u16,
    codecs: IceCodecs,
}

impl LocalIce {
    /// 写进 SIP SDP m= 行的接收端口 (也是 ICE 失败回退普通 RTP 的端口).
    /// 无视频 agent 时 video 为 0.
    pub fn ports(&self) -> RtpPorts {
        RtpPorts {
            audio: self.audio_port,
            video: self.video_port,
        }
    }

    /// 把 ICE 属性注入普通 offer SDP.
    pub fn offer_sdp(&self, plain: &str) -> Result<String> {
        let video = self.codecs.video_pt.and(self.video_blob.as_ref());
        Ok(sdp::append_ice(plain, video, Some(&self.audio_blob))?)
    }

    /// 按对端 offer 构造带 ICE 的 answer. 音频必须协商成 Opus; 视频可选,
    /// 只有对端选了 H264 且本端建了视频 agent 时才带 ICE.
    pub fn answer_sdp(
        &mut self,
        offer: &OfferMedias,
        local_ip: Ipv4Addr,
    ) -> Result<Option<String>> {
        let (audio, video, answer) = negotiate::build_answer(
            offer,
            IpAddr::V4(local_ip),
            self.audio_port,
            self.video_port,
            ice_audio_pred,
            ice_video_pred,
        );
        let Some((audio_pt, AudioCodec::Opus)) = audio else {
            return Ok(None);
        };
        let video_pt = match (&video, self.video_blob.as_ref()) {
            (Some((pt, VideoCodec::H264, _)), Some(_)) => Some(*pt),
            _ => None,
        };
        self.codecs = IceCodecs { audio_pt, video_pt };
        let video_blob = video_pt.and(self.video_blob.as_ref());
        Ok(Some(sdp::append_ice(
            &answer,
            video_blob,
            Some(&self.audio_blob),
        )?))
    }

    /// 喂入对端标准媒体 SDP 中的 ICE 参数. 音频必须有; 视频只有在本端
    /// 启用了视频 agent 时才喂.
    pub async fn add_remote(&self, remote_text: &str) -> Result<()> {
        let remote = sdp::parse_offer(remote_text).context("parse remote ICE SDP")?;
        self.add_remote_media(&remote).await
    }

    async fn add_remote_media(&self, remote: &RemoteMedia) -> Result<()> {
        let audio = remote
            .audio
            .as_ref()
            .context("remote SDP has no audio ICE")?;
        self.audio_agent().add_remote(audio).await?;
        if self.codecs.video_pt.is_some()
            && let (Some(video), Some(agent)) = (remote.video.as_ref(), self.video_agent.as_ref())
        {
            agent.add_remote(video).await?;
        }
        Ok(())
    }

    /// 等已启用的媒体路 Connected (测试用; 生产路径由媒体管线先启动再等).
    #[cfg(test)]
    pub async fn wait_connected(&self) -> Result<(), ipcam_ice::IceError> {
        let audio = self.audio_agent().wait_connected(CONNECT_TIMEOUT);
        if let Some(video) = self.video_agent.as_ref() {
            let (audio, video) = tokio::join!(audio, video.wait_connected(CONNECT_TIMEOUT),);
            audio?;
            video?;
        } else {
            audio.await?;
        }
        info!("SIP ICE connected");
        Ok(())
    }

    /// 覆盖协商出的 payload type (主叫在 answer 到达后调用).
    pub fn set_codecs(&mut self, mut codecs: IceCodecs) {
        if self.video_agent.is_none() {
            codecs.video_pt = None;
        }
        self.codecs = codecs;
    }

    /// 起 nicesink/nicesrc 媒体管线 (agent 所有权移入媒体句柄).
    /// 未协商视频时视频 agent 在这里关闭, 不进媒体管线.
    pub fn into_media(mut self, cfg: &GuiSipConfig, tap: &VideoFrameSink) -> Result<IceCallMedia> {
        let audio = self
            .audio_agent
            .take()
            .context("audio ICE agent already taken")?;
        let video = if self.codecs.video_pt.is_some() {
            self.video_agent.take()
        } else {
            if let Some(video) = self.video_agent.take() {
                video.close();
            }
            None
        };
        Ok(IceCallMedia::start(video, audio, self.codecs, cfg, tap)?)
    }

    #[cfg(test)]
    fn video_agent(&self) -> &IceAgent {
        self.video_agent.as_ref().expect("video ICE agent alive")
    }

    fn audio_agent(&self) -> &IceAgent {
        self.audio_agent.as_ref().expect("audio ICE agent alive")
    }
}

impl Drop for LocalIce {
    fn drop(&mut self) {
        if let Some(video) = self.video_agent.take() {
            video.close();
        }
        if let Some(audio) = self.audio_agent.take() {
            audio.close();
        }
    }
}

/// 建音频 agent, 需要视频时再建视频 agent, 并收集本地候选.
///
/// 音频 agent 必选; 视频 agent 可选 (Linphone Desktop 默认 audio-only).
/// 建失败或 gather 失败时, 已建 agent 随早期返回自动 Drop 关闭.
pub async fn prepare(
    local_ip: Ipv4Addr,
    stun: Option<String>,
    controlling: bool,
    want_video: bool,
) -> Result<LocalIce> {
    let audio_agent = IceAgent::new_rtp(stun.clone(), controlling)?;
    let video_agent = if want_video {
        Some(IceAgent::new_rtp(stun, controlling)?)
    } else {
        None
    };
    let (audio_blob, video_blob) = if let Some(video_agent) = video_agent.as_ref() {
        let (audio, video) = tokio::try_join!(
            audio_agent.local_blob_within(GATHER_TIMEOUT),
            video_agent.local_blob_within(GATHER_TIMEOUT),
        )?;
        (audio, Some(video))
    } else {
        (audio_agent.local_blob_within(GATHER_TIMEOUT).await?, None)
    };
    let endpoint = |blob: &IceBlob, what: &str| -> Result<u16> {
        sdp::media_endpoint(blob, IpAddr::V4(local_ip))
            .map(|addr| addr.port())
            .with_context(|| format!("ICE has no {what} UDP host candidate on {local_ip}"))
    };
    let audio_port = endpoint(&audio_blob, "audio")?;
    let video_port = match video_blob.as_ref() {
        Some(blob) => endpoint(blob, "video")?,
        None => 0,
    };
    info!(video_port, audio_port, "SIP ICE local candidates gathered");
    Ok(LocalIce {
        video_agent,
        audio_agent: Some(audio_agent),
        video_blob,
        audio_blob,
        video_port,
        audio_port,
        codecs: IceCodecs {
            audio_pt: PT_OPUS,
            video_pt: want_video.then_some(PT_H264),
        },
    })
}

/// 对端 answer 是否协商出可用的 Opus (视频可选).
pub fn answer_supports_ice(peers: &PeerMedias) -> Option<IceCodecs> {
    let audio = peers.audio.as_ref()?;
    if audio.addr.port() == 0 || !audio.codec.eq_ignore_ascii_case("opus") {
        return None;
    }
    let video_pt = peers.video.as_ref().and_then(|p| {
        (p.addr.port() != 0 && p.codec.eq_ignore_ascii_case("h264")).then_some(p.payload_type)
    });
    Some(IceCodecs {
        audio_pt: audio.payload_type,
        video_pt,
    })
}

/// 对端 offer 是否含 ICE 媒体链可用的 Opus (视频可选).
///
/// 动态段 PT 不能写死: Linphone 默认 Opus 是 96, 本端 offer 用 111.
pub fn offer_supports_ice(offer: &OfferMedias) -> Option<IceCodecs> {
    let audio_pt = offer.audio.as_ref().and_then(|m| {
        if m.port == 0 {
            return None;
        }
        m.codecs
            .iter()
            .find(|c| c.codec.eq_ignore_ascii_case("opus") && c.clock_rate == 48_000)
            .map(|c| c.payload_type)
    })?;
    let video_pt = offer.video.as_ref().and_then(|m| {
        if m.port == 0 {
            return None;
        }
        m.codecs
            .iter()
            .find(|c| c.codec.eq_ignore_ascii_case("h264"))
            .map(|c| c.payload_type)
    });
    Some(IceCodecs { audio_pt, video_pt })
}

/// ICE 音频协商谓词: 只接受 Opus.
pub fn ice_audio_pred(name: &str) -> Option<AudioCodec> {
    name.eq_ignore_ascii_case("opus")
        .then_some(AudioCodec::Opus)
}

/// ICE 视频协商谓词: 只接受 H264.
fn ice_video_pred(name: &str) -> Option<VideoCodec> {
    name.eq_ignore_ascii_case("h264")
        .then_some(VideoCodec::H264)
}

/// 把 offer 里影响传输的属性镜像到 answer.
///
/// 当前 agent 只有 component 1; 如果 offer 声明了 `a=rtcp-mux` 而
/// answer 不回, Linphone 会为 RTCP 建 component 2, 连通检查可能永远
/// 等不到该 component, 表现为 ICE 不 Connected 或没有媒体. 单 m-line
/// 也回 `a=mid`, 避免部分实现对 BUNDLE offer 的 answer 校验过严.
pub fn mirror_offer_transport_attrs(answer: &str, offer_text: &str) -> String {
    // 按媒体段取 offer 的 mid: audio 的 mid 要进 BUNDLE group,
    // 被拒的 video 段也要回自己的 mid (RFC 8843).
    let mut sections: Vec<(String, Option<String>)> = Vec::new();
    let mut current: Option<(String, Option<String>)> = None;
    for raw in offer_text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            let media = rest
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            current = Some((media, None));
        } else if let Some(mid) = line.strip_prefix("a=mid:")
            && let Some((_, current_mid)) = current.as_mut()
        {
            *current_mid = Some(mid.trim().to_string());
        }
    }
    if let Some(section) = current {
        sections.push(section);
    }

    let audio_mid = sections
        .iter()
        .find(|(media, _)| media == "audio")
        .and_then(|(_, mid)| mid.clone());
    let video_mid = sections
        .iter()
        .find(|(media, _)| media == "video")
        .and_then(|(_, mid)| mid.clone());
    let rtcp_mux = offer_text
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("a=rtcp-mux"));
    if audio_mid.is_none() && video_mid.is_none() && !rtcp_mux {
        return answer.to_string();
    }

    // 本端 audio/video 各用一个独立 ICE agent + 独立 5-tuple, 无法把
    // video 塞进 audio 的 BUNDLE transport; 因此 answer 明确不写
    // `a=group:BUNDLE`, 让 Linphone 对两条 m-line 各走各的 ICE.
    // 只保留 a=mid/a=rtcp-mux, 避免部分实现把 video 误绑到 audio 上.
    let mut out = String::with_capacity(answer.len() + 64);
    for raw in answer.lines() {
        let line = raw.trim_end_matches('\r');
        out.push_str(line);
        out.push_str("\r\n");
        if line.starts_with("m=audio") {
            if let Some(mid) = audio_mid.as_deref() {
                out.push_str(&format!("a=mid:{mid}\r\n"));
            }
            if rtcp_mux {
                out.push_str("a=rtcp-mux\r\n");
            }
        } else if line.starts_with("m=video") {
            if let Some(mid) = video_mid.as_deref() {
                out.push_str(&format!("a=mid:{mid}\r\n"));
            }
            if rtcp_mux {
                out.push_str("a=rtcp-mux\r\n");
            }
        }
    }
    out
}

/// 喂入对端 ICE -> 起媒体 -> 等连通. 先起 media 再等, 保证 nicesink/
/// nicesrc 已接管 component I/O (部分平台靠它们推进连通检查). 连通超时
/// 不阻断 (后续连通后自动开始收发); 组件 Failed 或喂入失败则停媒体并报错.
pub async fn activate(
    mut local: LocalIce,
    remote_sdp: &str,
    cfg: &GuiSipConfig,
    tap: &VideoFrameSink,
    codecs: IceCodecs,
) -> Result<IceCallMedia> {
    local.set_codecs(codecs);
    local.add_remote(remote_sdp).await?;
    let media = local.into_media(cfg, tap)?;
    match media.wait_connected(CONNECT_TIMEOUT).await {
        Ok(()) => info!("SIP ICE media connected"),
        Err(ipcam_ice::IceError::Timeout(_)) => {
            let (audio_state, audio_pair, video_state, video_pair) = media.debug_state().await;
            tracing::warn!(
                ?audio_state,
                ?audio_pair,
                ?video_state,
                ?video_pair,
                "ICE not connected yet, media keeps trying in background"
            );
        }
        Err(e) => return Err(e.into()),
    }
    Ok(media)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipcam_sip::sdp::{PT_H264, PT_PCMA, PT_PCMU, build_av_offer, parse_offer_all};

    /// 只有音频的 Linphone 风格 offer: Opus 动态 PT 96, 不带视频.
    fn audio_only_offer(ip: Ipv4Addr, port: u16) -> String {
        format!(
            "v=0\r\n\
             o=- 1 1 IN IP4 {ip}\r\n\
             s=-\r\n\
             c=IN IP4 {ip}\r\n\
             t=0 0\r\n\
             m=audio {port} RTP/AVP 96\r\n\
             a=rtpmap:96 opus/48000/2\r\n\
             a=fmtp:96 useinbandfec=1\r\n\
             a=sendrecv\r\n"
        )
    }

    /// SIP offer/answer 的 ICE 属性注入与回读: 双向都是 Opus(111)+H264(96),
    /// 且每路媒体各有独立 ufrag/候选.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sip_ice_offer_answer_roundtrip() {
        let ip = ipcam_sip::local_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);
        let offerer = match prepare(ip, None, true, true).await {
            Ok(local) => local,
            Err(_) => return, // libnice 不可用
        };
        let plain = build_av_offer(
            IpAddr::V4(ip),
            offerer.audio_port,
            offerer.video_port,
            &[PT_PCMU, PT_PCMA, PT_OPUS],
        )
        .to_string();
        let offer = offerer.offer_sdp(&plain).expect("offer SDP");
        let parsed_offer = parse_offer_all(offer.as_bytes()).expect("parse offer");
        let offer_codecs = offer_supports_ice(&parsed_offer).expect("offer ICE codecs");
        assert_eq!(offer_codecs.audio_pt, PT_OPUS);
        assert_eq!(offer_codecs.video_pt, Some(PT_H264));
        let offer_ice = ipcam_ice::sdp::parse_offer(&offer).expect("offer ICE");
        assert!(offer_ice.video.is_some() && offer_ice.audio.is_some());

        let mut answerer = match prepare(ip, None, false, true).await {
            Ok(local) => local,
            Err(_) => return,
        };
        let answer = answerer
            .answer_sdp(&parsed_offer, ip)
            .expect("answer build")
            .expect("ICE answer");
        let parsed_answer =
            ipcam_sip::sdp::parse_answer_all(answer.as_bytes()).expect("parse answer");
        let answer_codecs = answer_supports_ice(&parsed_answer).expect("answer ICE codecs");
        assert_eq!(answer_codecs.audio_pt, PT_OPUS);
        assert_eq!(answer_codecs.video_pt, Some(PT_H264));
        assert_eq!(parsed_answer.audio.as_ref().unwrap().payload_type, PT_OPUS);
        assert_eq!(parsed_answer.video.as_ref().unwrap().payload_type, PT_H264);
        let answer_ice = ipcam_ice::sdp::parse_offer(&answer).expect("answer ICE");
        assert!(answer_ice.video.is_some() && answer_ice.audio.is_some());
    }

    /// 两个本端 agent 通过 SIP offer/answer 交换 ICE 参数后能真正连通.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sip_ice_two_endpoints_connect() {
        let ip = ipcam_sip::local_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);
        let offerer = match prepare(ip, None, true, true).await {
            Ok(local) => local,
            Err(_) => return, // libnice 不可用
        };
        let plain = build_av_offer(
            IpAddr::V4(ip),
            offerer.audio_port,
            offerer.video_port,
            &[PT_PCMU, PT_PCMA, PT_OPUS],
        )
        .to_string();
        let offer = offerer.offer_sdp(&plain).expect("offer SDP");
        let parsed_offer = parse_offer_all(offer.as_bytes()).expect("parse offer");

        let mut answerer = match prepare(ip, None, false, true).await {
            Ok(local) => local,
            Err(_) => return,
        };
        let answer = answerer
            .answer_sdp(&parsed_offer, ip)
            .expect("answer build")
            .expect("ICE answer");

        // 本测试不建媒体管线, 用 attach_recv 接管两路 component I/O
        // (部分平台/libnice 版本不开接管则连通检查不推进).
        let (a, b, c, d) = tokio::join!(
            offerer.video_agent().attach_recv(|_| {}),
            offerer.audio_agent().attach_recv(|_| {}),
            answerer.video_agent().attach_recv(|_| {}),
            answerer.audio_agent().attach_recv(|_| {}),
        );
        a.expect("offerer video attach");
        b.expect("offerer audio attach");
        c.expect("answerer video attach");
        d.expect("answerer audio attach");

        let (a, b) = tokio::join!(offerer.add_remote(&answer), answerer.add_remote(&offer),);
        a.expect("offerer add_remote");
        b.expect("answerer add_remote");
        let (a, b) = tokio::join!(offerer.wait_connected(), answerer.wait_connected());
        a.expect("offerer connected");
        b.expect("answerer connected");
    }

    #[test]
    fn mirrors_offer_transport_attrs_into_answer() {
        let offer = "v=0\r\n\
            a=group:BUNDLE as vs\r\n\
            m=audio 60656 RTP/AVP 96\r\n\
            a=mid:as\r\n\
            a=rtcp-mux\r\n\
            m=video 60658 RTP/AVP 96\r\n\
            a=mid:vs\r\n";
        let answer = "v=0\r\n\
            c=IN IP4 192.168.11.17\r\n\
            t=0 0\r\n\
            m=audio 40000 RTP/AVP 96\r\n\
            a=rtpmap:96 opus/48000/2\r\n\
            a=ice-ufrag:local\r\n\
            m=video 0 RTP/AVP 96\r\n\
            a=rtpmap:96 AV1/90000\r\n";
        let out = mirror_offer_transport_attrs(answer, offer);
        assert!(
            !out.contains("a=group:BUNDLE"),
            "separate ICE agents must not claim BUNDLE:\n{out}"
        );
        assert!(out.contains("a=mid:as"), "answer:\n{out}");
        assert!(out.contains("a=mid:vs"), "answer:\n{out}");
        assert_eq!(out.matches("a=rtcp-mux").count(), 2, "answer:\n{out}");
        assert!(out.contains("a=rtpmap:96 opus/48000/2"), "answer:\n{out}");
    }

    /// Linphone Desktop 风格: audio-only + Opus 动态 PT 96.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sip_ice_audio_only_opus_pt_96_roundtrip() {
        let ip = ipcam_sip::local_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);
        let offerer = match prepare(ip, None, true, false).await {
            Ok(local) => local,
            Err(_) => return, // libnice 不可用
        };
        let plain = audio_only_offer(ip, offerer.audio_port);
        let offer = offerer.offer_sdp(&plain).expect("offer SDP");
        let parsed_offer = parse_offer_all(offer.as_bytes()).expect("parse offer");
        let offer_codecs = offer_supports_ice(&parsed_offer).expect("audio-only ICE offer");
        assert_eq!(offer_codecs.audio_pt, 96);
        assert_eq!(offer_codecs.video_pt, None);
        let offer_ice = ipcam_ice::sdp::parse_offer(&offer).expect("offer ICE");
        assert!(offer_ice.audio.is_some());
        assert!(offer_ice.video.is_none());

        let mut answerer = match prepare(ip, None, false, false).await {
            Ok(local) => local,
            Err(_) => return,
        };
        let answer = answerer
            .answer_sdp(&parsed_offer, ip)
            .expect("answer build")
            .expect("ICE answer");
        assert!(answer.contains("m=audio"), "answer:\n{answer}");
        assert!(
            answer.contains("a=rtpmap:96 opus/48000/2"),
            "answer:\n{answer}"
        );
        assert!(!answer.contains("m=video"), "answer:\n{answer}");
        let parsed_answer =
            ipcam_sip::sdp::parse_answer_all(answer.as_bytes()).expect("parse answer");
        let answer_codecs = answer_supports_ice(&parsed_answer).expect("audio-only ICE answer");
        assert_eq!(answer_codecs.audio_pt, 96);
        assert_eq!(answer_codecs.video_pt, None);
        let answer_ice = ipcam_ice::sdp::parse_offer(&answer).expect("answer ICE");
        assert!(answer_ice.audio.is_some());
        assert!(answer_ice.video.is_none());
    }

    /// audio-only 双端实际连通 (只 attach 音频 component).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sip_ice_audio_only_two_endpoints_connect() {
        let ip = ipcam_sip::local_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);
        let offerer = match prepare(ip, None, true, false).await {
            Ok(local) => local,
            Err(_) => return, // libnice 不可用
        };
        let plain = audio_only_offer(ip, offerer.audio_port);
        let offer = offerer.offer_sdp(&plain).expect("offer SDP");
        let parsed_offer = parse_offer_all(offer.as_bytes()).expect("parse offer");

        let mut answerer = match prepare(ip, None, false, false).await {
            Ok(local) => local,
            Err(_) => return,
        };
        let answer = answerer
            .answer_sdp(&parsed_offer, ip)
            .expect("answer build")
            .expect("ICE answer");

        let (a, b) = tokio::join!(
            offerer.audio_agent().attach_recv(|_| {}),
            answerer.audio_agent().attach_recv(|_| {}),
        );
        a.expect("offerer audio attach");
        b.expect("answerer audio attach");

        let (a, b) = tokio::join!(offerer.add_remote(&answer), answerer.add_remote(&offer),);
        a.expect("offerer add_remote");
        b.expect("answerer add_remote");
        let (a, b) = tokio::join!(offerer.wait_connected(), answerer.wait_connected());
        a.expect("offerer connected");
        b.expect("answerer connected");
    }
}
