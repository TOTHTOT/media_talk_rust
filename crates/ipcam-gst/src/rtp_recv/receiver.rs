//! RTP 接收器: 从 UDP 端口收 RTP 流, 两个出口可单开或同时开 (tee 分叉):
//!   存盘 (cfg.path): 音视频合进同一个 mp4
//!   播放 (cfg.playback): 解码后送本机扬声器/屏幕, 全双工对讲的收端
//!
//! 管线拓扑 (单出口时没有 tee, 直链):
//!   视频: udpsrc(port) → rtph264depay → h264parse ─┬→ queue → mp4mux.video_%u
//!         (存盘)                                    └→ queue → avdec_h264
//!                                                    → videoconvert → autovideosink (播放)
//!   音频: udpsrc(port) → rtppcmxdepay → mulawdec/alawdec ─┬→ audioconvert
//!           → audioresample → opusenc → queue → mp4mux.audio_%u          (存盘)
//!                                                        └→ queue → audioconvert
//!                                                          → audioresample → autoaudiosink (播放)
//!   mp4mux → filesink (.mp4)
//!
//! 设计约束:
//!   - udpsrc 绑定 0.0.0.0 接受任意来源的 RTP 包 (对端可能有多个 IP)
//!   - mp4mux 不认 G.711/裸 PCM (实测 gst-inspect 的 audio_%u caps:
//!     只有 mpeg/AAC/AC3/EAC3/ALAC/opus), 所以 G.711 存盘前解码转 opus
//!   - mp4mux 是 aggregator, mux 前必须挂 queue, 否则 latency 协商失败
//!   - mp4 的 moov 索引只在 EOS 时写入: stop() 从每个 udpsrc 的 src
//!     pad 注入 EOS, 等 muxer 收尾 (bus 出现 EOS/Error) 才落 Null,
//!     否则文件没有索引播不了

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, info, warn};

use crate::GstStreamError;
use crate::gstutil::{add_and_sync, link_chain, make, static_pad};
use ipcam_core::{AudioCodec, VideoCodec};

use super::config::RtpRecvConfig;

/// RTP 接收器句柄.
#[derive(Debug, Clone)]
pub struct RtpReceiver {
    pipeline: gst::Pipeline,
    stop_flag: Arc<AtomicBool>,
    /// 各轨 udpsrc 的 src pad, stop() 时从这里注入 EOS
    src_pads: Arc<Vec<gst::Pad>>,
}

impl RtpReceiver {
    /// 停止接收: 先给下游发 EOS (muxer 写完 moov), 再拆管线.
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
    cfg.validate()?;
    crate::ensure_init_internal()?;

    let pipeline = gst::Pipeline::new();
    let stop_flag = Arc::new(AtomicBool::new(false));

