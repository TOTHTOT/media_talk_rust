//! RTP 接收器: 从 UDP 端口收 RTP 流, 两个出口可单开或同时开 (tee 分叉):
//!   存盘 (cfg.path): 音视频合进同一个 TS 流文件
//!   播放 (cfg.playback): 解码后送本机扬声器/屏幕, 全双工对讲的收端
//!
//! 管线拓扑 (单出口时没有 tee, 直链):
//!   视频: udpsrc(port) → rtph264depay → h264parse ─┬→ queue → mpegtsmux.sink_%d
//!         (存盘)                                    └→ queue → avdec_h264
//!                                                    → videoconvert → autovideosink (播放)
//!   音频: udpsrc(port) → rtppcmxdepay → mulawdec/alawdec ─┬→ audioconvert
//!           → audioresample → opusenc → queue → mpegtsmux.sink_%d        (存盘)
//!                                                        └→ queue → audioconvert
//!                                                          → audioresample → autoaudiosink (播放)
//!   mpegtsmux → filesink (.ts)
//!
//! 设计约束:
//!   - udpsrc 绑定 0.0.0.0 接受任意来源的 RTP 包 (对端可能有多个 IP)
//!   - 用 TS 不用 mp4: mp4mux 是 aggregator, 要等齐所有 pad 才输出, 一路
//!     媒体静默就整体卡死 (实测录像 0 字节 + 播放黑屏), 且 moov 索引只在
//!     EOS 写, 进程被杀整个文件作废. TS 流式写出, 崩了也能播. 要 mp4 的
//!     话事后 `ffmpeg -i in.ts -c copy out.mp4` 无损转封装
//!   - TS 不认 G.711/裸 PCM (mpegtsmux 音频只收 mpeg/AAC/AC3/opus 等),
//!     所以 G.711 存盘前解码转 opus
//!   - stop() 从每个 udpsrc 的 src pad 注入 EOS, 等 muxer 收尾 (bus 出现
//!     EOS/Error) 才落 Null. TS 没有索引, EOS 只是冲刷, 丢了也只是
//!     缺结尾而不是整个文件播不了

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, info, warn};

use crate::GstStreamError;
use crate::gstutil::{add_and_sync, disable_sink_async, leaky_queue, link_chain, make, static_pad};
use ipcam_core::{AudioCodec, VideoCodec};

use super::config::RtpRecvConfig;

/// RTP 接收器句柄.
#[derive(Debug, Clone)]
pub struct RtpReceiver {
    pipeline: gst::Pipeline,
    stop_flag: Arc<AtomicBool>,
    /// 各轨 udpsrc 的 src pad, stop() 时从这里注入 EOS
    src_pads: Arc<Vec<gst::Pad>>,
    stats: Arc<crate::PktStats>,
}

impl RtpReceiver {
    /// (视频包数, 音频包数) -- udpsrc src pad 上的 probe 实时累加
    pub fn packet_counts(&self) -> (u64, u64) {
        (
            self.stats.video_pkts.load(Ordering::Relaxed),
            self.stats.audio_pkts.load(Ordering::Relaxed),
        )
    }

