//! 多源 → RTP 发送器 (SIP 通话联调用): 每路媒体独立指定源 (文件/RTSP/
//! 本机相机/麦克风), 解码后统一重编码发出.
//!
//! 链形 (每轨独立):
//!   视频: 源 → [解码] → queue → videoconvert → x264enc
//!         → rtph264pay(config-interval=1) → udpsink
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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::GstStreamError;
use crate::ensure_init_internal;
use crate::pipeline::make;

/// 一路媒体的发送目标: 对端收包地址 + 对端 answer 里协商出的 pt
#[derive(Debug, Clone, Copy)]
pub struct RtpDest {
    pub addr: SocketAddr,
    pub payload_type: u8,
}

/// 音频编码 (G.711 两兄弟, SIP 对讲场景基本只遇到这两个)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// PCMA (A-law), 静态 pt 8
    Pcma,
    /// PCMU (u-law), 静态 pt 0, 设备的原生偏好
    Pcmu,
}

impl AudioCodec {
    /// 从 SDP rtpmap 的编码名解析; None = 我们不支持发这种编码
    pub fn from_codec_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "PCMA" => Some(Self::Pcma),
            "PCMU" => Some(Self::Pcma),
            _ => None,
        }
    }

    /// (encoder, payloader) 元件名
    fn elements(self) -> (&'static str, &'static str) {
        match self {
            Self::Pcma => ("alawenc", "rtppcmapay"),
            Self::Pcmu => ("mulawenc", "rtppcmupay"),
        }
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

/// 一路媒体的来源
#[derive(Debug, Clone)]
pub enum TrackSource {
    /// 文件 (mp4/wav/mp3 均可, 只取当前需要的轨)
    File(PathBuf),
    /// RTSP 网络相机 (凭据内嵌在 uri 里, H264/H265 均可)
    Rtsp { uri: String },
    /// 本机相机: Linux v4l2src (USB 和 MIPI/CSI 同元件), Windows ksvideosrc
    LocalCamera { device: Option<String> },
    /// 本机麦克风: Windows wasapisrc, Linux alsasrc
    Mic,
}

/// 要建的是哪一路 (源段按它匹配 uridecodebin 的裸流 pad)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackKind {
    Audio,
    Video,
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
                    warn!(error = %e.error(), debug = ?e.debug(), "rtp sender error");
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
        audio = ?cfg.audio,
        video = ?cfg.video,
        "rtp sender started"
    );
    Ok(RtpSender {
        pipeline,
        stop_flag,
        stats,
    })
}

/// 文件/RTSP 源: uridecodebin 解码出裸流, pad-added 里按 TrackKind 匹配,
/// 命中的 pad 交给 on_raw_pad 接发送链. 源里没有请求的轨 = pad 永远不来,
/// 不阻塞另一路 (靠 stats 0 包发现)
fn plug_uri_source(
    pipeline: &gst::Pipeline,
    uri: &str,
    kind: TrackKind,
    on_raw_pad: impl Fn(gst::Pad) -> Result<(), GstStreamError> + Send + Sync + 'static,
) -> Result<(), GstStreamError> {
    let dec = make("uridecodebin")?;
    dec.set_property("uri", uri);
    pipeline
        .add(&dec)
        .map_err(|e| GstStreamError::Init(format!("add uridecodebin: {e}")))?;
    dec.connect_pad_added(move |_dec, pad| {
        let Some(caps) = pad.current_caps() else {
            return;
        };
        let Some(s) = caps.structure(0) else { return };
        let hit = matches!(
            (kind, s.name().as_str()),
            (TrackKind::Video, "video/x-raw") | (TrackKind::Audio, "audio/x-raw")
        );
        if hit {
            if let Err(e) = on_raw_pad(pad.clone()) {
                warn!(error = %e, "rtp sender: failed to link send chain");
            }
        }
    });
    dec.sync_state_with_parent()
        .map_err(|e| GstStreamError::Init(format!("sync uridecodebin: {e}")))?;
    Ok(())
}

fn file_uri(path: &std::path::Path) -> Result<String, GstStreamError> {
    let abs = path.canonicalize().map_err(|e| {
        GstStreamError::InvalidConfig(format!("media file not found: {}: {e}", path.display()))
    })?;
    let uri = gst::glib::filename_to_uri(&abs, None)
        .map_err(|e| GstStreamError::InvalidConfig(format!("to file uri: {e}")))?;
    Ok(uri.to_string())
}

fn build_video_track(
    pipeline: &gst::Pipeline,
    source: &TrackSource,
    dest: RtpDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    match source {
        TrackSource::File(path) => {
            let uri = file_uri(path)?;
            let pipeline = pipeline.clone();
            let chain_pipeline = pipeline.clone();
            plug_uri_source(&pipeline, &uri, TrackKind::Video, move |pad| {
                link_video_send_chain(&chain_pipeline, &pad, dest, stats.clone())
            })
        }
        other => Err(GstStreamError::InvalidConfig(format!(
            "video source not wired yet: {other:?}"
        ))),
    }
}

