//! ICE 通话媒体管线: 复用 SIP 发送链的编码配置, 收发端点从 udpsink/udpsrc
//! 换成 nicesink/nicesrc, 共享 IceAgent 的 NiceAgent (agent 只有一个
//! stream: component 1=video, 2=audio).
//!
//! 链形 (分支按配置裁剪, 无消费者不建):
//!   视频发: 源 → [解码] → 源支路(queue → videoconvert → videoscale →
//!           capsfilter(I420 方像素) → x264enc) → rtph264pay(pt=96)
//!           → nicesink(1)
//!   视频收: nicesrc(1) → rtph264depay → h264parse → avdec_h264
//!           → videoconvert → appsink(RGBA tap)
//!   音频发: 麦克风 → 源支路(queue → audioconvert → audioresample)
//!           → capsfilter(48kHz/stereo) → opusenc → rtpopuspay(pt=111)
//!           → nicesink(2)
//!   音频收: nicesrc(2) → rtpopusdepay → opusdec → audioconvert
//!           → audioresample → tee → [alsasink] 与/或 [S16LE appsink tap]
//!
//! 上下文模型 (Task 3 实测结论): 本版 libnice 的 attach_recv 会把
//! component 的全部 socket I/O (含 conncheck 回包) 路由到传入的
//! GMainContext, 传 NULL 则落进程默认 ctx (无人迭代 = 永久 stall).
//! nicesink/nicesrc 内部各自建私有 GMainContext + 迭代线程, 并以该 ctx
//! attach — 元件启动后 component I/O 由它们的线程驱动, 与 IceAgent 自己
//! 的 glib 线程互不依赖. 同号 component 的 nicesink + nicesrc 共存是
//! 生产形态 (视频收发都在 component 1), 数据面环回测试覆盖该形态.
//!
//! 生命周期: `agent` 是 GObject 属性, 元件 set 时 add ref, 对象本体不会
//! 悬垂; 但 agent 的运行时关停 (Task 6 teardown) 以 IceAgent 存活为前提,
//! 顺序纪律不变 — 调用方 MUST 先 stop/drop IceMediaHandle, 再关闭
//! IceAgent.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::{self as gst};
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use tracing::info;

use crate::rtp_send::{
    TrackKind, TrackSource, audio_source_branch, camera_src_name, file_uri, mic_src_name,
    plug_device_source, plug_uri_source, video_source_branch,
};
use crate::{
    AudioChunkSink, GstStreamError, RawAudioChunk, RawTaps, RawVideoFrame, VideoFrameSink,
    gstutil::{add_and_sync, leaky_queue, link_chain, make, static_pad},
};
use ipcam_ice::IceAgent;

/// component 号: agent 只有一个 stream, 1=video, 2=audio.
const COMP_VIDEO: u32 = 1;
const COMP_AUDIO: u32 = 2;

/// 音频接收方式: 板子扬声器 / 仅 tap (开发机无 ALSA).
#[derive(Debug, Clone)]
pub enum AudioRecv {
    Disabled,
    Alsa(String),
    TapOnly,
}

/// ICE 媒体会话配置.
pub struct IceMediaConfig<'a> {
    /// 已连通 (或连通中) 的 ICE agent.
    pub agent: &'a IceAgent,
    /// 发送视频源; None = 只收不放视频 (调试时常见).
    pub video_src: Option<TrackSource>,
    /// 是否发送音频 (源固定为本机麦克风).
    pub audio_send: bool,
    /// 音频接收方式.
    pub audio_recv: AudioRecv,
    /// 发送限宽, 0 = 不限 (转 Option 传给源支路, 与 RtpDest 语义一致).
    pub max_width: u32,
    /// 收端帧回调 (video=RGBA 帧, audio=S16LE 块), 与 ingest tap 同语义;
    /// 视频收分支路仅在 video tap 存在时建 (无消费者不建, 同 ingest 纪律).
    pub taps: RawTaps,
}

