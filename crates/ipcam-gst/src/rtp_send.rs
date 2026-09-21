//! 多源 → RTP 发送器 (SIP 通话联调用): 每路媒体独立指定源 (文件/RTSP/
//! 本机相机/麦克风), 解码后统一重编码发出.
//!
//! 链形 (每轨独立):
//!   视频: 源 → [解码] → queue → videoconvert
//!         → [videoscale → capsfilter(width≤max_width, 保持宽高比)]
//!         → x264enc → rtph264pay(config-interval=1) → udpsink
//!   音频: 源 → [解码] → queue → audioconvert → audioresample
//!         → capsfilter(8kHz/mono) → alawenc|mulawenc
//!         → rtppcmapay|rtppcmupay → udpsink
//!
//! 设计约束 (门口机黑屏三轮修复的经验, 全部固化在发送链里):
//! - x264enc aud=false + option-string slice-max-size=1300: 无 AUD,
//!   每个 NAL 小于 MTU, mode 0 对端不需要认 FU-A 分片
//! - rtph264pay config-interval=1: SPS/PPS 秒级周期重发, 对端中途
//!   开始收也能解
//! - udpsink sync=true: 按 buffer 时间戳限速, 否则以最快速度泼出去
//! - 发送 pt/音频编码必须取对端 answer 里的值 (RFC 3264)

use gstreamer as gst;
use gstreamer::prelude::*;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::GstStreamError;
use crate::ensure_init_internal;
use chain::{link_audio_send_chain, link_video_send_chain};
use ipcam_core::AudioCodec;
use source::{
    TrackKind, camera_src_name, file_uri, mic_src_name, plug_device_source, plug_uri_source,
    redact_uri_credentials,
};

mod chain;
mod source;

pub use source::{TrackSource, parse_track_source};

/// 一路媒体的发送目标: 对端收包地址 + 对端 answer 里协商出的 pt
#[derive(Debug, Clone, Copy)]
pub struct RtpDest {
    pub addr: SocketAddr,
    pub payload_type: u8,
    /// 对端解码能力的限宽: Some(w) 时源超宽会降采样到 w 以内
    /// (保持宽高比), None = 不限制 (不插 videoscale/capsfilter).
    /// 门口机给 Some(640), 能力未知的对端先按 640 保守发
    pub max_width: Option<u32>,
}

/// 从 SDP rtpmap 的编码名解析出我们支持发送的编码.
/// None = 不支持发这种 (SIP 对讲场景只发 G.711 两兄弟)
pub fn sendable_audio_codec(name: &str) -> Option<AudioCodec> {
    match AudioCodec::from_name(name) {
        c @ (AudioCodec::G711A | AudioCodec::G711U) => Some(c),
        _ => None,
    }
}

/// 音频路的发送目标: 地址 + pt + answer 协商出的编码.
/// 编码必须取 answer 里的值 -- 对端收窄到 PCMU 我们还发 PCMA,
/// 就是标签和内容都对不上的错包 (RFC 3264)
#[derive(Debug, Clone, Copy)]
pub struct AudioDest {
    pub addr: SocketAddr,
    pub payload_type: u8,
    pub codec: AudioCodec,
}

#[derive(Debug, Clone)]
pub struct RtpSendConfig {
    /// None = 不发这路 (对端没接这路媒体时)
    pub audio: Option<(TrackSource, AudioDest)>,
    pub video: Option<(TrackSource, RtpDest)>,
}

/// 发包计数 (payloader src 上的 probe 累加), 用来回答 "到底有没有数据
/// 离开我们" 这个联调第一问
#[derive(Default)]
struct SendStats {
    video_pkts: AtomicU64,
    audio_pkts: AtomicU64,
}

pub struct RtpSender {
    pipeline: gst::Pipeline,
    stop_flag: Arc<AtomicBool>,
    stats: Arc<SendStats>,
}

impl RtpSender {
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    /// (视频包数, 音频包数) -- payloader src 上的 probe 实时累加,
    /// 联调时先确认数据离开了我们, 再怀疑对端
    pub fn packet_counts(&self) -> (u64, u64) {
        (
            self.stats.video_pkts.load(Ordering::Relaxed),
            self.stats.audio_pkts.load(Ordering::Relaxed),
        )
    }
}

