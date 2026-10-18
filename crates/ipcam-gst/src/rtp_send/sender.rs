//! 发送器实现: 按 RtpSendConfig 组装管线 (源插拔见 source.rs,
//! 重编码发送链见 chain.rs), 挂 bus 日志线程和发包计数.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::GstStreamError;
use crate::ensure_init_internal;

use super::chain::{link_audio_send_chain, link_video_send_chain};
use super::config::{AudioDest, RtpDest, RtpSendConfig};
use super::source::{
    TrackKind, TrackSource, camera_src_name, file_uri, mic_src_name, plug_device_source,
    plug_shared_uri_source, plug_uri_source, redact_uri_credentials,
};

pub struct RtpSender {
    pipeline: Arc<gst::Pipeline>,
    stop_flag: Arc<AtomicBool>,
    stats: Arc<crate::PktStats>,
}

impl RtpSender {
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        let _ = self.pipeline.clone().set_state(gst::State::Null);
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

/// 检测 audio 和 video 是否来自同一文件, 是则返回 (文件路径, audio目标, video目标)
fn shared_file_info(
    audio: &Option<(TrackSource, AudioDest)>,
    video: &Option<(TrackSource, RtpDest)>,
) -> Option<(std::path::PathBuf, AudioDest, RtpDest)> {
    match (audio, video) {
        (Some((TrackSource::File(ap), ad)), Some((TrackSource::File(vp), vd))) if ap == vp => {
            Some((ap.clone(), *ad, *vd))
        }
        _ => None,
    }
}

/// 同文件共享 uridecodebin3: 只建一个解码器, audio pad 和 video pad 各自接发送链.
///
/// # Arguments
///
/// * `pipeline`: GStreamer 管线, 闭包捕获 Arc 引用, caller 负责生命周期
/// * `uri`: 文件绝对路径转成的 `file://` URI
/// * `audio_dest`: 音频发送目标 (地址/pt/编码)
/// * `video_dest`: 视频发送目标 (地址/pt/编码/限宽)
/// * `stats`: 发包计数器 Arc, 闭包捕获克隆
///
/// returns: Result<(), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
fn build_shared_file_track(
    pipeline: &Arc<gst::Pipeline>,
    uri: &str,
    audio_dest: AudioDest,
    video_dest: RtpDest,
    audio_socket: Option<crate::SharedUdpSocket>,
    video_socket: Option<crate::SharedUdpSocket>,
    stats: &Arc<crate::PktStats>,
) -> Result<(), GstStreamError> {
    let stats_audio = stats.clone();
    let stats_video = stats.clone();
    let pipeline_for_audio = pipeline.clone();
    let pipeline_for_video = pipeline.clone();
    plug_shared_uri_source(
        pipeline,
        uri,
        Some(std::sync::Arc::new(move |pad| {
            link_audio_send_chain(
                &pipeline_for_audio,
                &pad,
                audio_dest,
                audio_socket.clone(),
                stats_audio.clone(),
            )
        })),
        Some(std::sync::Arc::new(move |pad| {
            link_video_send_chain(
                &pipeline_for_video,
                &pad,
                video_dest,
                video_socket.clone(),
                stats_video.clone(),
            )
        })),
    )
}

/// 起 RTP 发送器: 按 RtpSendConfig 组装管线 (源插拔见 source.rs,
/// 重编码发送链见 chain.rs), 挂 bus 日志线程和发包计数.
///
/// # Arguments
///
/// * `cfg`: 发送器配置 (音频/视频源 + 目标地址)
///
/// returns: Result<RtpSender, GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub fn start_rtp_sender(cfg: RtpSendConfig) -> Result<RtpSender, GstStreamError> {
    info!(send_config = ?cfg);
    ensure_init_internal()?;
    if cfg.audio.is_none() && cfg.video.is_none() {
        return Err(GstStreamError::InvalidConfig(
            "neither audio nor video destination given".into(),
        ));
    }

    let pipeline = Arc::new(gst::Pipeline::new());
    let stats = Arc::new(crate::PktStats::default());
    let stop_flag = Arc::new(AtomicBool::new(false));

    // 同文件共享: audio 和 video 都是 TrackSource::File 且路径相同,
    // 只建一个 uridecodebin3 解码一次, 音视频 pad 各自接发送链
    let shared_file = shared_file_info(&cfg.audio, &cfg.video);

    if let Some((file_path, audio_dest, video_dest)) = shared_file {
        let uri = file_uri(&file_path)?;
        info!("some audio and video path");
        build_shared_file_track(
            &pipeline,
            &uri,
            audio_dest,
            video_dest,
            cfg.audio_socket.clone(),
            cfg.video_socket.clone(),
            &stats,
        )?;
    } else {
        build_tracks(&pipeline, &cfg, &stats)?;
    }

