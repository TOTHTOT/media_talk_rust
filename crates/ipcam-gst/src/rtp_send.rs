//! 文件 → RTP 发送器 (SIP 通话联调用): 把本地媒体文件按 SDP 协商结果
//! 打成 RTP 发往对端.
//!
//! 管线 (按需各起一路, qtdemux 的 pad 是动态的, pad-added 里分流):
//!   视频: filesrc → qtdemux → h264parse(config-interval=1)
//!         → capsfilter(byte-stream/au) → rtph264pay → udpsink
//!   音频: filesrc → qtdemux → aacparse → avdec_aac → audioconvert
//!         → audioresample → capsfilter(8kHz/mono) → alawenc → rtppcmapay → udpsink
//!
//! 设计约束:
//! - 视频不重编码: 源文件必须已经是 H264 (rtph264pay 只吃 H264 流)
//! - 强制 byte-stream/au: mp4 里是 avcC 格式, rtph264pay 对 annexb
//!   (byte-stream) 兼容性最稳, 别赌对端实现对 avc 的支持
//! - udpsink sync=true: 按 buffer 时间戳限速, 否则文件会以最快速度泼出去,
//!   对端 jitter buffer 直接炸
//! - h264parse config-interval=1: SPS/PPS 周期随码流带内重发. SIP 不像
//!   MP4 有带外参数集, 对端从中途开始收也要能解
//! - 发送用的 payload type 必须取对端 answer 里的值 (动态 pt 分方向,
//!   见 ipcam-sip sdp 模块注释)

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

#[derive(Debug, Clone)]
pub struct RtpSendConfig {
    /// 源文件路径 (视频必须 H264, 音频任意可解码格式)
    pub file: PathBuf,
    /// None = 不发这路 (对端没接这路媒体时)
    pub audio: Option<RtpDest>,
    pub video: Option<RtpDest>,
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
    if !cfg.file.exists() {
        return Err(GstStreamError::InvalidConfig(format!(
            "media file not found: {}",
            cfg.file.display()
        )));
    }
    if cfg.audio.is_none() && cfg.video.is_none() {
        return Err(GstStreamError::InvalidConfig(
            "neither audio nor video destination given".into(),
        ));
    }

    let pipeline = gst::Pipeline::new();
    let src = make("filesrc")?;
    src.set_property("location", cfg.file.to_string_lossy().as_ref());
    let demux = make("qtdemux")?;
    pipeline
        .add_many([&src, &demux])
        .map_err(|e| GstStreamError::Init(format!("add src/demux: {e}")))?;
    src.link(&demux)
        .map_err(|e| GstStreamError::Link(format!("filesrc to qtdemux: {e}")))?;