impl Drop for RtpSender {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start_rtp_sender(cfg: RtpSendConfig) -> Result<RtpSender, GstStreamError> {
    ensure_init_internal()?;
    if cfg.audio.is_none() && cfg.video.is_none() {
        return Err(GstStreamError::InvalidConfig(
            "neither audio nor video destination given".into(),
        ));
    }

    let pipeline = gst::Pipeline::new();
    let stats = Arc::new(SendStats::default());
    let stop_flag = Arc::new(AtomicBool::new(false));

    if let Some((source, dest)) = &cfg.video {
        build_video_track(&pipeline, source, *dest, stats.clone())?;
    }
    if let Some((source, dest)) = &cfg.audio {
        build_audio_track(&pipeline, source, *dest, stats.clone())?;
    }

    // bus 线程: 只盯 EOS/Error 打日志, 不负责拆管线.
    // 注意: iter 在 bus 进入 flushing 时也会结束 (不一定是 EOS/Error),
    // 盲目在这里 set_state(Null) 会把刚起起来的管线误杀 (启动竞态实测复现),
    // 收尾统一由 RtpSender::stop/Drop 负责
    let bus = pipeline
        .bus()
        .ok_or_else(|| GstStreamError::Init("pipeline has no bus".into()))?;
    std::thread::spawn(move || {
        let mut reason = "flushed";
        for msg in bus.iter() {
            match msg.view() {
                gst::MessageView::Eos(_) => {
                    info!("rtp sender: file playback finished (EOS)");
                    reason = "eos";
                    break;
                }
                gst::MessageView::Error(e) => {
                    // gst 元素错误文本会带原始 uri (含明文 rtsp://user:pass@...),
                    // 两个字符串各过一遍脱敏再打
                    let error_text = redact_uri_credentials(&e.error().to_string());
                    let debug_text = e
                        .debug()
                        .map(|d| redact_uri_credentials(&d))
                        .unwrap_or_default();
                    warn!(error = %error_text, debug = %debug_text, "rtp sender error");
                    reason = "error";
                    break;
                }
                _ => {}
            }
        }
        debug!(reason, "rtp sender: bus thread exiting");
    });

    // 每 2s 报一次发包数: 视频约 fps 个包, 音频 ptime=20ms 即 50 包/s,
    // 看到数字涨就证明数据确实离开了我们, 问题在对端或网络
    let stats_for_tick = stats.clone();
    let stop_for_tick = stop_flag.clone();
    std::thread::spawn(move || {
        while !stop_for_tick.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(2));
            info!(
                video_pkts = stats_for_tick.video_pkts.load(Ordering::Relaxed),
                audio_pkts = stats_for_tick.audio_pkts.load(Ordering::Relaxed),
                "rtp sender stats"
            );
        }
    });

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| GstStreamError::Init(format!("pipeline set Playing: {e}")))?;
    info!(
        audio = ?cfg.audio.as_ref().map(|(source, _)| source.label()),
        video = ?cfg.video.as_ref().map(|(source, _)| source.label()),
        "rtp sender started"
    );
    Ok(RtpSender {
        pipeline,
        stop_flag,
        stats,
    })
}

fn build_video_track(
    pipeline: &gst::Pipeline,
    source: &TrackSource,
    dest: RtpDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    info!(?source, "building video track");
    match source {
        TrackSource::File(path) => {
            let uri = file_uri(path)?;
            let pipeline = pipeline.clone();
            let chain_pipeline = pipeline.clone();
            plug_uri_source(&pipeline, &uri, TrackKind::Video, move |pad| {
                link_video_send_chain(&chain_pipeline, &pad, dest, stats.clone())
            })
        }
        TrackSource::Rtsp { uri } => {
            let uri = uri.clone();
            let pipeline = pipeline.clone();
            let chain_pipeline = pipeline.clone();
            plug_uri_source(&pipeline, &uri, TrackKind::Video, move |pad| {
                link_video_send_chain(&chain_pipeline, &pad, dest, stats.clone())
            })
        }
        TrackSource::LocalCamera { device } => {
            let pipeline = pipeline.clone();
            let chain_pipeline = pipeline.clone();
            let device = device.clone();
            plug_device_source(
                &pipeline,
                camera_src_name(),
                device.as_deref(),
                move |pad| link_video_send_chain(&chain_pipeline, &pad, dest, stats.clone()),
            )
        }
        other => Err(GstStreamError::InvalidConfig(format!(
            "unsupported source for video track: {other:?}"
        ))),
    }
}

