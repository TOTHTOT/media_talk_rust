//! 发送链: 裸流 pad → 重编码 → payloader → udpsink. 所有源共用,
//! 由 sender 的 track 组装在源 pad 出现时回调进来.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::info;

use super::config::{AudioDest, RtpDest};
use crate::GstStreamError;
use crate::PktStats;
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

/// x264enc 基础五属性 (SIP 与 ICE 链路逐属性一致的契约集合):
/// zerolatency + veryfast + 400kbps + 秒级 IDR + 不插 AUD NAL (设备
/// 解析器不认). doorphone 专用的 slice-max-size 是 SIP 尾部私有约束,
/// 不在此列 (ICE 两端都是本程序, FU-A 正常).
fn configure_x264enc(enc: &gst::Element) {
    enc.set_property_from_str("tune", "zerolatency");
    enc.set_property_from_str("speed-preset", "veryfast");
    enc.set_property("bitrate", 400u32); // kbps, CIF 档足够
    enc.set_property("key-int-max", 30u32); // 秒级 IDR, 对端中途收也能起
    enc.set_property("aud", false);
}

/// 视频源支路 (SIP/ICE 发送链共用的编码前段):
/// queue → videoconvert → videoscale → capsfilter → x264enc(基础五属性).
/// videoscale + capsfilter 常驻, 编码前统一 I420 方像素;
/// max_width 有值时限宽 (范围 caps, 保持宽高比). 尾部 (payloader/sink)
/// 由各链路自拼: SIP 走 rtph264pay → udpsink, ICE 走 rtph264pay → nicesink.
pub(crate) fn video_source_branch(
    max_width: Option<u32>,
) -> Result<Vec<gst::Element>, GstStreamError> {
    // convert 编码格式转换
    let mut chain = vec![make("queue")?, make("videoconvert")?];
    // 编码前统一成 I420 方像素偶数尺寸, videoscale 常驻:
    // - 相机源可能协商出 Y42B (4:2:2), x264enc 会编成 High 4:2:2 profile,
    //   门口机硬解只认 4:2:0, 解不出来 = 黑屏 (实测 1280x720 YUY2 相机)
    // - 只限宽时 videoscale 会用 PAR=2/1 代偿高度 (1280x720 → 640x720
    //   非方像素), 不认 SAR 的对端画面被压扁
    // - 方像素化后尺寸可能变奇数 (352x288 PAR 108/55 → 352x147), x264 的
    //   4:2:0 要求偶数宽高, 奇数直接初始化失败, 所以宽高都用步进 2 的范围
    let scale = make("videoscale")?;
    let caps = make("capsfilter")?;
    // 限宽: 1080p 相机直发超出门口机解码上限会黑屏. caps 用范围而不是
    // 定值: videoscale 只在源超宽时降采样, 且保持宽高比
    let max_w = max_width.map(|w| w as i32).unwrap_or(i32::MAX - 1);
    caps.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "I420")
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .field("width", gst::IntRange::with_step(2, max_w, 2))
            .field("height", gst::IntRange::with_step(2, i32::MAX - 1, 2))
            .build(),
    );
    chain.push(scale);
    chain.push(caps);
    let enc = make("x264enc")?;
    configure_x264enc(&enc);
    chain.push(enc);
    Ok(chain)
}

/// 音频源支路 (SIP/ICE 发送链共用的重采样前段):
/// queue → audioconvert → audioresample. 目标格式的 capsfilter 与
/// 编码器由各链路自拼 (SIP 锁 8kHz mono G.711, ICE 锁 48kHz stereo Opus).
pub(crate) fn audio_source_branch() -> Result<Vec<gst::Element>, GstStreamError> {
    Ok(vec![
        make("queue")?,
        make("audioconvert")?,
        make("audioresample")?,
    ])
}

/// 视频发送链 (所有源共用): 裸流重编码, 参数集周期重发.
/// videoscale + capsfilter 常驻, 编码前统一 I420 方像素;
/// dest.max_width 有值时限宽 (范围 caps, 保持宽高比)
pub(super) fn link_video_send_chain(
    pipeline: &gst::Pipeline,
    raw_pad: &gst::Pad,
    dest: RtpDest,
    socket: Option<crate::SharedUdpSocket>,
    stats: Arc<PktStats>,
) -> Result<(), GstStreamError> {
    let mut chain = video_source_branch(dest.max_width)?;
    // doorphone 硬解约束 (SIP 专用, 不进共用支路): 每个 NAL 切到 MTU 以下,
    // 从源头消除 FU-A 分片需求 (mode 0)
    if let Some(enc) = chain.last() {
        enc.set_property("option-string", "slice-max-size=1300");
    }
    let pay = make("rtph264pay")?;
    pay.set_property("pt", dest.payload_type as u32);
    pay.set_property("config-interval", 1i32);
    install_pkt_probe(&pay, stats.clone(), false)?;
    chain.extend([pay, make_udpsink(dest.addr, socket.as_ref())?]);
    let refs: Vec<&gst::Element> = chain.iter().collect();
    add_link_and_plug(pipeline, raw_pad, &refs, "video")
}

/// 音频发送链 (所有源共用): 裸流重采样成 answer 协商出的 G.711 (8kHz 单声道)
pub(super) fn link_audio_send_chain(
    pipeline: &gst::Pipeline,
    raw_pad: &gst::Pad,
    dest: AudioDest,
    socket: Option<crate::SharedUdpSocket>,
    stats: Arc<PktStats>,
) -> Result<(), GstStreamError> {
    let mut chain = audio_source_branch()?;
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
    let sink = make_udpsink(dest.addr, socket.as_ref())?;
    chain.extend([caps, enc, pay, sink]);
    let refs: Vec<&gst::Element> = chain.iter().collect();
    add_link_and_plug(pipeline, raw_pad, &refs, "audio")
}

fn make_udpsink(
    addr: SocketAddr,
    socket: Option<&crate::SharedUdpSocket>,
) -> Result<gst::Element, GstStreamError> {
    let sink = make("udpsink")?;
    sink.set_property("host", addr.ip().to_string());
    sink.set_property("port", addr.port() as i32);
    // sync=true: 按码流时间戳限速发送; async=false: 不等 clock 对齐 preroll
    sink.set_property("sync", true);
    sink.set_property("async", false);
    // 共享 socket (对称 RTP): 源端口 = socket 绑定端口; close-socket=false
    // — socket 由调用方持有, 管线 teardown 不能关掉, 接收器还在用
    if let Some(sock) = socket {
        sink.set_property("socket", sock.as_gio());
        sink.set_property("close-socket", false);
    }
    info!(host = %addr.ip(), port = addr.port(), shared_socket = socket.is_some(), "udpsink target");
    Ok(sink)
}

/// 在 payloader 的 src pad 上数包: 每个 buffer = 一个 RTP 包, 这是个探测器
fn install_pkt_probe(
    pay: &gst::Element,
    stats: Arc<PktStats>,
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