    let stats = Arc::new(SendStats::default());
    let stop_flag = Arc::new(AtomicBool::new(false));
    install_demux_pad_added(&pipeline, &demux, &cfg, stats.clone());

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
        file = %cfg.file.display(),
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

/// qtdemux 的流 pad 是动态出现的 (解复用后才知道有几路), 每来一路
/// 按 caps 分流到对应的 RTP 发送链
fn install_demux_pad_added(
    pipeline: &gst::Pipeline,
    demux: &gst::Element,
    cfg: &RtpSendConfig,
    stats: Arc<SendStats>,
) {
    let pipeline = pipeline.clone();
    let cfg = cfg.clone();
    demux.connect_pad_added(move |_demux, pad| {
        let caps = match pad.current_caps().or_else(|| Some(pad.query_caps(None))) {
            Some(c) => c,
            None => return,
        };
        let Some(s) = caps.structure(0) else { return };
        let r = match s.name().as_str() {
            "video/x-h264" => cfg
                .video
                .map(|dest| link_video_chain(&pipeline, pad, dest, stats.clone())),
            "audio/mpeg" => cfg
                .audio
                .map(|dest| link_audio_chain(&pipeline, pad, dest, stats.clone())),
            other => {
                warn!(
                    media = other,
                    "rtp sender: unsupported stream type, ignored"
                );
                None
            }
        };
        if let Some(Err(e)) = r {
            warn!(error = %e, "rtp sender: failed to link chain");
        }
    });
}

/// 视频链: H264 直接解包重打, 不解码不重编码
fn link_video_chain(
    pipeline: &gst::Pipeline,
    demux_pad: &gst::Pad,
    dest: RtpDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    let queue = make("queue")?;
    let parse = make("h264parse")?;
    parse.set_property("config-interval", 1i32);
    // mp4 里是 avcC, 强制转成 byte-stream(annexb)/au 再交给 payloader
    let caps = make("capsfilter")?;
    caps.set_property(
        "caps",
        gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build(),
    );
    let pay = make("rtph264pay")?;
    pay.set_property("pt", dest.payload_type as u32);
    install_pkt_probe(&pay, stats.clone(), false)?;
    let sink = make_udpsink(dest)?;
    add_link_and_plug(
        pipeline,
        demux_pad,
        &[&queue, &parse, &caps, &pay, &sink],
        "video",
    )
}

/// 音频链: AAC 解码后重编码成 G.711 A-law (PCMA, 8kHz 单声道)
fn link_audio_chain(
    pipeline: &gst::Pipeline,
    demux_pad: &gst::Pad,
    dest: RtpDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    let queue = make("queue")?;
    let parse = make("aacparse")?;
    let decode = make("avdec_aac")?;
    let convert = make("audioconvert")?;
    let resample = make("audioresample")?;
    // G.711 定死 8kHz; capsfilter 强制输出格式, 不依赖源文件采样率
    let caps = make("capsfilter")?;
    caps.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("rate", 8000i32)
            .field("channels", 1i32)
            .build(),
    );
    let enc = make("alawenc")?;
    let pay = make("rtppcmapay")?;
    pay.set_property("pt", dest.payload_type as u32);
    install_pkt_probe(&pay, stats.clone(), true)?;
    let sink = make_udpsink(dest)?;
    add_link_and_plug(
        pipeline,
        demux_pad,
        &[
            &queue, &parse, &decode, &convert, &resample, &caps, &enc, &pay, &sink,
        ],
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

/// 在 payloader 的 src pad 上数包: 每个 buffer = 一个 RTP 包
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

/// 公共尾巴: 元件入管线 → 依次互链 → demux 动态 pad 插到链头 →
/// 同步状态 (链条是在管线已 Playing 后才接上的, 必须手动 sync)
fn add_link_and_plug(
    pipeline: &gst::Pipeline,
    demux_pad: &gst::Pad,
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
    demux_pad
        .link(&head_sink)
        .map_err(|e| GstStreamError::Link(format!("plug demux pad to {what} chain: {e}")))?;
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

    /// 真实跑管线: 把 oceans.mp4 发到 loopback, 3 秒内两路都必须出包.
    /// 不发包 = pad 分流/链接有 bug, 不用等对端设备就能发现
    #[test]
    fn sender_actually_emits_packets() {
        // bus 线程的 EOS/Error 走 tracing, 不初始化 subscriber 就永远看不见
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let sender = start_rtp_sender(RtpSendConfig {
            file: PathBuf::from("../../assets/oceans.mp4"),
            audio: Some(RtpDest {
                addr: "127.0.0.1:40000".parse().unwrap(),
                payload_type: 8,
            }),
            video: Some(RtpDest {
                addr: "127.0.0.1:40002".parse().unwrap(),
                payload_type: 96,
            }),
        })
        .expect("sender starts");
        std::thread::sleep(Duration::from_secs(3));
        let (video, audio) = sender.packet_counts();
        sender.stop();
        assert!(video > 0, "no video RTP packets emitted");
        assert!(audio > 0, "no audio RTP packets emitted");
    }
}
