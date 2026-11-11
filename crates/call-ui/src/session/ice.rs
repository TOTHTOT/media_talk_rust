//! SIP + ICE 会话准备: 双 agent 收集/喂入/连通等待与 SDP 组装.
//!
//! SIP 路径的 ICE 使用标准媒体 SDP (`m=audio`/`m=video` 内嵌
//! `a=ice-ufrag`/`a=ice-pwd`/`a=candidate`), 对端不回 ICE 时由调用方
//! 回退普通 RTP. ICE 媒体链固定 Opus(111)/H264(96), 因此只有在
//! 协商结果正好落在这两个 payload type 上时才启用.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use anyhow::{Context as _, Result};
use ipcam_core::{AudioCodec, VideoCodec};
use ipcam_gst::VideoFrameSink;
use ipcam_ice::blob::IceBlob;
use ipcam_ice::sdp::{self, RemoteMedia};
use ipcam_ice::{IceAgent, IceConfig};
use ipcam_sip::negotiate;
use ipcam_sip::sdp::{OfferMedias, PT_H264, PT_OPUS, PeerMedias};
use tracing::info;

use super::types::{GuiSipConfig, RtpPorts};
use crate::media::IceCallMedia;

/// 建立阶段候选收集上限: 超时也接受已收集到的 host 候选, 不让 STUN
/// 不可达阻塞呼叫建立.
const GATHER_TIMEOUT: Duration = Duration::from_secs(3);
/// ICE 连通性检查上限.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 本端双 agent 与 gather 结果. Drop 时关闭 agent (媒体句柄必须先停).
pub struct LocalIce {
    video_agent: Option<IceAgent>,
    audio_agent: Option<IceAgent>,
    pub video_blob: IceBlob,
    pub audio_blob: IceBlob,
    pub video_port: u16,
    pub audio_port: u16,
}

impl LocalIce {
    /// 写进 SIP SDP m= 行的接收端口 (也是 ICE 失败回退普通 RTP 的端口).
    pub fn ports(&self) -> RtpPorts {
        RtpPorts {
            audio: self.audio_port,
            video: self.video_port,
        }
    }

    /// 把 ICE 属性注入普通 offer SDP.
    pub fn offer_sdp(&self, plain: &str) -> Result<String> {
        Ok(sdp::append_ice(
            plain,
            Some(&self.video_blob),
            Some(&self.audio_blob),
        )?)
    }

    /// 按对端 offer 构造带 ICE 的 answer. 协商结果不是 Opus(111)+H264(96)
    /// 时返回 None (调用方回退普通 RTP answer).
    pub fn answer_sdp(&self, offer: &OfferMedias, local_ip: Ipv4Addr) -> Result<Option<String>> {
        let (audio, video, answer) = negotiate::build_answer(
            offer,
            IpAddr::V4(local_ip),
            self.audio_port,
            self.video_port,
            ice_audio_pred,
            ice_video_pred,
        );
        let audio_ok = matches!(&audio, Some((pt, AudioCodec::Opus)) if *pt == PT_OPUS);
        let video_ok = matches!(&video, Some((pt, VideoCodec::H264, _)) if *pt == PT_H264);
        if !audio_ok || !video_ok {
            return Ok(None);
        }
        Ok(Some(sdp::append_ice(
            &answer,
            Some(&self.video_blob),
            Some(&self.audio_blob),
        )?))
    }

    /// 喂入对端标准媒体 SDP 中的 ICE 参数 (video/audio 都必须有).
    pub async fn add_remote(&self, remote_text: &str) -> Result<()> {
        let remote = sdp::parse_offer(remote_text).context("parse remote ICE SDP")?;
        self.add_remote_media(&remote).await
    }

    async fn add_remote_media(&self, remote: &RemoteMedia) -> Result<()> {
        let video = remote
            .video
            .as_ref()
            .context("remote SDP has no video ICE")?;
        let audio = remote
            .audio
            .as_ref()
            .context("remote SDP has no audio ICE")?;
        self.video_agent().add_remote(video).await?;
        self.audio_agent().add_remote(audio).await?;
        Ok(())
    }

    /// 等两路媒体都 Connected (测试用; 生产路径由媒体管线先启动再等).
    #[cfg(test)]
    pub async fn wait_connected(&self) -> Result<(), ipcam_ice::IceError> {
        let (video, audio) = tokio::join!(
            self.video_agent().wait_connected(CONNECT_TIMEOUT),
            self.audio_agent().wait_connected(CONNECT_TIMEOUT),
        );
        video?;
        audio?;
        info!("SIP ICE connected (video+audio)");
        Ok(())
    }

