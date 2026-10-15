//! 源侧抽象: 一路媒体从哪来 (文件/RTSP/本机相机/麦克风), 以及怎么把
//! 源接进管线 (uridecodebin3 动态 pad / 设备源静态 pad). 只产裸流 pad,
//! 后续重编码发送链在 chain.rs.

use crate::GstStreamError;
use crate::gstutil::make;
use gstreamer::prelude::*;
use gstreamer::{self as gst};
use std::path::PathBuf;
use tracing::warn;

/// 共享 uridecodebin3 的音视频回调类型: audio_cb / video_cb 各一个 Arc 闭包
type MediaClusterType =
    Option<std::sync::Arc<dyn Fn(gst::Pad) -> Result<(), GstStreamError> + Send + Sync + 'static>>;

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

impl TrackSource {
    /// 日志用标签: Rtsp uri 可能内嵌凭据 (rtsp://user:pass@...),
    /// 必须先脱敏再进日志
    pub(super) fn label(&self) -> String {
        match self {
            Self::File(path) => format!("file:{}", path.display()),
            Self::Rtsp { uri } => format!("rtsp:{}", redact_uri_credentials(uri)),
            Self::LocalCamera { device } => {
                format!("camera:{}", device.as_deref().unwrap_or("default"))
            }
            Self::Mic => "mic".to_string(),
        }
    }
}

/// CLI 源描述解析: file:<路径> | rtsp://<uri> | camera[:<设备>] | mic
pub fn parse_track_source(s: &str) -> Result<TrackSource, String> {
    if let Some(path) = s.strip_prefix("file:") {
        return Ok(TrackSource::File(PathBuf::from(path)));
    }
    if s.starts_with("rtsp://") {
        return Ok(TrackSource::Rtsp { uri: s.to_string() });
    }
    if s == "camera" {
        return Ok(TrackSource::LocalCamera { device: None });
    }
    if let Some(dev) = s.strip_prefix("camera:") {
        return Ok(TrackSource::LocalCamera {
            device: Some(dev.to_string()),
        });
    }
    if s == "mic" {
        return Ok(TrackSource::Mic);
    }
    Err(format!(
        "unknown track source: {s} (file:/rtsp://camera/mic)"
    ))
}

/// 把 uri authority 里的 user:pass@ 换成 ***:***@; 无凭据原样返回
pub(super) fn redact_uri_credentials(uri: &str) -> String {
    let Some(scheme_end) = uri.find("://") else {
        return uri.to_string();
    };
    let after_scheme = &uri[scheme_end + 3..];
    let authority_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    let Some(at) = authority.rfind('@') else {
        return uri.to_string();
    };
    format!(
        "{}://***:***@{}",
        &uri[..scheme_end],
        &after_scheme[at + 1..]
    )
}

/// 要建的是哪一路 (源段按它匹配 uridecodebin3 的裸流 pad)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TrackKind {
    Audio,
    Video,
}

/// 同文件共享 uridecodebin3: 音视频两路都来自同一文件时, 只建一个
/// uridecodebin3 解码一次, audio pad 和 video pad 各自接发送链.
///
/// # Arguments
///
/// * `pipeline`: GStreamer 管线
/// * `uri`: 文件 URI (`file://...`)
/// * `audio_cb`: 音频 pad 出现时的发送链回调, None 表示不建音频路
/// * `video_cb`: 视频 pad 出现时的发送链回调, None 表示不建视频路
///
/// returns: Result<()>
///
/// # Examples
///
/// ```
///
/// ```
pub(super) fn plug_shared_uri_source(
    pipeline: &gst::Pipeline,
    uri: &str,
    audio_cb: MediaClusterType,
    video_cb: MediaClusterType,
) -> Result<(), GstStreamError> {
    let dec = make("uridecodebin3")?;
    dec.set_property("uri", uri);
    pipeline
        .add(&dec)
        .map_err(|e| GstStreamError::Init(format!("add uridecodebin3: {e}")))?;

    dec.connect_pad_added(move |_dec, pad| {
        // 1. Try immediate current_caps first
        match try_link_from_caps(&pad, pad.current_caps().as_ref(), &video_cb, &audio_cb) {
            Ok(true) => return, // linked successfully, done
            Ok(false) => {
                warn!("not our type, fall through to notify")
            }
            Err(e) => {
                warn!(error = %e, "shared uridecodebin3: try_link failed");
                return;
            }
        }

        // 2. Caps not ready: listen for notify::caps to retry
        let video_cb = video_cb.clone();
        let audio_cb = audio_cb.clone();
        pad.connect_notify(Some("caps"), move |p, _spec| {
            on_pad_caps_notified(p, &video_cb, &audio_cb);
        });
    });
    dec.sync_state_with_parent()
        .map_err(|e| GstStreamError::Init(format!("sync uridecodebin3: {e}")))?;
    Ok(())
}

