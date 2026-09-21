//! RTP 接收器: 从 UDP 端口收 RTP 流, 音视频合进同一个 mp4.
//!
//! 管线拓扑 (两路各挂 mp4mux 的一个 request pad):
//!   视频: udpsrc(port) → rtph264depay → h264parse → queue → mp4mux.video_%u
//!   音频: udpsrc(port) → rtppcmxdepay → mulawdec/alawdec
//!         → audioconvert → audioresample → opusenc → queue → mp4mux.audio_%u
//!   mp4mux → filesink (.mp4)
//!
//! 设计约束:
//!   - udpsrc 绑定 0.0.0.0 接受任意来源的 RTP 包 (对端可能有多个 IP)
//!   - mp4mux 不认 G.711/裸 PCM (实测 gst-inspect 的 audio_%u caps:
//!     只有 mpeg/AAC/AC3/EAC3/ALAC/opus), 所以 G.711 解码后转 opus
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
    /// 停止接收: 先给 muxer 发 EOS 让它写完 moov, 再拆管线.
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

    let mux = make("mp4mux")?;
    let filesink = make("filesink")?;
    filesink.set_property("location", cfg.path.to_string_lossy().as_ref());
    add_and_sync(&pipeline, &[&mux, &filesink])?;
    link_chain(&[&mux, &filesink], "mux to filesink")?;

    let mut src_pads = Vec::new();
    if let Some((port, codec)) = cfg.video {
        src_pads.push(build_video_track(&pipeline, &mux, port, codec)?);
    }
    if let Some((port, codec)) = cfg.audio {
        src_pads.push(build_audio_track(&pipeline, &mux, port, codec)?);
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
        path = %cfg.path.display(),
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

/// 视频接收链: udpsrc → depay → parse → mp4mux.video_%u.
/// 返回 udpsrc 的 src pad (stop 时注入 EOS 用)
fn build_video_track(
    pipeline: &gst::Pipeline,
    mux: &gst::Element,
    port: u16,
    codec: VideoCodec,
) -> Result<gst::Pad, GstStreamError> {
    let (depay_name, parse_name, encoding_name) = match codec {
        VideoCodec::H264 => ("rtph264depay", "h264parse", "H264"),
        VideoCodec::H265 => ("rtph265depay", "h265parse", "H265"),
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
    // mp4mux 是 aggregator, 直挂会报 "Impossible to configure latency",
    // mux 前必须有 queue 缓冲
    let queue = make("queue")?;

    let elems: Vec<&gst::Element> = vec![&udpsrc, &depay, &parse, &queue];
    add_and_sync(pipeline, &elems)?;
    link_chain(&elems, "video recv chain")?;

    let mux_pad = mux
        .request_pad_simple("video_%u")
        .ok_or_else(|| GstStreamError::Link("mp4mux request video pad failed".into()))?;
    static_pad(&queue, "src")?
        .link(&mux_pad)
        .map_err(|e| GstStreamError::Link(format!("failed to link h264parse to mp4mux: {e}")))?;

    info!(port, codec = ?codec, "video receive track linked");
    static_pad(&udpsrc, "src")
}

/// 音频接收链: udpsrc → depay → G.711 解码 → convert → resample →
/// opusenc → mp4mux.audio_%u (mp4 不认 G.711, 转 opus).
/// 返回 udpsrc 的 src pad (stop 时注入 EOS 用)
fn build_audio_track(
    pipeline: &gst::Pipeline,
    mux: &gst::Element,
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
    let convert = make("audioconvert")?;
    let resample = make("audioresample")?;
    let enc = make("opusenc")?;
    // 同视频路: mux 前必须有 queue (aggregator latency)
    let queue = make("queue")?;

    let elems: Vec<&gst::Element> =
        vec![&udpsrc, &depay, &decode, &convert, &resample, &enc, &queue];
    add_and_sync(pipeline, &elems)?;
    link_chain(&elems, "audio recv chain")?;

    let mux_pad = mux
        .request_pad_simple("audio_%u")
        .ok_or_else(|| GstStreamError::Link("mp4mux request audio pad failed".into()))?;
    static_pad(&queue, "src")?
        .link(&mux_pad)
        .map_err(|e| GstStreamError::Link(format!("failed to link opusenc to mp4mux: {e}")))?;

    info!(port, codec = ?codec, "audio receive track linked (transcode to opus)");
    static_pad(&udpsrc, "src")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 端到端: 真发 RTP (本机环回) → 接收器合并成 mp4 → stop() 注入 EOS
    /// 后文件必须有 moov 索引 (非空且能被 discoverer 认出).
    /// gst-launch 测不了这条路径: 它对 udpsrc 注入不了 EOS, 只有
    /// stop() 的 push_event 能走到
    #[test]
    fn receiver_merges_av_into_playable_mp4() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        crate::ensure_init_internal().unwrap();

        let dir = std::env::temp_dir().join(format!("rtp_recv_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.mp4");

        let receiver = start_rtp_receiver(RtpRecvConfig {
            path: path.clone(),
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
}