fn build_audio_track(
    pipeline: &gst::Pipeline,
    source: &TrackSource,
    dest: AudioDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    info!(?source, "building audio track");
    match source {
        TrackSource::File(path) => {
            let uri = file_uri(path)?;
            let pipeline = pipeline.clone();
            let chain_pipeline = pipeline.clone();
            plug_uri_source(&pipeline, &uri, TrackKind::Audio, move |pad| {
                link_audio_send_chain(&chain_pipeline, &pad, dest, stats.clone())
            })
        }
        TrackSource::Rtsp { uri } => {
            let uri = uri.clone();
            let pipeline = pipeline.clone();
            let chain_pipeline = pipeline.clone();
            plug_uri_source(&pipeline, &uri, TrackKind::Audio, move |pad| {
                link_audio_send_chain(&chain_pipeline, &pad, dest, stats.clone())
            })
        }
        TrackSource::Mic => {
            let pipeline = pipeline.clone();
            let chain_pipeline = pipeline.clone();
            plug_device_source(&pipeline, mic_src_name(), None, move |pad| {
                link_audio_send_chain(&chain_pipeline, &pad, dest, stats.clone())
            })
        }
        other => Err(GstStreamError::InvalidConfig(format!(
            "unsupported source for audio track: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// SDP rtpmap 编码名解析: 大小写不敏感, 未知编码返回 None
    #[test]
    fn audio_codec_from_codec_name() {
        assert_eq!(sendable_audio_codec("PCMU"), Some(AudioCodec::G711U));
        assert_eq!(sendable_audio_codec("pcmu"), Some(AudioCodec::G711U));
        assert_eq!(sendable_audio_codec("PcMu"), Some(AudioCodec::G711U));
        assert_eq!(sendable_audio_codec("PCMA"), Some(AudioCodec::G711A));
        assert_eq!(sendable_audio_codec("pcma"), Some(AudioCodec::G711A));
        assert_eq!(sendable_audio_codec("opus"), None);
    }

    /// 文件源视频轨: 解码重编码后 3 秒内必须出 RTP 包
    #[test]
    fn video_file_track_emits_packets() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let sender = start_rtp_sender(RtpSendConfig {
            audio: None,
            video: Some((
                TrackSource::File(PathBuf::from("../../assets/oceans.mp4")),
                RtpDest {
                    addr: "127.0.0.1:40002".parse().unwrap(),
                    payload_type: 96,
                    max_width: Some(640),
                },
            )),
        })
        .expect("sender starts");
        std::thread::sleep(Duration::from_secs(3));
        let (video, _audio) = sender.packet_counts();
        sender.stop();
        assert!(video > 0, "no video RTP packets emitted");
    }

    /// 双轨都来自同一文件: 两路都必须出包 (文件会被解码两次, 联调工具不做共享)
    #[test]
    fn av_file_tracks_emit_packets() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let file = TrackSource::File(PathBuf::from("../../assets/oceans.mp4"));
        let sender = start_rtp_sender(RtpSendConfig {
            audio: Some((
                file.clone(),
                AudioDest {
                    addr: "127.0.0.1:40000".parse().unwrap(),
                    payload_type: 8,
                    codec: AudioCodec::G711A,
                },
            )),
            video: Some((
                file,
                RtpDest {
                    addr: "127.0.0.1:40002".parse().unwrap(),
                    payload_type: 96,
                    max_width: Some(640),
                },
            )),
        })
        .expect("sender starts");
        std::thread::sleep(Duration::from_secs(3));
        let (video, audio) = sender.packet_counts();
        sender.stop();
        assert!(video > 0, "no video RTP packets emitted");
        assert!(audio > 0, "no audio RTP packets emitted");
    }
}
