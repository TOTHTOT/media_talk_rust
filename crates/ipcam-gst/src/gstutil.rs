//! gst 样板辅助: ingest (拉流进) 和 rtp_send (发出) 共用的建元件/
//! 链接/同步小函数. 只收两边完全同形的; 带各自业务状态的 probe
//! (SendStats / GstStreamHandle) 留在各自模块.

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::GstStreamError;
use crate::rtp_send::redact_uri_credentials;

/// 把 pipeline 切到 Playing; 失败时把 bus 里的具体 Error 一起返回.
///
/// `set_state` 的 Display 只有 "Element failed to change its state",
/// 真正原因在 bus 的 Error 消息里 (哪个 source/sink 失败). 错误文本可能
/// 带原始 URI (含 rtsp://user:pass@...), 所以两段都先脱敏.
pub(crate) fn set_playing_with_detail(
    pipeline: &gst::Pipeline,
    prefix: &str,
) -> Result<(), String> {
    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        let detail = pipeline
            .bus()
            .and_then(|bus| {
                bus.timed_pop_filtered(
                    gst::ClockTime::from_mseconds(100),
                    &[gst::MessageType::Error],
                )
            })
            .map(|msg| match msg.view() {
                gst::MessageView::Error(err) => {
                    let error_text = redact_uri_credentials(&err.error().to_string());
                    let debug_text = err
                        .debug()
                        .map(|d| redact_uri_credentials(&d))
                        .unwrap_or_default();
                    format!("{error_text} ({debug_text})")
                }
                _ => String::new(),
            })
            .unwrap_or_default();
        let _ = pipeline.set_state(gst::State::Null);
        return Err(if detail.is_empty() {
            format!("{prefix}: {e}")
        } else {
            format!("{prefix}: {e}; bus error: {detail}")
        });
    }
    Ok(())
}

/// Create an element, mapping a missing factory/plugin to `Init` with
/// the element name in the message (a missing `rtspsrc` usually means
/// `gstreamer1.0-plugins-good` is not installed on the target).
///
/// # Arguments
///
/// * `name`: GStreamer 元件名
///
/// returns: Result<gst::Element, GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub(crate) fn make(name: &str) -> Result<gst::Element, GstStreamError> {
    gst::ElementFactory::make(name)
        .build()
        .map_err(|e| GstStreamError::Init(format!("missing element `{name}`: {e}")))
}

/// Link `elems` into a chain (a ! b ! c ! ...).
///
/// # Arguments
///
/// * `elems`: 要链接的元件列表
/// * `what`: 链的描述 (用于错误信息)
///
/// returns: Result<(), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub(crate) fn link_chain(elems: &[&gst::Element], what: &str) -> Result<(), GstStreamError> {
    gst::Element::link_many(elems.iter().copied())
        .map_err(|e| GstStreamError::Link(format!("failed to link {what}: {e}")))?;
    Ok(())
}

/// Add `elems` to the pipeline and sync each with the parent state.
///
/// # Arguments
///
/// * `pipeline`: GStreamer 管线
/// * `elems`: 要加入的元件列表
///
/// returns: Result<(), GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub(crate) fn add_and_sync(
    pipeline: &gst::Pipeline,
    elems: &[&gst::Element],
) -> Result<(), GstStreamError> {
    pipeline
        .add_many(elems.iter().copied())
        .map_err(|e| GstStreamError::Link(format!("failed to add branch to pipeline: {e}")))?;
    for elem in elems {
        if let Err(e) = elem.sync_state_with_parent() {
            tracing::warn!(element = %elem.name(), %e, "sync state with parent failed");
        }
    }
    Ok(())
}

/// Static pad lookup with the element name in the error.
///
/// # Arguments
///
/// * `elem`: 元件
/// * `name`: pad 名称
///
/// returns: Result<gst::Pad, GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub(crate) fn static_pad(elem: &gst::Element, name: &str) -> Result<gst::Pad, GstStreamError> {
    elem.static_pad(name).ok_or(GstStreamError::Link(format!(
        "element `{}` has no {name} pad",
        elem.name()
    )))
}

/// Queue that drops the oldest buffers when full, so a stalled branch
/// can never back-pressure the main path through a tee.
///
/// returns: Result<gst::Element, GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub(crate) fn leaky_queue() -> Result<gst::Element, GstStreamError> {
    let q = make("queue")?;
    q.set_property_from_str("leaky", "downstream");
    q.set_property("max-size-buffers", 5u32);
    Ok(q)
}

/// auto*sink 是 bin, `async` 属性在 READY→PAUSED 时才懒创建的内部真 sink
/// 上, 这里通过 element-added 信号给它关掉. 收不到数据的播放分支如果
/// 保持 async, sink 会永远卡在 preroll, 把整条管线拖在 PAUSED (实测:
/// 音频静默时录像 0 字节). async=false 后来数据直接渲染
pub(crate) fn disable_sink_async(sink: &gst::Element) {
    let Ok(bin) = sink.clone().downcast::<gst::Bin>() else {
        return;
    };
    bin.connect_element_added(move |_bin, child| {
        if child.find_property("async").is_some() {
            child.set_property("async", false);
        }
    });
}
