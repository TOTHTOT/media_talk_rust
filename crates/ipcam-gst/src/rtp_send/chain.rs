//! 发送链: 裸流 pad → 重编码 → payloader → udpsink. 所有源共用,
//! 由根模块的 track 组装在源 pad 出现时回调进来.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::info;

use super::{AudioDest, RtpDest, SendStats};
use crate::GstStreamError;
use crate::gstutil::make;
use ipcam_core::AudioCodec;

/// 音频编码对应的 (encoder, payloader) 元件名; None = 不支持发
fn audio_encode_elements(codec: AudioCodec) -> Option<(&'static str, &'static str)> {
    match codec {
        AudioCodec::G711A => Some(("alawenc", "rtppcmapay")),
        AudioCodec::G711U => Some(("mulawenc", "rtppcmupay")),
        _ => None,
    }
}

/// 视频发送链 (所有源共用): 裸流重编码, 参数集周期重发.
/// dest.max_width 有值时中间插 videoscale + capsfilter 限宽
pub(super) fn link_video_send_chain(
    pipeline: &gst::Pipeline,
    raw_pad: &gst::Pad,
    dest: RtpDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    // convert 编码格式转换
    let mut chain = vec![make("queue")?, make("videoconvert")?];
    if let Some(max_w) = dest.max_width {
        // 限宽: 1080p 相机直发超出门口机解码上限会黑屏.
        // caps 用范围而不是定值: videoscale 只在源超宽时降采样, 且保持宽高比
        let scale = make("videoscale")?;
        let caps = make("capsfilter")?;
        caps.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", gst::IntRange::new(1i32, max_w as i32))
                .build(),
        );
        chain.push(scale);
        chain.push(caps);
    }
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
    chain.extend([enc, pay, make_udpsink(dest.addr)?]);
    let refs: Vec<&gst::Element> = chain.iter().collect();
    add_link_and_plug(pipeline, raw_pad, &refs, "video")
}

/// 音频发送链 (所有源共用): 裸流重采样成 answer 协商出的 G.711 (8kHz 单声道)
pub(super) fn link_audio_send_chain(
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
    let Some((enc_name, pay_name)) = audio_encode_elements(dest.codec) else {
        return Err(GstStreamError::InvalidConfig(format!(
            "unsupported audio codec for send: {:?}",
            dest.codec
        )));
    };
    let enc = make(enc_name)?;
    let pay = make(pay_name)?;
    pay.set_property("pt", dest.payload_type as u32);
    install_pkt_probe(&pay, stats.clone(), true)?;
    let sink = make_udpsink(dest.addr)?;
    add_link_and_plug(
        pipeline,
        raw_pad,
        &[&queue, &convert, &resample, &caps, &enc, &pay, &sink],
        "audio",
    )
}

fn make_udpsink(addr: SocketAddr) -> Result<gst::Element, GstStreamError> {
    let sink = make("udpsink")?;
    sink.set_property("host", addr.ip().to_string());
    sink.set_property("port", addr.port() as i32);
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