/// 存活中的 ICE 媒体会话.
pub struct IceMediaHandle {
    pipeline: gst::Pipeline,
    bytes: Arc<AtomicBytes>,
}

#[derive(Default)]
struct AtomicBytes {
    tx: AtomicU64,
    rx: AtomicU64,
}

impl IceMediaHandle {
    /// 停止会话 (pipeline 置 Null; agent 由调用方之后关闭, 顺序见模块注释).
    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    /// (发送字节, 接收字节) 快照, 供 UI 状态行.
    pub fn bytes(&self) -> (u64, u64) {
        (
            self.bytes.tx.load(Ordering::Relaxed),
            self.bytes.rx.load(Ordering::Relaxed),
        )
    }
}

impl Drop for IceMediaHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 裸指针的 Send+Sync 搬运容器: 只负责把 NiceAgent 指针带过 pad-added
/// 回调的 'static 边界, 不附带解引用语义.
#[derive(Clone, Copy)]
struct AgentPtr(*mut c_void);

// SAFETY: 容器本身不碰指向的数据; 指针只在 gst 流线程经 nice_agent_send /
// attach_recv 触达, agent 本体由 IceAgent 持有并保证活到元件停用之后
// (模块注释的顺序纪律).
unsafe impl Send for AgentPtr {}
unsafe impl Sync for AgentPtr {}

/// 构建并启动 ICE 媒体管线.
///
/// 收发分支按配置裁剪: 视频收建不建看 video tap, 音频收看 audio_recv +
/// audio tap, 至少一个分支都没有则报 InvalidConfig. 媒体元件启动即
/// attach_recv 接管 component I/O, 与 agent 的连通状态机在同一批 ctx 上
/// 推进; 对端 blob 喂入 (add_remote) 后才会真正收发数据.
///
/// # Arguments
///
/// * `cfg`: 媒体配置; `cfg.agent` 必须已 gathering 完成并喂入对端 blob.
///
/// returns: Result<IceMediaHandle, GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub fn start_ice_media(cfg: &IceMediaConfig<'_>) -> Result<IceMediaHandle, GstStreamError> {
    crate::ensure_init_internal()?;
    if cfg.video_src.is_none() && !cfg.audio_send && !audio_recv_wanted(cfg) {
        return Err(GstStreamError::InvalidConfig(
            "ice media: nothing to send or receive".into(),
        ));
    }
    let pipeline = gst::Pipeline::new();
    let bytes = Arc::new(AtomicBytes::default());
    // 裸指针进 'static 回调的搬运容器; 存活纪律见模块注释 (handle 先于 agent drop).
    let agent = AgentPtr(cfg.agent.raw_agent());
    let max_width = cfg.max_width;

    if let Some(tap) = &cfg.taps.video {
        build_video_recv(&pipeline, agent, tap.clone(), &bytes)?;
    }
    if audio_recv_wanted(cfg) {
        build_audio_recv(&pipeline, agent, cfg, &bytes)?;
    }
    if let Some(source) = &cfg.video_src {
        info!(video_src = %source.label(), "ice media: video send branch");
        let pipeline_cb = pipeline.clone();
        match source {
            TrackSource::File(path) => {
                let uri = file_uri(path)?;
                let bytes = bytes.clone();
                plug_uri_source(&pipeline, &uri, TrackKind::Video, move |pad| {
                    link_video_send(&pipeline_cb, &pad, agent, max_width, &bytes)
                })?;
            }
            TrackSource::Rtsp { uri } => {
                let uri = uri.clone();
                let bytes = bytes.clone();
                plug_uri_source(&pipeline, &uri, TrackKind::Video, move |pad| {
                    link_video_send(&pipeline_cb, &pad, agent, max_width, &bytes)
                })?;
            }
            TrackSource::LocalCamera { device } => {
                let bytes = bytes.clone();
                plug_device_source(
                    &pipeline,
                    camera_src_name(),
                    device.as_deref(),
                    move |pad| link_video_send(&pipeline_cb, &pad, agent, max_width, &bytes),
                )?;
            }
            TrackSource::Mic => {
                return Err(GstStreamError::InvalidConfig(
                    "video_src=mic not supported (audio_send uses the mic)".into(),
                ));
            }
        }
    }
    if cfg.audio_send {
        info!("ice media: audio send branch (mic)");
        let pipeline_cb = pipeline.clone();
        let bytes = bytes.clone();
        plug_device_source(&pipeline, mic_src_name(), None, move |pad| {
            link_audio_send(&pipeline_cb, &pad, agent, &bytes)
        })?;
    }

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| GstStreamError::Stream(format!("ice media play: {e}")))?;
    info!(
        video_tx = cfg.video_src.is_some(),
        audio_tx = cfg.audio_send,
        audio_rx = audio_recv_wanted(cfg),
        video_tap = cfg.taps.video.is_some(),
        "ice media started"
    );
    Ok(IceMediaHandle { pipeline, bytes })
}