    // 存盘出口: mp4mux → filesink; 纯播放 (path=None) 不建
    let mux = match &cfg.path {
        Some(path) => {
            let mux = make("mp4mux")?;
            let filesink = make("filesink")?;
            filesink.set_property("location", path.to_string_lossy().as_ref());
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
        )?);
    }
    if let Some((port, codec)) = cfg.audio {
        src_pads.push(build_audio_track(
            &pipeline,
            mux.as_ref(),
            cfg.playback,
            port,
            codec,
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

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| GstStreamError::Init(format!("pipeline set Playing: {e}")))?;

    info!(
        path = ?cfg.path,
        playback = cfg.playback,
        video = ?cfg.video,
        audio = ?cfg.audio,
        "rtp receiver started",
    );

    Ok(RtpReceiver {
        pipeline,
        stop_flag,
        src_pads: Arc::new(src_pads),
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

/// queue src → mp4mux 的 request pad
fn link_mux(
    queue: &gst::Element,
    mux: &gst::Element,
    template: &str,
) -> Result<(), GstStreamError> {
    let mux_pad = mux
        .request_pad_simple(template)
        .ok_or_else(|| GstStreamError::Link(format!("mp4mux request {template} pad failed")))?;
    static_pad(queue, "src")?
        .link(&mux_pad)
        .map_err(|e| GstStreamError::Link(format!("failed to link queue to mp4mux: {e}")))?;
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
) -> Result<gst::Pad, GstStreamError> {
    let (depay_name, parse_name, decode_name, encoding_name) = match codec {
        VideoCodec::H264 => ("rtph264depay", "h264parse", "avdec_h264", "H264"),
        VideoCodec::H265 => ("rtph265depay", "h265parse", "avdec_h265", "H265"),
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

    match (mux, playback) {
        (Some(mux), false) => {
            // mp4mux 是 aggregator, 直挂会报 "Impossible to configure latency",
            // mux 前必须有 queue 缓冲
            let queue = make("queue")?;
            let elems = [&udpsrc, &depay, &parse, &queue];
            add_and_sync(pipeline, &elems)?;
            link_chain(&elems, "video recv chain")?;
            link_mux(&queue, mux, "video_%u")?;
        }
        (None, true) => {
            let decode = make(decode_name)?;
            let convert = make("videoconvert")?;
            let sink = make("autovideosink")?;
            let elems = [&udpsrc, &depay, &parse, &decode, &convert, &sink];
            add_and_sync(pipeline, &elems)?;
            link_chain(&elems, "video play chain")?;
        }
        (Some(mux), true) => {
            let tee = make("tee")?;
            // tee 分支各自独立调度, 每条分支自己的 queue
            let rec_queue = make("queue")?;
            let play_queue = make("queue")?;
            let decode = make(decode_name)?;
            let convert = make("videoconvert")?;
            let sink = make("autovideosink")?;
            let elems = [
                &udpsrc,
                &depay,
                &parse,
                &tee,
                &rec_queue,
                &play_queue,
                &decode,
                &convert,
                &sink,
            ];
            add_and_sync(pipeline, &elems)?;
            link_chain(&[&udpsrc, &depay, &parse, &tee], "video recv head")?;
            link_chain(
                &[&play_queue, &decode, &convert, &sink],
                "video play branch",
            )?;
            link_tee_branch(&tee, &rec_queue)?;
            link_tee_branch(&tee, &play_queue)?;
            link_mux(&rec_queue, mux, "video_%u")?;
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
/// → resample → opusenc → queue → mux, mp4 不认 G.711 转 opus) / 播放
/// (convert → resample → autoaudiosink) / tee 双全.
/// 返回 udpsrc 的 src pad (stop 时注入 EOS 用)
fn build_audio_track(
    pipeline: &gst::Pipeline,
    mux: Option<&gst::Element>,
    playback: bool,
    port: u16,
    codec: AudioCodec,
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

    match (mux, playback) {
        (Some(mux), false) => {
            let convert = make("audioconvert")?;
            let resample = make("audioresample")?;
            let enc = make("opusenc")?;
            // 同视频路: mux 前必须有 queue (aggregator latency)
            let queue = make("queue")?;
            let elems = [&udpsrc, &depay, &decode, &convert, &resample, &enc, &queue];
            add_and_sync(pipeline, &elems)?;
            link_chain(&elems, "audio recv chain")?;
            link_mux(&queue, mux, "audio_%u")?;
        }
        (None, true) => {
            let convert = make("audioconvert")?;
            let resample = make("audioresample")?;
            let sink = make("autoaudiosink")?;
            let elems = [&udpsrc, &depay, &decode, &convert, &resample, &sink];
            add_and_sync(pipeline, &elems)?;
            link_chain(&elems, "audio play chain")?;
        }
        (Some(mux), true) => {
            let tee = make("tee")?;
            let play_queue = make("queue")?;
            let play_convert = make("audioconvert")?;
            let play_resample = make("audioresample")?;
            let sink = make("autoaudiosink")?;
            let rec_convert = make("audioconvert")?;
            let rec_resample = make("audioresample")?;
            let enc = make("opusenc")?;
            let rec_queue = make("queue")?;
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
            link_mux(&rec_queue, mux, "audio_%u")?;
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

    /// 端到端: 真发 RTP (本机环回) → 接收器合并成 mp4 → stop() 注入 EOS
    /// 后文件必须有 moov 索引 (非空且能被 discoverer 认出).
    /// gst-launch 测不了这条路径: 它对 udpsrc 注入不了 EOS, 只有
    /// stop() 的 push_event 能走到
    #[test]
    fn receiver_merges_av_into_playable_mp4() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        crate::ensure_init_internal().unwrap();

        let dir = test_dir("record");
        let path = dir.join("out.mp4");

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
        assert!(bytes > 1000, "mp4 not finalized or empty ({bytes} bytes)");
    }

    /// 播放 + 存盘同时开 (tee 分叉): 播放链 (autoaudiosink) 不能拖垮
    /// 存盘链, mp4 照常落盘
    #[test]
    fn audio_tee_playback_and_record() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        crate::ensure_init_internal().unwrap();

        let dir = test_dir("tee");
        let path = dir.join("out.mp4");

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
        assert!(bytes > 1000, "mp4 not finalized or empty ({bytes} bytes)");
    }
}