    /// 停止接收: 先给下游发 EOS (muxer 冲刷收尾), 再拆管线.
    /// 可重复调用 (Drop 也会调).
    pub fn stop(&self) {
        if self.stop_flag.swap(true, Ordering::SeqCst) {
            return;
        }
        for pad in self.src_pads.iter() {
            let _ = pad.push_event(gst::event::Eos::new());
        }
        if let Some(bus) = self.pipeline.bus() {
            let _ = bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            );
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for RtpReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动 RTP 接收器.
pub fn start_rtp_receiver(cfg: RtpRecvConfig) -> Result<RtpReceiver, GstStreamError> {
    info!(?cfg, "recveiver start");
    cfg.validate()?;
    crate::ensure_init_internal()?;

    let pipeline = gst::Pipeline::new();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(crate::PktStats::default());

    // 存盘出口: mpegtsmux → filesink; 纯播放 (path=None) 不建.
    // 用 TS 不用 mp4: mp4mux 是 aggregator, 要等齐所有 pad 才输出,
    // 一路媒体静默就整体卡死 (实测录像 0 字节 + 播放黑屏), 而且 moov
    // 依赖 EOS, 进程被杀整个文件作废. TS 流式写出, 来什么录什么
    let mux = match &cfg.path {
        Some(path) => {
            let mux = make("mpegtsmux")?;
            let filesink = make("filesink")?;
            filesink.set_property("location", path.to_string_lossy().as_ref());
            // 不等 preroll: 播放分支的 sink 可能永远拿不到数据 (对端
            // 静默), async sink 会把整条管线拖在 PAUSED
            filesink.set_property("async", false);
            add_and_sync(&pipeline, &[&mux, &filesink])?;
            link_chain(&[&mux, &filesink], "mux to filesink")?;
            Some(mux)
        }
        None => None,
    };

    let mut src_pads = Vec::new();
    if let Some((port, codec)) = cfg.video {
        src_pads.push(build_video_track(
            &pipeline,
            mux.as_ref(),
            cfg.playback,
            port,
            codec,
            stats.clone(),
        )?);
    }
    if let Some((port, codec)) = cfg.audio {
        src_pads.push(build_audio_track(
            &pipeline,
            mux.as_ref(),
            cfg.playback,
            port,
            codec,
            stats.clone(),
        )?);
    }

    let bus = pipeline
        .bus()
        .ok_or_else(|| GstStreamError::Init("pipeline has no bus".into()))?;
    let stop_flag_for_bus = stop_flag.clone();
    std::thread::spawn(move || {
        for msg in bus.iter() {
            if stop_flag_for_bus.load(Ordering::SeqCst) {
                break;
            }
            match msg.view() {
                gst::MessageView::Eos(_) => {
                    info!("rtp receiver: EOS");
                    break;
                }
                gst::MessageView::Error(e) => {
                    warn!(error = %e.error(), debug = %e.debug().unwrap_or_default(), "rtp receiver error");
                    break;
                }
                _ => {}
            }
        }
        debug!("rtp receiver: bus thread exiting");
    });

    // 每 2s 报一次收包数: 视频约 fps 个包, 音频 ptime=20ms 即 50 包/s,
    // 看到数字涨就证明数据确实进来了, 问题在对端或网络
    let stats_for_tick = stats.clone();
    let stop_for_tick = stop_flag.clone();
    std::thread::spawn(move || {
        while !stop_for_tick.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(2));
            let vp = stats_for_tick.video_pkts.load(Ordering::Relaxed);
            let ap = stats_for_tick.audio_pkts.load(Ordering::Relaxed);
            info!("rtp receiver stats video_pkts={} audio_pkts={}", vp, ap);
        }
        debug!("rtp receiver stats task exiting");
    });

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| GstStreamError::Init(format!("pipeline set Playing: {e}")))?;

    Ok(RtpReceiver {
        pipeline,
        stop_flag,
        src_pads: Arc::new(src_pads),
        stats,
    })
}

/// tee 的 request pad 接到分支链头
fn link_tee_branch(tee: &gst::Element, branch_head: &gst::Element) -> Result<(), GstStreamError> {
    let src = tee
        .request_pad_simple("src_%u")
        .ok_or_else(|| GstStreamError::Link("tee request src_%u pad failed".into()))?;
    let sink = static_pad(branch_head, "sink")?;
    src.link(&sink)
        .map_err(|e| GstStreamError::Link(format!("failed to link tee to branch: {e}")))?;
    Ok(())
}

