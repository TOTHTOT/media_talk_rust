//! 发送器实现: 按 RtpSendConfig 组装管线 (源插拔见 source.rs,
//! 重编码发送链见 chain.rs), 挂 bus 日志线程和发包计数.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::GstStreamError;
use crate::ensure_init_internal;

use super::chain::{link_audio_send_chain, link_video_send_chain};
use super::config::{AudioDest, RtpDest, RtpSendConfig};
use super::source::{
    TrackKind, TrackSource, camera_src_name, file_uri, mic_src_name, plug_device_source,
    plug_uri_source, redact_uri_credentials,
};

/// 发包计数 (payloader src 上的 probe 累加), 用来回答 "到底有没有数据
/// 离开我们" 这个联调第一问
#[derive(Default)]
pub(super) struct SendStats {
    pub(super) video_pkts: AtomicU64,
    pub(super) audio_pkts: AtomicU64,
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
    use ipcam_core::AudioCodec;
    use std::path::PathBuf;

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