    // bus 线程: 只盯 EOS/Error 打日志, 不负责拆管线.
    // 注意: iter 在 bus 进入 flushing 时也会结束 (不一定是 EOS/Error),
    // 盲目在这里 set_state(Null) 会把刚起起来的管线误杀 (启动竞态实测复现),
    // 收尾统一由 RtpSender::stop/Drop 负责
    let bus = pipeline
        .clone()
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

/// 通用的单路媒体 track 构建: File/Rtsp 走 uridecodebin3, 设备走对应源元件.
/// kind 决定用 video 还是 audio 的 pad 匹配; chain_fn 是该路的发送链回调.
///
/// # Arguments
///
/// * `pipeline`: GStreamer 管线
/// * `source`: 媒体源 (File/Rtsp/LocalCamera/Mic)
/// * `kind`: 音频或视频
/// * `chain_fn`: 裸流 pad 出现时的发送链回调
///
/// returns: Result<(), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
fn build_track(
    pipeline: &gst::Pipeline,
    source: &TrackSource,
    kind: TrackKind,
    chain_fn: impl Fn(gst::Pad) -> Result<(), GstStreamError> + Send + Sync + 'static,
) -> Result<(), GstStreamError> {
    match source {
        TrackSource::File(path) => {
            let uri = file_uri(path)?;
            let pipeline = pipeline.clone();
            plug_uri_source(&pipeline, &uri, kind, move |pad| {
                chain_fn(pad)?;
                Ok(())
            })
        }
        TrackSource::Rtsp { uri } => {
            let uri = uri.clone();
            let pipeline = pipeline.clone();
            plug_uri_source(&pipeline, &uri, kind, move |pad| {
                chain_fn(pad)?;
                Ok(())
            })
        }
        TrackSource::LocalCamera { device } => {
            let pipeline = pipeline.clone();
            plug_device_source(
                &pipeline,
                camera_src_name(),
                device.as_deref(),
                move |pad| {
                    chain_fn(pad)?;
                    Ok(())
                },
            )
        }
        TrackSource::Mic => {
            let pipeline = pipeline.clone();
            plug_device_source(&pipeline, mic_src_name(), None, move |pad| {
                chain_fn(pad)?;
                Ok(())
            })
        }
    }
}

fn build_video_track(
    pipeline: &gst::Pipeline,
    source: &TrackSource,
    dest: RtpDest,
    socket: Option<crate::SharedUdpSocket>,
    stats: Arc<crate::PktStats>,
) -> Result<(), GstStreamError> {
    info!(?source, "building video track");
    let pipeline_for_chain = pipeline.clone();
    let pipeline_for_build = pipeline.clone();
    let chain_fn = move |pad| {
        link_video_send_chain(&pipeline_for_chain, &pad, dest, socket.clone(), stats.clone())
    };
    build_track(&pipeline_for_build, source, TrackKind::Video, chain_fn)
}

fn build_audio_track(
    pipeline: &gst::Pipeline,
    source: &TrackSource,
    dest: AudioDest,
    socket: Option<crate::SharedUdpSocket>,
    stats: Arc<crate::PktStats>,
) -> Result<(), GstStreamError> {
    info!(?source, "building audio track");
    let pipeline_for_chain = pipeline.clone();
    let pipeline_for_build = pipeline.clone();
    let chain_fn = move |pad| {
        link_audio_send_chain(&pipeline_for_chain, &pad, dest, socket.clone(), stats.clone())
    };
    build_track(&pipeline_for_build, source, TrackKind::Audio, chain_fn)
}

/// Build video and/or audio tracks based on config.
/// Centralized track building to avoid duplication between shared-file and separate paths.
fn build_tracks(
    pipeline: &Arc<gst::Pipeline>,
    cfg: &RtpSendConfig,
    stats: &Arc<crate::PktStats>,
) -> Result<(), GstStreamError> {
    if let Some((source, dest)) = &cfg.video {
        build_video_track(
            pipeline,
            source,
            *dest,
            cfg.video_socket.clone(),
            stats.clone(),
        )?;
    }
    if let Some((source, dest)) = &cfg.audio {
        build_audio_track(
            pipeline,
            source,
            *dest,
            cfg.audio_socket.clone(),
            stats.clone(),
        )?;
    }
    Ok(())
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
            audio_socket: None,
            video_socket: None,
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
            audio_socket: None,
            video_socket: None,
        })
        .expect("sender starts");
        std::thread::sleep(Duration::from_secs(3));
        let (video, audio) = sender.packet_counts();
        sender.stop();
        assert!(video > 0, "no video RTP packets emitted");
        assert!(audio > 0, "no audio RTP packets emitted");
    }
}