/// 音频收分支路是否有消费者: Alsa 喇叭恒有, TapOnly 依赖 audio tap 存在.
fn audio_recv_wanted(cfg: &IceMediaConfig<'_>) -> bool {
    match cfg.audio_recv {
        AudioRecv::Disabled => false,
        AudioRecv::Alsa(_) => true,
        AudioRecv::TapOnly => cfg.taps.audio.is_some(),
    }
}

/// 视频收: nicesrc(1) → rtph264depay → h264parse → avdec_h264 →
/// videoconvert → appsink(RGBA tap). 链名表与 ingest/track.rs 的
/// video_chain_names(H264) 一致; appsink 配置照搬其 build_video_appsink
/// (max-buffers=2, drop, sync=false — 慢消费者丢帧不积压).
///
/// # Arguments
///
/// * `pipeline`: 目标管线
/// * `agent`: 共享 NiceAgent 指针
/// * `tap`: 视频帧回调
/// * `bytes`: 字节计数器 (probe 挂 nicesrc src pad, 记 rx)
///
/// returns: Result<(), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
fn build_video_recv(
    pipeline: &gst::Pipeline,
    agent: AgentPtr,
    tap: VideoFrameSink,
    bytes: &Arc<AtomicBytes>,
) -> Result<(), GstStreamError> {
    let src = make_nice_element("nicesrc", agent, COMP_VIDEO)?;
    let depay = make("rtph264depay")?;
    let parse = make("h264parse")?;
    let dec = make("avdec_h264")?;
    let conv = make("videoconvert")?;
    let sink = video_tap_appsink(tap);
    let refs = [&src, &depay, &parse, &dec, &conv, &sink];
    add_and_sync(pipeline, &refs)?;
    link_chain(&refs, "ice video receive")?;
    install_byte_probe(&static_pad(&src, "src")?, bytes, false);
    Ok(())
}