/// 根据 pad caps 结构将 pad 分派到 audio 或 video 发送链回调.
///
/// # Arguments
///
/// * `pad`: uridecodebin3 新创建的 pad
/// * `caps`: pad 的当前 caps, None 表示还没协商好
/// * `video_cb`: 视频发送链回调
/// * `audio_cb`: 音频发送链回调
///
/// returns: Result<bool> — Ok(true) linked 成功, Ok(false) 非音视频类型, Err 回调执行失败
///
/// # Examples
///
/// ```
///
/// ```
fn try_link_from_caps(
    pad: &gst::Pad,
    caps: Option<&gst::Caps>,
    video_cb: &MediaClusterType,
    audio_cb: &MediaClusterType,
) -> Result<bool, GstStreamError> {
    let Some(caps) = caps else { return Ok(false) };
    dispatch_pad_core(pad, &caps, video_cb, audio_cb)
}

/// 共享分派核心: 根据 caps 结构名走 audio 或 video 回调.
fn dispatch_pad_core(
    pad: &gst::Pad,
    caps: &gst::Caps,
    video_cb: &MediaClusterType,
    audio_cb: &MediaClusterType,
) -> Result<bool, GstStreamError> {
    let Some(s) = caps.structure(0) else { return Ok(false) };
    match s.name().as_str() {
        "video/x-raw" => {
            if let Some(ref cb) = *video_cb {
                cb(pad.clone())?;
            }
            Ok(true)
        }
        "audio/x-raw" => {
            if let Some(ref cb) = *audio_cb {
                cb(pad.clone())?;
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// pad 收到 caps 通知时调用, 触发 link 分派重试.
///
/// # Arguments
///
/// * `pad`: 收到 caps 通知的 pad
/// * `video_cb`: 视频发送链回调
/// * `audio_cb`: 音频发送链回调
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
fn on_pad_caps_notified(pad: &gst::Pad, video_cb: &MediaClusterType, audio_cb: &MediaClusterType) {
    if let Some(caps) = pad.current_caps() {
        if let Err(e) = dispatch_pad_core(pad, &caps, video_cb, audio_cb) {
            warn!(error = %e, "shared uridecodebin3: notify caps link failed");
        }
    }
}

/// 文件/RTSP 源: uridecodebin3 解码出裸流, pad-added 里按 TrackKind 匹配,
/// 命中的 pad 交给 on_raw_pad 接发送链. 源里没有请求的轨 = pad 永远不来,
/// 不阻塞另一路 (靠 stats 0 包发现)
pub(super) fn plug_uri_source(
    pipeline: &gst::Pipeline,
    uri: &str,
    kind: TrackKind,
    on_raw_pad: impl Fn(gst::Pad) -> Result<(), GstStreamError> + Send + Sync + 'static,
) -> Result<(), GstStreamError> {
    let dec = make("uridecodebin3")?;
    dec.set_property("uri", uri);
    pipeline
        .add(&dec)
        .map_err(|e| GstStreamError::Init(format!("add uridecodebin3: {e}")))?;
    dec.connect_pad_added(move |_dec, pad| {
        // uridecodebin3 may fire pad-added before caps are negotiated;
        // use query_caps as fallback when current_caps returns None
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let Some(s) = caps.structure(0) else { return };
        let hit = matches!(
            (kind, s.name().as_str()),
            (TrackKind::Video, "video/x-raw") | (TrackKind::Audio, "audio/x-raw")
        );
        if hit {
            if let Err(e) = on_raw_pad(pad.clone()) {
                tracing::warn!(error = %e, "rtp sender: failed to link send chain");
            }
        }
    });
    dec.sync_state_with_parent()
        .map_err(|e| GstStreamError::Init(format!("sync uridecodebin3: {e}")))?;
    Ok(())
}

/// 平台分发集中在这两个函数, 不用 #[cfg] 散布.
/// 未知平台编译能过, 运行时 make() 报元件不存在 (与缺插件行为一致)
pub(super) fn camera_src_name() -> &'static str {
    if cfg!(windows) {
        "ksvideosrc"
    } else {
        "v4l2src"
    }
}

pub(super) fn mic_src_name() -> &'static str {
    if cfg!(windows) {
        "wasapisrc"
    } else {
        "alsasrc"
    }
}

/// 设备源出来直接是裸流 (或可被下游 negotiate 成裸流), src pad 静态存在
pub(super) fn plug_device_source(
    pipeline: &gst::Pipeline,
    element_name: &str,
    device: Option<&str>,
    on_raw_pad: impl FnOnce(gst::Pad) -> Result<(), GstStreamError>,
) -> Result<(), GstStreamError> {
    let src = make(element_name)?;
    if let Some(dev) = device {
        // v4l2src 用 device=/dev/videoX; ksvideosrc 用 device-path.
        // MIPI 相机在 Linux 上同为 v4l2src, 只是节点不同
        let prop = if element_name == "ksvideosrc" {
            "device-path"
        } else {
            "device"
        };
        src.set_property(prop, dev);
    }
    pipeline
        .add(&src)
        .map_err(|e| GstStreamError::Init(format!("add {element_name}: {e}")))?;
    src.sync_state_with_parent()
        .map_err(|e| GstStreamError::Init(format!("sync {element_name}: {e}")))?;
    let pad = src
        .static_pad("src")
        .ok_or_else(|| GstStreamError::Link(format!("{element_name} has no src pad")))?;
    on_raw_pad(pad)
}

pub(super) fn file_uri(path: &std::path::Path) -> Result<String, GstStreamError> {
    let abs = path.canonicalize().map_err(|e| {
        GstStreamError::InvalidConfig(format!("media file not found: {}: {e}", path.display()))
    })?;
    let uri = gst::glib::filename_to_uri(&abs, None)
        .map_err(|e| GstStreamError::InvalidConfig(format!("to file uri: {e}")))?;
    Ok(uri.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rtsp 标签必须脱敏内嵌凭据: user:pass 不得出现在日志文本里
    #[test]
    fn rtsp_label_redacts_credentials() {
        let source = TrackSource::Rtsp {
            uri: "rtsp://admin:secret@192.168.1.10:8554/ch01".to_string(),
        };
        let label = source.label();
        assert!(!label.contains("secret"), "credential leaked: {label}");
        assert!(!label.contains("admin"), "username leaked: {label}");
        assert!(
            label.contains("192.168.1.10:8554/ch01"),
            "host lost: {label}"
        );

        // 无凭据的 uri 原样保留
        let plain = TrackSource::Rtsp {
            uri: "rtsp://192.168.1.10:8554/ch01".to_string(),
        };
        assert_eq!(plain.label(), "rtsp:rtsp://192.168.1.10:8554/ch01");
    }

    #[test]
    fn parse_track_source_variants() {
        assert!(matches!(
            parse_track_source("file:a/b.mp4"),
            Ok(TrackSource::File(p)) if p == std::path::Path::new("a/b.mp4")
        ));
        assert!(matches!(
            parse_track_source("rtsp://cam/1"),
            Ok(TrackSource::Rtsp { uri }) if uri == "rtsp://cam/1"
        ));
        assert!(matches!(
            parse_track_source("camera"),
            Ok(TrackSource::LocalCamera { device: None })
        ));
        assert!(matches!(
            parse_track_source("camera:/dev/video1"),
            Ok(TrackSource::LocalCamera { device: Some(d) }) if d == "/dev/video1"
        ));
        assert!(matches!(parse_track_source("mic"), Ok(TrackSource::Mic)));
        assert!(parse_track_source("bogus").is_err());
    }

    /// 平台分发: windows 用 ksvideosrc/wasapisrc, 其余平台 v4l2src/alsasrc
    #[test]
    fn device_src_names_match_platform() {
        if cfg!(windows) {
            assert_eq!(camera_src_name(), "ksvideosrc");
            assert_eq!(mic_src_name(), "wasapisrc");
        } else {
            assert_eq!(camera_src_name(), "v4l2src");
            assert_eq!(mic_src_name(), "alsasrc");
        }
    }
}