fn build_audio_track(
    pipeline: &gst::Pipeline,
    source: &TrackSource,
    dest: AudioDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    match source {
        TrackSource::File(path) => {
            let uri = file_uri(path)?;
            let pipeline = pipeline.clone();
            let chain_pipeline = pipeline.clone();
            plug_uri_source(&pipeline, &uri, TrackKind::Audio, move |pad| {
                link_audio_send_chain(&chain_pipeline, &pad, dest, stats.clone())
            })
        }
        other => Err(GstStreamError::InvalidConfig(format!(
            "audio source not wired yet: {other:?}"
        ))),
    }
}

/// 视频发送链 (所有源共用): 裸流重编码, 参数集周期重发
fn link_video_send_chain(
    pipeline: &gst::Pipeline,
    raw_pad: &gst::Pad,
    dest: RtpDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    let queue = make("queue")?;
    let convert = make("videoconvert")?;
    let enc = make("x264enc")?;
    enc.set_property_from_str("tune", "zerolatency");
    enc.set_property_from_str("speed-preset", "veryfast");
    enc.set_property("bitrate", 400u32); // kbps, CIF 档足够
    enc.set_property("key-int-max", 30u32); // 秒级 IDR, 对端中途收也能起
    // 不插 AUD NAL (设备解析器不认); 每个 NAL 切到 MTU 以下,
    // 从源头消除 FU-A 分片需求 (mode 0)
    enc.set_property("aud", false);
    enc.set_property("option-string", "slice-max-size=1300");
    let pay = make("rtph264pay")?;
    pay.set_property("pt", dest.payload_type as u32);
    pay.set_property("config-interval", 1i32);
    install_pkt_probe(&pay, stats.clone(), false)?;
    let sink = make_udpsink(dest)?;
    add_link_and_plug(
        pipeline,
        raw_pad,
        &[&queue, &convert, &enc, &pay, &sink],
        "video",
    )
}

/// 音频发送链 (所有源共用): 裸流重采样成 answer 协商出的 G.711 (8kHz 单声道)
fn link_audio_send_chain(
    pipeline: &gst::Pipeline,
    raw_pad: &gst::Pad,
    dest: AudioDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    let queue = make("queue")?;
    let convert = make("audioconvert")?;
    let resample = make("audioresample")?;
    // G.711 定死 8kHz; capsfilter 强制输出格式, 不依赖源采样率
    let caps = make("capsfilter")?;
    caps.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("rate", 8000i32)
            .field("channels", 1i32)
            .build(),
    );
    // 编码器/payloader 按 answer 协商结果选: PCMA→alawenc/rtppcmapay,
    // PCMU→mulawenc/rtppcmupay
    let (enc_name, pay_name) = dest.codec.elements();
    let enc = make(enc_name)?;
    let pay = make(pay_name)?;
    pay.set_property("pt", dest.payload_type as u32);
    install_pkt_probe(&pay, stats.clone(), true)?;
    let sink = make_udpsink(RtpDest {
        addr: dest.addr,
        payload_type: dest.payload_type,
    })?;
    add_link_and_plug(
        pipeline,
        raw_pad,
        &[&queue, &convert, &resample, &caps, &enc, &pay, &sink],
        "audio",
    )
}

fn make_udpsink(dest: RtpDest) -> Result<gst::Element, GstStreamError> {
    let sink = make("udpsink")?;
    sink.set_property("host", dest.addr.ip().to_string());
    sink.set_property("port", dest.addr.port() as i32);
    // sync=true: 按码流时间戳限速发送; async=false: 不等 clock 对齐 preroll
    sink.set_property("sync", true);
    sink.set_property("async", false);
    Ok(sink)
}

/// 在 payloader 的 src pad 上数包: 每个 buffer = 一个 RTP 包, 这是个探测器
fn install_pkt_probe(
    pay: &gst::Element,
    stats: Arc<SendStats>,
    is_audio: bool,
) -> Result<(), GstStreamError> {
    let src_pad = pay
        .static_pad("src")
        .ok_or_else(|| GstStreamError::Link("payloader has no src pad".into()))?;
    src_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
        let counter = if is_audio {
            &stats.audio_pkts
        } else {
            &stats.video_pkts
        };
        counter.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
    Ok(())
}

/// 公共尾巴: 元件入管线 → 依次互链 → 源动态 pad 插到链头 →
/// 同步状态 (链条是在管线已 Playing 后才接上的, 必须手动 sync)
fn add_link_and_plug(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
    chain: &[&gst::Element],
    what: &str,
) -> Result<(), GstStreamError> {
    pipeline
        .add_many(chain)
        .map_err(|e| GstStreamError::Init(format!("add {what} chain: {e}")))?;
    gst::Element::link_many(chain)
        .map_err(|e| GstStreamError::Link(format!("link {what} chain: {e}")))?;
    let head_sink = chain[0]
        .static_pad("sink")
        .ok_or_else(|| GstStreamError::Link(format!("{what} chain head has no sink pad")))?;
    src_pad
        .link(&head_sink)
        .map_err(|e| GstStreamError::Link(format!("plug src pad to {what} chain: {e}")))?;
    for elem in chain {
        elem.sync_state_with_parent()
            .map_err(|e| GstStreamError::Link(format!("sync {what} chain state: {e}")))?;
    }
    info!(what, "rtp sender: chain linked");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                },
            )),
        })
        .expect("sender starts");
        std::thread::sleep(Duration::from_secs(3));
        let (video, _audio) = sender.packet_counts();
        sender.stop();
        assert!(video > 0, "no video RTP packets emitted");
    }
}