/// queue src → mpegtsmux 的 request pad. 构建期急切挂接 (不用懒挂):
/// mpegtsmux 是 collectpads 不是 aggregator, 死 pad 不会拖住输出
/// (实测音频静默下视频照常写 600KB+), 而且急切挂接保证 latency query
/// 能一路摸到 udpsrc, 管线才会被判为 live (live 管线不等 sink preroll,
/// 否则文件/播放 sink 全部卡 PAUSED, 数据流不动)
fn link_mux(
    queue: &gst::Element,
    mux: &gst::Element,
    template: &str,
) -> Result<(), GstStreamError> {
    let mux_pad = mux
        .request_pad_simple(template)
        .ok_or_else(|| GstStreamError::Link(format!("mpegtsmux request {template} pad failed")))?;
    static_pad(queue, "src")?
        .link(&mux_pad)
        .map_err(|e| GstStreamError::Link(format!("failed to link queue to mpegtsmux: {e}")))?;
    Ok(())
}

/// 视频接收轨: udpsrc → depay → parse, 之后按出口分: 存盘 (queue → mux)
/// / 播放 (decode → videoconvert → autovideosink) / tee 双全.
/// 返回 udpsrc 的 src pad (stop 时注入 EOS 用)
fn build_video_track(
    pipeline: &gst::Pipeline,
    mux: Option<&gst::Element>,
    playback: bool,
    port: u16,
    codec: VideoCodec,
    stats: Arc<crate::PktStats>,
) -> Result<gst::Pad, GstStreamError> {
    let (depay_name, parse_name, decode_name, encoding_name, media_type) = match codec {
        VideoCodec::H264 => (
            "rtph264depay",
            "h264parse",
            "avdec_h264",
            "H264",
            "video/x-h264",
        ),
        VideoCodec::H265 => (
            "rtph265depay",
            "h265parse",
            "avdec_h265",
            "H265",
            "video/x-h265",
        ),
        other => {
            return Err(GstStreamError::InvalidConfig(format!(
                "unsupported video codec for receive: {other:?}"
            )));
        }
    };

    let udpsrc = make("udpsrc")?;
    // udpsrc 绑定 caps, 让后续 depay 能正常工作
    let caps = gst::Caps::builder_full()
        .structure(
            gst::Structure::builder("application/x-rtp")
                .field("media", "video")
                .field("payload", 96i32)
                .field("encoding-name", encoding_name)
                .build(),
        )
        .build();
    udpsrc.set_property("caps", &caps);
    udpsrc.set_property("port", port as i32);

    let depay = make(depay_name)?;
    let parse = make(parse_name)?;
    // 强制 byte-stream: tee 分叉后 parse 可能选 avc (avdec 优先),
    // 而 mpegtsmux 只收 byte-stream, 协商失败存盘链静默断流 (实测 0 字节)
    let bs = make("capsfilter")?;
    bs.set_property(
        "caps",
        gst::Caps::builder(media_type)
            .field("stream-format", "byte-stream")
            .build(),
    );

    // 在 udpsrc src pad 上装 probe 统计收包数. 不能装在 depay 后面:
    // depay 会把多个连续 RTP 包合并成一个大 buffer 输出, 计数会缩水
    let stats_for_probe = stats.clone();
    if let Some(src_pad) = udpsrc.static_pad("src") {
        src_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            stats_for_probe.video_pkts.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }

    match (mux, playback) {
        (Some(mux), false) => {
            // mux 前挂 queue 解耦: mux 的消费节奏不能卡住网络收包线程
            let queue = make("queue")?;
            let elems = [&udpsrc, &depay, &parse, &bs, &queue];
            add_and_sync(pipeline, &elems)?;
            link_chain(&elems, "video recv chain")?;
            link_mux(&queue, mux, "sink_%d")?;
        }
        (None, true) => {
            let decode = make(decode_name)?;
            let convert = make("videoconvert")?;
            let sink = make("autovideosink")?;
            disable_sink_async(&sink);
            let elems = [&udpsrc, &depay, &parse, &bs, &decode, &convert, &sink];
            add_and_sync(pipeline, &elems)?;
            link_chain(&elems, "video play chain")?;
        }
        (Some(mux), true) => {
            let tee = make("tee")?;
            // tee 分支必须用 leaky queue: 任一分支停滞 (比如 mux 或
            // 播放 sink 卡住) 时普通 queue 会填满反压 tee, 把另一条分支
            // 和整条管线一起拖死 (实测: 音频缺失时视频冻结在 ~27 帧)
            let rec_queue = leaky_queue()?;
            let play_queue = leaky_queue()?;
            let decode = make(decode_name)?;
            let convert = make("videoconvert")?;
            let sink = make("autovideosink")?;
            disable_sink_async(&sink);
            let elems = [
                &udpsrc,
                &depay,
                &parse,
                &bs,
                &tee,
                &rec_queue,
                &play_queue,
                &decode,
                &convert,
                &sink,
            ];
            add_and_sync(pipeline, &elems)?;
            link_chain(&[&udpsrc, &depay, &parse, &bs, &tee], "video recv head")?;
            link_chain(
                &[&play_queue, &decode, &convert, &sink],
                "video play branch",
            )?;
            link_tee_branch(&tee, &rec_queue)?;
            link_tee_branch(&tee, &play_queue)?;
            link_mux(&rec_queue, mux, "sink_%d")?;
        }
        (None, false) => {
            return Err(GstStreamError::InvalidConfig(
                "video track with neither record nor playback".into(),
            ));
        }
    }

    info!(port, codec = ?codec, record = mux.is_some(), playback, "video receive track linked");
    static_pad(&udpsrc, "src")
}

