//! RTP 接收器: 从 UDP 端口接收 RTP 流并保存到文件.
//!
//! 管线拓扑:
//!   视频: udpsrc(port) → rtph264depay → h264parse → mpegtsmux → filesink (.ts)
//!   音频: udpsrc(port) → rtppcmxdepay → wavenc → filesink (.wav)
//!
//! 设计约束:
//!   - udpsrc 绑定 0.0.0.0 接受任意来源的 RTP 包 (因为对端可能有多个 IP)
//!   - filesink 以 append 模式打开, 支持断点续写
//!   - 不做解码, 直接保存编码后的 H.264 / G.711 数据
//!   - 容器选择: H264 进 mpegtsmux 得到可直接播的 ts; G.711 不在
//!     MPEG-TS 的合法载荷里 (mux 会 link 失败), 走 wavenc 存成 wav

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, info, warn};

use crate::GstStreamError;
use crate::gstutil::{add_and_sync, link_chain, make};
use ipcam_core::{AudioCodec, VideoCodec};

use super::config::RtpRecvConfig;

/// RTP 接收器句柄.
#[derive(Debug, Clone)]
pub struct RtpReceiver {
    pipeline: gst::Pipeline,
    stop_flag: Arc<AtomicBool>,
}

impl RtpReceiver {
    /// 停止接收.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
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

    if let Some(path) = &cfg.video_path {
        build_video_track(&pipeline, path, cfg.video_port, cfg.video_codec)?;
    }
    if let Some(path) = &cfg.audio_path {
        build_audio_track(&pipeline, path, cfg.audio_port, cfg.audio_codec)?;
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
        video = ?cfg.video_path.as_ref().map(|p| p.display()),
        video_port = cfg.video_port,
        audio = ?cfg.audio_path.as_ref().map(|p| p.display()),
        audio_port = cfg.audio_port,
        "rtp receiver started",
    );

    Ok(RtpReceiver {
        pipeline,
        stop_flag,
    })
}

/// 构建视频接收链路: udpsrc → depay → parse → filesink
fn build_video_track(
    pipeline: &gst::Pipeline,
    path: &Path,
    port: u16,
    codec: VideoCodec,
) -> Result<(), GstStreamError> {
    // 分别输出原始nal和根据nal组成au
    let (depay_name, parse_name) = match codec {
        VideoCodec::H264 => ("rtph264depay", "h264parse"),
        VideoCodec::H265 => ("rtph265depay", "h265parse"),
        other => {
            return Err(GstStreamError::InvalidConfig(format!(
                "unsupported video codec for receive: {other:?}"
            )));
        }
    };

    let udpsrc = make("udpsrc")?;
    // udpsrc绑定caps, 让后续 depay 能正常工作
    let caps = match codec {
        VideoCodec::H264 => gst::Caps::builder_full()
            .structure(
                gst::Structure::builder("application/x-rtp")
                    .field("media", "video")
                    .field("payload", 96i32)
                    .field("encoding-name", "H264")
                    .build(),
            )
            .build(),
        VideoCodec::H265 => gst::Caps::builder_full()
            .structure(
                gst::Structure::builder("application/x-rtp")
                    .field("media", "video")
                    .field("payload", 96i32)
                    .field("encoding-name", "H265")
                    .build(),
            )
            .build(),
        _ => unreachable!(),
    };
    udpsrc.set_property("caps", &caps);
    udpsrc.set_property("port", port as i32);
    // 设置 capsfilter 确保 caps 精确匹配
    let capsfilter = make("capsfilter")?;
    capsfilter.set_property("caps", &caps);
    let depay = make(depay_name)?;
    let parse = make(parse_name)?;

    let tsmux = make("mpegtsmux")?;
    let filesink = make("filesink")?;
    filesink.set_property("append", true);

    // append=true 断点续写
    filesink.set_property("location", path.to_string_lossy().as_ref());
    filesink.set_property("append", true);

    let elems: Vec<&gst::Element> = vec![&udpsrc, &capsfilter, &depay, &parse, &tsmux, &filesink];
    add_and_sync(pipeline, &elems)?;
    link_chain(&elems, "video recv chain")?;

    info!(port, codec = ?codec, path = %path.display(), "video receive track linked");
    Ok(())
}

/// 构建音频接收链路: udpsrc → depay → wavenc → filesink.
/// 注意 G.711 不能进 mpegtsmux (TS 不含 G.711 载荷, link 直接失败),
/// wavenc 原生接受 audio/x-alaw|x-mulaw, 存出来就是可播的 wav
fn build_audio_track(
    pipeline: &gst::Pipeline,
    path: &Path,
    port: u16,
    codec: AudioCodec,
) -> Result<(), GstStreamError> {
    let depay_name = match codec {
        AudioCodec::G711A => "rtppcmadepay",
        AudioCodec::G711U => "rtppcmudepay",
        other => {
            return Err(GstStreamError::InvalidConfig(format!(
                "unsupported audio codec for receive: {other:?}"
            )));
        }
    };

    let udpsrc = make("udpsrc")?;
    // 静态 pt (PCMU=0/PCMA=8) 和编码名都取自 AudioCodec 的协议映射
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
    let wav = make("wavenc")?;
    let filesink = make("filesink")?;
    filesink.set_property("location", path.to_string_lossy().as_ref());
    filesink.set_property("append", true);

    let elems: Vec<&gst::Element> = vec![&udpsrc, &depay, &wav, &filesink];
    add_and_sync(pipeline, &elems)?;
    link_chain(&elems, "audio recv chain")?;

    info!(port, codec = ?codec, path = %path.display(), "audio receive track linked");
    Ok(())
}