/// 音频收: nicesrc(2) → rtpopusdepay → opusdec → audioconvert →
/// audioresample → tee → [alsasink(device)] 与/或 [S16LE appsink tap].
/// TapOnly/Alsa 都建 tap 支路 (只要 taps.audio 有值); 支路两桃都缺时整路
/// 不建 (调用前 audio_recv_wanted 已拦截). tap appsink 配置照搬
/// ingest/track.rs 的 build_audio_appsink (max-buffers=8, drop, sync=false).
///
/// # Arguments
///
/// * `pipeline`: 目标管线
/// * `agent`: 共享 NiceAgent 指针
/// * `cfg`: 媒体配置 (audio_recv 决定 alsasink, taps.audio 决定 tap)
/// * `bytes`: 字节计数器 (probe 挂 nicesrc src pad, 记 rx)
///
/// returns: Result<(), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
fn build_audio_recv(
    pipeline: &gst::Pipeline,
    agent: AgentPtr,
    cfg: &IceMediaConfig<'_>,
    bytes: &Arc<AtomicBytes>,
) -> Result<(), GstStreamError> {
    let src = make_nice_element("nicesrc", agent, COMP_AUDIO)?;
    let depay = make("rtpopusdepay")?;
    let dec = make("opusdec")?;
    let conv = make("audioconvert")?;
    let resample = make("audioresample")?;
    let tee = make("tee")?;
    // alsasink 支路 (与 ingest/track.rs 的 build_alsa_elements 同形):
    // queue → alsasink(device), 转码已在 tee 前共用段完成.
    let alsa = match &cfg.audio_recv {
        AudioRecv::Alsa(device) => {
            let queue = make("queue")?;
            let sink = make("alsasink")?;
            sink.set_property("device", device);
            Some(vec![queue, sink])
        }
        _ => None,
    };
    // tap 支路: leaky_queue → S16LE appsink (与 ingest 音频 tap 同形).
    let tap = match &cfg.taps.audio {
        Some(cb) => Some(vec![leaky_queue()?, audio_tap_appsink(cb.clone())]),
        None => None,
    };

    let mut refs = vec![&src, &depay, &dec, &conv, &resample, &tee];
    if let Some(alsa) = &alsa {
        refs.extend(alsa.iter());
    }
    if let Some(tap) = &tap {
        refs.extend(tap.iter());
    }
    add_and_sync(pipeline, &refs)?;
    link_chain(
        &[&src, &depay, &dec, &conv, &resample, &tee],
        "ice audio receive",
    )?;
    if let Some(alsa) = &alsa {
        let alsa_refs: Vec<&gst::Element> = alsa.iter().collect();
        link_chain(&alsa_refs, "ice audio alsa branch")?;
        link_tee_branch(&tee, &alsa[0])?;
    }
    if let Some(tap) = &tap {
        let tap_refs: Vec<&gst::Element> = tap.iter().collect();
        link_chain(&tap_refs, "ice audio tap branch")?;
        link_tee_branch(&tee, &tap[0])?;
    }
    install_byte_probe(&static_pad(&src, "src")?, bytes, false);
    Ok(())
}

/// 视频发: 源支路 (rtp_send 复用, x264enc 基础五属性) →
/// rtph264pay(pt=96, config-interval=1) → nicesink(1).
/// 尾属性与 rtp_send/chain.rs 逐一对齐: config-interval=1 参数集周期重发;
/// nicesink sync/async 对齐 udpsink 语义 (sync=true 按码流时间戳限速,
/// async=false 不卡 preroll). doorphone 专用 slice-max-size 不加 (ICE
/// 两端都是本程序, FU-A 正常).
///
/// # Arguments
///
/// * `pipeline`: 目标管线
/// * `raw_pad`: 源解码出的裸流 pad
/// * `agent`: 共享 NiceAgent 指针
/// * `max_width`: 发送限宽, 0 = 不限
/// * `bytes`: 字节计数器 (probe 挂 nicesink sink pad, 记 tx)
///
/// returns: Result<(), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
fn link_video_send(
    pipeline: &gst::Pipeline,
    raw_pad: &gst::Pad,
    agent: AgentPtr,
    max_width: u32,
    bytes: &Arc<AtomicBytes>,
) -> Result<(), GstStreamError> {
    let cap = (max_width > 0).then_some(max_width);
    let mut chain = video_source_branch(cap)?;
    let pay = make("rtph264pay")?;
    pay.set_property("pt", 96u32);
    pay.set_property("config-interval", 1i32);
    let sink = make_nice_sink(agent, COMP_VIDEO)?;
    install_byte_probe(&static_pad(&sink, "sink")?, bytes, true);
    chain.extend([pay, sink]);
    plug_chain(pipeline, raw_pad, &chain, "ice video send")
}