    /// 起 nicesink/nicesrc 媒体管线 (agent 所有权移入媒体句柄).
    pub fn into_media(self, cfg: &GuiSipConfig, tap: &VideoFrameSink) -> Result<IceCallMedia> {
        let mut this = self;
        let video = this
            .video_agent
            .take()
            .context("video ICE agent already taken")?;
        let audio = this
            .audio_agent
            .take()
            .context("audio ICE agent already taken")?;
        Ok(IceCallMedia::start(video, audio, cfg, tap)?)
    }

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

/// 建两个独立 agent 并收集本地候选 (视频/音频并行).
pub async fn prepare(
    local_ip: Ipv4Addr,
    stun: Option<String>,
    controlling: bool,
) -> Result<LocalIce> {
    let video_agent = IceAgent::new(IceConfig {
        stun: stun.clone(),
        controlling,
        components: 1,
    })?;
    let audio_agent = match IceAgent::new(IceConfig {
        stun,
        controlling,
        components: 1,
    }) {
        Ok(agent) => agent,
        Err(e) => {
            video_agent.close();
            return Err(e.into());
        }
    };
    let gathered = tokio::try_join!(
        video_agent.local_blob_within(GATHER_TIMEOUT),
        audio_agent.local_blob_within(GATHER_TIMEOUT),
    );
    let (video_blob, audio_blob) = match gathered {
        Ok(blobs) => blobs,
        Err(e) => {
            video_agent.close();
            audio_agent.close();
            return Err(e.into());
        }
    };
    let endpoint = |blob: &IceBlob, what: &str| -> Result<u16> {
        sdp::media_endpoint(blob, IpAddr::V4(local_ip))
            .map(|addr| addr.port())
            .with_context(|| format!("ICE has no {what} UDP host candidate on {local_ip}"))
    };
    let video_port = match endpoint(&video_blob, "video") {
        Ok(port) => port,
        Err(e) => {
            video_agent.close();
            audio_agent.close();
            return Err(e);
        }
    };
    let audio_port = match endpoint(&audio_blob, "audio") {
        Ok(port) => port,
        Err(e) => {
            video_agent.close();
            audio_agent.close();
            return Err(e);
        }
    };
    info!(video_port, audio_port, "SIP ICE local candidates gathered");
    Ok(LocalIce {
        video_agent: Some(video_agent),
        audio_agent: Some(audio_agent),
        video_blob,
        audio_blob,
        video_port,
        audio_port,
    })
}

/// 对端 SDP 的协商结果是否正好是 ICE 媒体链支持的 Opus/H264 payload.
pub fn answer_supports_ice(peers: &PeerMedias) -> bool {
    let audio = peers.audio.as_ref().is_some_and(|p| {
        p.addr.port() != 0 && p.payload_type == PT_OPUS && p.codec.eq_ignore_ascii_case("opus")
    });
    let video = peers.video.as_ref().is_some_and(|p| {
        p.addr.port() != 0 && p.payload_type == PT_H264 && p.codec.eq_ignore_ascii_case("h264")
    });
    audio && video
}

/// 对端 offer 是否含 ICE 媒体链可用的 Opus/H264 (PT 必须与本端发送链一致).
pub fn offer_supports_ice(offer: &OfferMedias) -> bool {
    let audio = offer.audio.as_ref().is_some_and(|m| {
        m.port != 0
            && m.codecs.iter().any(|c| {
                c.payload_type == PT_OPUS
                    && c.codec.eq_ignore_ascii_case("opus")
                    && c.clock_rate == 48_000
            })
    });
    let video = offer.video.as_ref().is_some_and(|m| {
        m.port != 0
            && m.codecs
                .iter()
                .any(|c| c.payload_type == PT_H264 && c.codec.eq_ignore_ascii_case("h264"))
    });
    audio && video
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

/// 喂入对端 ICE -> 起媒体 -> 等连通. 先起 media 再等, 保证 nicesink/
/// nicesrc 已接管 component I/O (部分平台靠它们推进连通检查). 连通超时
/// 不阻断 (后续连通后自动开始收发); 组件 Failed 或喂入失败则停媒体并报错.
pub async fn activate(
    local: LocalIce,
    remote_sdp: &str,
    cfg: &GuiSipConfig,
    tap: &VideoFrameSink,
) -> Result<IceCallMedia> {
    local.add_remote(remote_sdp).await?;
    let media = local.into_media(cfg, tap)?;
    match media.wait_connected(CONNECT_TIMEOUT).await {
        Ok(()) => info!("SIP ICE media connected"),
        Err(ipcam_ice::IceError::Timeout(_)) => {
            tracing::warn!("ICE not connected yet, media keeps trying in background");
        }
        Err(e) => return Err(e.into()),
    }
    Ok(media)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipcam_sip::sdp::{PT_H264, PT_PCMA, PT_PCMU, build_av_offer, parse_offer_all};

    /// SIP offer/answer 的 ICE 属性注入与回读: 双向都必须是
    /// Opus(111)+H264(96), 且每路媒体各有独立 ufrag/候选.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sip_ice_offer_answer_roundtrip() {
        let ip = ipcam_sip::local_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);
        let offerer = match prepare(ip, None, true).await {
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
        assert!(offer_supports_ice(&parsed_offer), "offer:\n{offer}");
        let offer_ice = ipcam_ice::sdp::parse_offer(&offer).expect("offer ICE");
        assert!(offer_ice.video.is_some() && offer_ice.audio.is_some());

        let answerer = match prepare(ip, None, false).await {
            Ok(local) => local,
            Err(_) => return,
        };
        let answer = answerer
            .answer_sdp(&parsed_offer, ip)
            .expect("answer build")
            .expect("ICE answer");
        let parsed_answer =
            ipcam_sip::sdp::parse_answer_all(answer.as_bytes()).expect("parse answer");
        assert!(answer_supports_ice(&parsed_answer), "answer:\n{answer}");
        assert_eq!(parsed_answer.audio.as_ref().unwrap().payload_type, PT_OPUS);
        assert_eq!(parsed_answer.video.as_ref().unwrap().payload_type, PT_H264);
        let answer_ice = ipcam_ice::sdp::parse_offer(&answer).expect("answer ICE");
        assert!(answer_ice.video.is_some() && answer_ice.audio.is_some());
    }

    /// 两个本端 agent 通过 SIP offer/answer 交换 ICE 参数后能真正连通.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sip_ice_two_endpoints_connect() {
        let ip = ipcam_sip::local_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);
        let offerer = match prepare(ip, None, true).await {
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

        let answerer = match prepare(ip, None, false).await {
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
}