/// 音频接收轨: udpsrc → depay → G.711 解码, 之后按出口分: 存盘 (convert
/// → resample → opusenc → queue → mux, TS 不认 G.711 转 opus) / 播放
/// (convert → resample → autoaudiosink) / tee 双全.
/// 返回 udpsrc 的 src pad (stop 时注入 EOS 用)
fn build_audio_track(
    pipeline: &gst::Pipeline,
    mux: Option<&gst::Element>,
    playback: bool,
    port: u16,
    codec: AudioCodec,
    stats: Arc<crate::PktStats>,
) -> Result<gst::Pad, GstStreamError> {
    let (depay_name, decode_name) = match codec {
        AudioCodec::G711A => ("rtppcmadepay", "alawdec"),
        AudioCodec::G711U => ("rtppcmudepay", "mulawdec"),
        other => {
            return Err(GstStreamError::InvalidConfig(format!(
                "unsupported audio codec for receive: {other:?}"
            )));
        }
    };

    let udpsrc = make("udpsrc")?;
    // 静态 pt (PCMU=0/PCMA=8) 和编码名取自 AudioCodec 的协议映射
    let caps = gst::Caps::builder_full()
        .structure(
            gst::Structure::builder("application/x-rtp")
                .field("media", "audio")
                .field("payload", codec.static_pt().unwrap_or(0) as i32)
                .field("clock-rate", 8000i32)
                .field("encoding-name", codec.rtpmap_name())
                .build(),
        )
        .build();
    udpsrc.set_property("caps", &caps);
    udpsrc.set_property("port", port as i32);

    let depay = make(depay_name)?;
    let decode = make(decode_name)?;

    // 同视频路: probe 装在 udpsrc src pad, 避开 depay 的合包
    let stats_for_probe = stats.clone();
    if let Some(src_pad) = udpsrc.static_pad("src") {
        src_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            stats_for_probe.audio_pkts.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }

    match (mux, playback) {
        (Some(mux), false) => {
            let convert = make("audioconvert")?;
            let resample = make("audioresample")?;
            let enc = make("opusenc")?;
            // 同视频路: mux 前必须有 queue 解耦
            let queue = make("queue")?;
            let elems = [&udpsrc, &depay, &decode, &convert, &resample, &enc, &queue];
            add_and_sync(pipeline, &elems)?;
            link_chain(&elems, "audio recv chain")?;
            link_mux(&queue, mux, "sink_%d")?;
        }
        (None, true) => {
            let convert = make("audioconvert")?;
            let resample = make("audioresample")?;
            let sink = make("autoaudiosink")?;
            disable_sink_async(&sink);
            let elems = [&udpsrc, &depay, &decode, &convert, &resample, &sink];
            add_and_sync(pipeline, &elems)?;
            link_chain(&elems, "audio play chain")?;
        }
        (Some(mux), true) => {
            let tee = make("tee")?;
            // 同视频路: tee 分支必须 leaky queue, 一路停滞不拖垮另一路
            let play_queue = leaky_queue()?;
            let play_convert = make("audioconvert")?;
            let play_resample = make("audioresample")?;
            let sink = make("autoaudiosink")?;
            disable_sink_async(&sink);
            let rec_convert = make("audioconvert")?;
            let rec_resample = make("audioresample")?;
            let enc = make("opusenc")?;
            let rec_queue = leaky_queue()?;
            let elems = [
                &udpsrc,
                &depay,
                &decode,
                &tee,
                &play_queue,
                &play_convert,
                &play_resample,
                &sink,
                &rec_convert,
                &rec_resample,
                &enc,
                &rec_queue,
            ];
            add_and_sync(pipeline, &elems)?;
            link_chain(&[&udpsrc, &depay, &decode, &tee], "audio recv head")?;
            link_chain(
                &[&play_queue, &play_convert, &play_resample, &sink],
                "audio play branch",
            )?;
            link_chain(
                &[&rec_convert, &rec_resample, &enc, &rec_queue],
                "audio record branch",
            )?;
            link_tee_branch(&tee, &play_queue)?;
            link_tee_branch(&tee, &rec_convert)?;
            link_mux(&rec_queue, mux, "sink_%d")?;
        }
        (None, false) => {
            return Err(GstStreamError::InvalidConfig(
                "audio track with neither record nor playback".into(),
            ));
        }
    }

    info!(port, codec = ?codec, record = mux.is_some(), playback, "audio receive track linked");
    static_pad(&udpsrc, "src")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rtp_recv_{name}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 端到端: 真发 RTP (本机环回) → 接收器合并成 TS → stop() 注入 EOS
    /// 收尾 (TS 无索引, EOS 只是冲刷; 文件非空且能被 discoverer 认出即可).
    /// gst-launch 测不了这条路径: 它对 udpsrc 注入不了 EOS, 只有
    /// stop() 的 push_event 能走到
    #[test]
    fn receiver_merges_av_into_playable_ts() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        crate::ensure_init_internal().unwrap();

        let dir = test_dir("record");
        let path = dir.join("out.ts");

        let receiver = start_rtp_receiver(RtpRecvConfig {
            path: Some(path.clone()),
            playback: false,
            video: Some((42110, VideoCodec::H264)),
            audio: Some((42112, AudioCodec::G711U)),
        })
        .expect("receiver starts");

        // 本机环回发包: 模拟 SIP 对端
        let sender = gst::parse::launch(
            "audiotestsrc is-live=true ! mulawenc ! rtppcmupay ! udpsink host=127.0.0.1 port=42112 \
             videotestsrc is-live=true ! video/x-raw,framerate=15/1 ! x264enc tune=zerolatency \
             ! rtph264pay ! udpsink host=127.0.0.1 port=42110",
        )
        .expect("sender pipeline parses")
        .downcast::<gst::Pipeline>()
        .expect("sender is a pipeline");
        sender.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_secs(3));

        receiver.stop();
        sender.set_state(gst::State::Null).ok();

        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(bytes > 1000, "ts empty ({bytes} bytes)");
    }

    /// 播放 + 存盘同时开 (tee 分叉): 播放链 (autoaudiosink) 不能拖垮
    /// 存盘链, TS 照常落盘
    #[test]
    fn audio_tee_playback_and_record() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        crate::ensure_init_internal().unwrap();

        let dir = test_dir("tee");
        let path = dir.join("out.ts");

        let receiver = start_rtp_receiver(RtpRecvConfig {
            path: Some(path.clone()),
            playback: true,
            video: None,
            audio: Some((42114, AudioCodec::G711U)),
        })
        .expect("receiver starts");

        let sender = gst::parse::launch(
            "audiotestsrc is-live=true ! mulawenc ! rtppcmupay ! udpsink host=127.0.0.1 port=42114",
        )
        .expect("sender pipeline parses")
        .downcast::<gst::Pipeline>()
        .expect("sender is a pipeline");
        sender.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_secs(3));

        receiver.stop();
        sender.set_state(gst::State::Null).ok();

        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(bytes > 1000, "ts empty ({bytes} bytes)");
    }

    /// 回归: 一路媒体缺失时另一路不能冻结. 配了音视频双路但只发音频,
    /// mux/sink 任一分支停滞时 tee 必须不被拖死 (leaky queue), 音频包数要持续
    /// 增长 (修复前: 普通 queue 1s≈50 包填满 → tee 反压 → 全线冻结)
    #[test]
    fn missing_track_does_not_freeze_other() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        crate::ensure_init_internal().unwrap();

        let dir = test_dir("freeze");
        let path = dir.join("out.ts");

        let receiver = start_rtp_receiver(RtpRecvConfig {
            path: Some(path.clone()),
            playback: true,
            video: Some((42120, VideoCodec::H264)),
            audio: Some((42122, AudioCodec::G711U)),
        })
        .expect("receiver starts");

        // 只发音频, 视频路永远静默 (复现门口机只出音频的通话场景).
        // samplesperbuffer=160 = 20ms @ 8kHz, 让 rtppcmupay 按 50 pkt/s 发包
        let sender = gst::parse::launch(
            "audiotestsrc is-live=true samplesperbuffer=160 ! mulawenc ! rtppcmupay ! udpsink host=127.0.0.1 port=42122",
        )
        .expect("sender pipeline parses")
        .downcast::<gst::Pipeline>()
        .expect("sender is a pipeline");
        sender.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_secs(3));

        let (_video, audio) = receiver.packet_counts();
        receiver.stop();
        sender.set_state(gst::State::Null).ok();

        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_dir_all(&dir);

        // ptime=20ms → ~50 包/s, 3s 应 ~150; 冻结时卡死在 ~50
        assert!(audio > 100, "audio track froze at {audio} packets");
        // 视频静默不影响纯音频 TS 落盘 (mpegtsmux 不等死 pad)
        assert!(bytes > 1000, "audio-only ts not written ({bytes} bytes)");
    }

    /// 回归 (实机故障): 音频全是垃圾包/完全静默时, 视频必须照常录像.
    /// 故障链: mp4mux 等齐所有 pad 才输出, 音频 pad 无数据 → 录像 0 字节
    /// + 管线卡死黑屏. 换 mpegtsmux (流式, 不等死 pad) 后不再有这个约束
    #[test]
    fn video_record_survives_dead_audio() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        crate::ensure_init_internal().unwrap();

        let dir = test_dir("dead_audio");
        let path = dir.join("out.ts");

        let receiver = start_rtp_receiver(RtpRecvConfig {
            path: Some(path.clone()),
            playback: true,
            video: Some((42124, VideoCodec::H264)),
            audio: Some((42126, AudioCodec::G711U)),
        })
        .expect("receiver starts");

        // 只发视频, 音频路一个包都不发 (比垃圾包更极端)
        let sender = gst::parse::launch(
            "videotestsrc is-live=true ! video/x-raw,framerate=15/1 ! x264enc tune=zerolatency \
             ! rtph264pay ! udpsink host=127.0.0.1 port=42124",
        )
        .expect("sender pipeline parses")
        .downcast::<gst::Pipeline>()
        .expect("sender is a pipeline");
        sender.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_secs(3));

        let (video, _audio) = receiver.packet_counts();
        receiver.stop();
        sender.set_state(gst::State::Null).ok();

        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(video > 30, "video track not receiving ({video} packets)");
        assert!(bytes > 1000, "video-only ts not written ({bytes} bytes)");
    }
}