/// 音频发: 源支路 (queue → audioconvert → audioresample) →
/// capsfilter(48kHz/stereo) → opusenc → rtpopuspay(pt=111) → nicesink(2).
///
/// # Arguments
///
/// * `pipeline`: 目标管线
/// * `raw_pad`: 麦克风采出的裸流 pad
/// * `agent`: 共享 NiceAgent 指针
/// * `bytes`: 字节计数器 (probe 挂 nicesink sink pad, 记 tx)
///
/// returns: Result<(), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
fn link_audio_send(
    pipeline: &gst::Pipeline,
    raw_pad: &gst::Pad,
    agent: AgentPtr,
    bytes: &Arc<AtomicBytes>,
) -> Result<(), GstStreamError> {
    let mut chain = audio_source_branch()?;
    // Opus 定死 48kHz/stereo; capsfilter 强制输出格式, 不依赖源采样率
    // (与 rtp_send 的 8kHz/mono 同模式).
    let caps = make("capsfilter")?;
    caps.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("rate", 48000i32)
            .field("channels", 2i32)
            .build(),
    );
    let enc = make("opusenc")?;
    let pay = make("rtpopuspay")?;
    pay.set_property("pt", 111u32);
    let sink = make_nice_sink(agent, COMP_AUDIO)?;
    install_byte_probe(&static_pad(&sink, "sink")?, bytes, true);
    chain.extend([caps, enc, pay, sink]);
    plug_chain(pipeline, raw_pad, &chain, "ice audio send")
}

/// 建 nicesink/nicesrc 并注入共享 NiceAgent.
fn make_nice_element(
    name: &str,
    agent: AgentPtr,
    component: u32,
) -> Result<gst::Element, GstStreamError> {
    let e = make(name)?;
    e.set_property("agent", agent_object(agent.0));
    e.set_property("stream", 1u32); // agent 只有一个 stream
    e.set_property("component", component);
    Ok(e)
}

/// nicesink 尾部公共属性: sync/async 对齐 rtp_send 的 udpsink 语义.
fn make_nice_sink(agent: AgentPtr, component: u32) -> Result<gst::Element, GstStreamError> {
    let sink = make_nice_element("nicesink", agent, component)?;
    sink.set_property("sync", true);
    sink.set_property("async", false);
    Ok(sink)
}

/// 把裸 NiceAgent 指针包成 glib::Object (nicesink/nicesrc 的 `agent`
/// 属性是 GParamSpecObject(NICE_TYPE_AGENT) — 实测报错
/// "expected: 'NiceAgent', got: 'gpointer'", 不是旧版 gst-plugins-bad 的
/// gpointer 属性).
fn agent_object(p: *mut c_void) -> glib::Object {
    // SAFETY: p 是 IceAgent 持有的 NiceAgent GObject (G_TYPE_OBJECT 子类);
    // from_glib_none 只借用引用, set_property 经 g_value_set_object 自行
    // add ref (元件 dispose 对应释放), 不偷所有权. 指针本体存活纪律见模块注释.
    unsafe { glib::translate::from_glib_none(p.cast::<glib::gobject_ffi::GObject>()) }
}

/// BUFFER probe 累加字节数: nicesink 的 sink pad (发送方向) / nicesrc 的
/// src pad (接收方向), 供 UI 状态行取快照.
fn install_byte_probe(pad: &gst::Pad, bytes: &Arc<AtomicBytes>, is_tx: bool) {
    let bytes = bytes.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        if let Some(gst::PadProbeData::Buffer(ref buffer)) = info.data {
            let counter = if is_tx { &bytes.tx } else { &bytes.rx };
            counter.fetch_add(buffer.size() as u64, Ordering::Relaxed);
        }
        gst::PadProbeReturn::Ok
    });
}

/// 公共尾巴 (与 rtp_send/chain.rs 的 add_link_and_plug 同序): 元件入管线
/// → 依次互链 → 源 pad 插到链头 → 逐元件同步父状态 (链条在管线 Playing
/// 后才接上时必须手动 sync).
fn plug_chain(
    pipeline: &gst::Pipeline,
    raw_pad: &gst::Pad,
    chain: &[gst::Element],
    what: &str,
) -> Result<(), GstStreamError> {
    pipeline
        .add_many(chain.iter())
        .map_err(|e| GstStreamError::Init(format!("add {what} chain: {e}")))?;
    let refs: Vec<&gst::Element> = chain.iter().collect();
    link_chain(&refs, what)?;
    let head_sink = static_pad(&chain[0], "sink")?;
    raw_pad
        .link(&head_sink)
        .map_err(|e| GstStreamError::Link(format!("plug src pad to {what} chain: {e}")))?;
    for elem in chain {
        elem.sync_state_with_parent()
            .map_err(|e| GstStreamError::Link(format!("sync {what} chain state: {e}")))?;
    }
    Ok(())
}

/// Request one tee src pad and link it to `first`'s sink pad (branch head).
fn link_tee_branch(tee: &gst::Element, first: &gst::Element) -> Result<(), GstStreamError> {
    let sink = static_pad(first, "sink")?;
    let tee_src = tee
        .request_pad_simple("src_%u")
        .ok_or(GstStreamError::Link("tee request pad src_%u failed".into()))?;
    tee_src
        .link(&sink)
        .map_err(|e| GstStreamError::Link(format!("failed to link tee src pad: {e}")))?;
    Ok(())
}

/// appsink delivering decoded RGBA frames into the tap callback.
/// `drop + max-buffers=2`: a slow consumer drops frames instead of
/// accumulating latency/memory. 配置与 ingest/track.rs build_video_appsink
/// 完全一致.
fn video_tap_appsink(sink: VideoFrameSink) -> gst::Element {
    let appsink = gst_app::AppSink::builder()
        .caps(
            &gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .build(),
        )
        .max_buffers(2u32)
        .drop(true)
        .sync(false)
        .build();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink_el| {
                let sample = sink_el.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                let info = gst_video::VideoInfo::from_caps(caps)
                    .map_err(|_| gst::FlowError::NotNegotiated)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?.to_owned();
                let frame = gst_video::VideoFrame::from_buffer_readable(buffer, &info)
                    .map_err(|_| gst::FlowError::Error)?;
                let data = frame.plane_data(0).map_err(|_| gst::FlowError::Error)?;
                sink.lock()(RawVideoFrame {
                    width: info.width(),
                    height: info.height(),
                    stride: info.stride()[0] as usize,
                    data,
                });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    appsink.upcast()
}

/// appsink delivering decoded S16LE audio chunks into the tap callback.
/// 配置与 ingest/track.rs build_audio_appsink 完全一致.
fn audio_tap_appsink(sink: AudioChunkSink) -> gst::Element {
    let appsink = gst_app::AppSink::builder()
        .caps(
            &gst::Caps::builder("audio/x-raw")
                .field("format", "S16LE")
                .build(),
        )
        .max_buffers(8u32)
        .drop(true)
        .sync(false)
        .build();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink_el| {
                let sample = sink_el.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                let s = caps.structure(0).ok_or(gst::FlowError::NotNegotiated)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                sink.lock()(RawAudioChunk {
                    rate: s.get::<i32>("rate").unwrap_or(0) as u32,
                    channels: s.get::<i32>("channels").unwrap_or(0) as u32,
                    data: map.as_slice(),
                });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    appsink.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipcam_ice::{IceConfig, IceState};
    use std::time::Duration;
    use tokio::sync::mpsc;

    async fn wait_connected(agent: &IceAgent) {
        let mut rx = agent.state_rx();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if *rx.borrow() == IceState::Connected {
                return;
            }
            tokio::select! {
                _ = rx.changed() => {}
                _ = tokio::time::sleep_until(deadline) => panic!("ICE not connected"),
            }
        }
    }

    /// 真实数据面环回 (非图形状断言): 双 agent 本机打洞 (Task 3 模式),
    /// A 侧建 appsrc → nicesink(1) + nicesrc(1) → appsink — 与生产同型的
    /// 收发共存, B 侧裸字节对发. 必须实测的原因: 本版 libnice 的 socket
    /// I/O (含 conncheck 回包) 只路由到 attach_recv 传入且有人迭代的
    /// GMainContext — nicesink/nicesrc 若没像预期那样自建迭代线程, 或收发
    /// 共存时后 attach 者抢走 I/O, 这里会真实超时, 而不是图形状测试那样
    /// 绿着却收不到数据.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nice_elements_move_bytes_over_connected_ice() {
        crate::ensure_init_internal().unwrap();
        if gst::ElementFactory::find("nicesrc").is_none()
            || gst::ElementFactory::find("nicesink").is_none()
        {
            tracing::warn!(
                "nicesrc/nicesink not available (gst-plugins-bad missing), skipping test"
            );
            return;
        }
        let Ok(a) = IceAgent::new(IceConfig {
            stun: None,
            controlling: true,
        }) else {
            return; // libnice 不可用
        };
        let Ok(b) = IceAgent::new(IceConfig {
            stun: None,
            controlling: false,
        }) else {
            return;
        };

        // A 侧管线先发后收: appsrc → nicesink(1) 与 nicesrc(1) → appsink.
        // Playing 先建 — nicesrc start 时 attach_recv 把 component 1 的
        // socket I/O 接管到它的私有 ctx (nicesink 若也 attach 同理), 与
        // 生产一致地先于/伴随连通建立.
        let pipeline = gst::Pipeline::new();
        let asrc = gst_app::AppSrc::builder()
            .caps(&gst::Caps::builder("application/x-ice").build())
            .build();
        let nicesink = make_nice_sink(AgentPtr(a.raw_agent()), COMP_VIDEO).unwrap();
        let nicesrc = make_nice_element("nicesrc", AgentPtr(a.raw_agent()), COMP_VIDEO).unwrap();
        let asink = gst_app::AppSink::builder().sync(false).build();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        asink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink_el| {
                    let sample = sink_el.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let _ = tx.try_send(map.as_slice().to_vec());
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let asrc_el = asrc.clone().upcast::<gst::Element>();
        let asink_el = asink.upcast();
        pipeline
            .add_many([&asrc_el, &nicesink, &nicesrc, &asink_el])
            .unwrap();
        gst::Element::link_many([&asrc_el, &nicesink]).unwrap();
        gst::Element::link_many([&nicesrc, &asink_el]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        // 信令: 交换 blob 触发 conncheck; B 侧裸挂 attach (agent 自己的
        // ctx, 由 IceAgent 的 glib 线程迭代), 数据进 channel 供断言.
        let (btx, mut brx) = mpsc::channel::<Vec<u8>>(8);
        b.attach_recv(move |data| {
            let _ = btx.try_send(data.to_vec());
        })
        .await
        .unwrap();
        let (a_blob, b_blob) = tokio::join!(a.local_blob(), b.local_blob());
        let (a_blob, b_blob) = (a_blob.unwrap(), b_blob.unwrap());
        let (ra, rb) = tokio::join!(a.add_remote(&b_blob), b.add_remote(&a_blob));
        ra.unwrap();
        rb.unwrap();
        wait_connected(&a).await;
        wait_connected(&b).await;
        println!("ice_media loopback: both agents Connected");

        // B → A: B 裸发字节, A 的 nicesrc → appsink 必须 5s 内收到
        // (nicesrc 私有 ctx 真的被迭代 + conncheck 回包路由未被收发
        // 共存打乱).
        b.send(b"ping").await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("nicesrc receive timeout")
            .expect("channel closed");
        assert_eq!(got, b"ping");
        println!("ice_media loopback: nicesrc -> appsink received {got:?}");

        // A → B: 走生产发送元件 appsrc → nicesink, B 裸收.
        for _ in 0..3 {
            let buf = gst::Buffer::from_slice(b"pong");
            asrc.push_buffer(buf).unwrap();
        }
        let got = tokio::time::timeout(Duration::from_secs(5), brx.recv())
            .await
            .expect("nicesink send timeout")
            .expect("channel closed");
        assert_eq!(got, b"pong");
        println!("ice_media loopback: appsrc -> nicesink delivered {got:?}");

        pipeline.set_state(gst::State::Null).unwrap();
    }
}
