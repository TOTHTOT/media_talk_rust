//! 摄像头功能: 设置页管理 (搜索/添加/删除, 见 settings 子模块) + 监控页
//! 查看控制 (Task 6, 本文件).
//! ONVIF 搜索是异步网络操作, 在独立线程跑 tokio runtime, 结果经
//! invoke_from_event_loop 回 UI 线程刷属性, UI 线程零阻塞.

mod settings;

pub(crate) use settings::wire_settings;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use ipcam_gst::VideoFrameSink;
use slint::ComponentHandle;
use tracing::{info, warn};

use crate::MainWindow;
use crate::app::{NoticeKind, NoticeLevel};
use crate::store::Store;

/// 监控查看控制器: 任一时刻最多一路 ingest 在跑 (单路全屏形态).
/// 与对讲共用同一个 tap/video-frame 属性 — 靠"phase==0 才能进监控页
/// + 拨打/接听时强制 stop"保证不并发
#[derive(Default)]
pub(crate) struct MonitorController {
    current: Option<ipcam_gst::GstStreamHandle>,
    selected: Option<i64>,
}

impl MonitorController {
    pub fn new() -> Self {
        Self::default()
    }

    // 当前选中摄像头 id; 目前只有状态机单测消费 (UI 侧选中态走
    // monitor-selected-id 属性), 故仅测试构建可见
    #[cfg(test)]
    pub fn selected_id(&self) -> Option<i64> {
        self.selected
    }

    /// 选中摄像头: 停旧起新 (本地查看模式 + 限宽 640, 不要摄像头音频).
    /// 启动失败 (RTSP 不通/认证错) 时旧流已停, 返回 Err 由调用方 toast
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `cam`: 目标摄像头行 (uri/凭据/id 取自它)
    /// * `tap`: 视频帧输出槽 (与对讲共用, 靠 phase 互斥不并发)
    ///
    /// returns: Result<(), ipcam_gst::GstStreamError>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn select(
        &mut self,
        cam: &crate::store::CameraRow,
        tap: &VideoFrameSink,
    ) -> Result<(), ipcam_gst::GstStreamError> {
        self.stop();
        let cfg = ipcam_gst::GstStreamConfig {
            uri: cam.uri.clone(),
            credentials: if cam.username.is_empty() {
                None
            } else {
                Some((cam.username.clone(), cam.password.clone()))
            },
            audio_output: ipcam_gst::AudioOutput::Disabled,
            webrtc: false,
            tap_max_width: Some(640),
            // stream_name 本地模式用不到 (不建 webrtcsink), 随便给个唯一的
            stream_name: format!("monitor-{}", cam.id),
            ..Default::default()
        };
        let taps = ipcam_gst::RawTaps {
            video: Some(tap.clone()),
            audio: None,
        };
        let handle = ipcam_gst::start_with_taps(cfg, taps)?;
        self.current = Some(handle);
        self.selected = Some(cam.id);
        Ok(())
    }

    /// 停止当前查看 (幂等)
    pub fn stop(&mut self) {
        if let Some(h) = self.current.take() {
            h.stop();
        }
        self.selected = None;
    }

    /// 当前流的句柄克隆 (看门狗线程轮询状态用; GstStreamHandle 是
    /// Arc 包装, 克隆与控制器内这份指向同一路会话)
    pub fn current_handle(&self) -> Option<ipcam_gst::GstStreamHandle> {
        self.current.clone()
    }
}

/// 宽限窗看门狗单次轮询的判定 (纯逻辑, 单测覆盖)
#[derive(Debug)]
enum WatchVerdict {
    /// 还在连接中且宽限窗未用完, 继续等
    Wait,
    /// 已出流, 静默退出
    Connected,
    /// 流已终结 (用户切台/停止/通话抢占), 静默退出
    Stopped,
    /// 失败: 停流 + 复位选中 + toast (detail 为错误原文, 可空)
    Failed(Option<String>),
}

/// RTSP 启动是异步的: start_with_taps 只报同步构建错误, 网络不通走无限
/// 重连 (Reconnecting 循环, 永远不到 Failed), 认证错才直接 Failed.
/// 所以宽限窗内到 Playing = 成功; Failed 或超时未出流都按失败处理
///
///
///
/// # Arguments
///
/// * `state`: 当前流状态 (本轮轮询的快照)
/// * `last_error`: 流累计的最近错误原文, 供 Failed 时 toast 展示
/// * `elapsed`: 距看门狗启动的已耗时
/// * `grace`: 宽限窗总长, 超时未出流按失败处理
///
/// returns: WatchVerdict
///
/// # Examples
///
/// ```
///
/// ```
fn watch_verdict(
    state: ipcam_gst::StreamState,
    last_error: Option<&str>,
    elapsed: Duration,
    grace: Duration,
) -> WatchVerdict {
    use ipcam_gst::StreamState as S;
    match state {
        S::Playing => WatchVerdict::Connected,
        S::Ended => WatchVerdict::Stopped,
        S::Failed => WatchVerdict::Failed(last_error.map(str::to_owned)),
        S::Connecting | S::Reconnecting if elapsed < grace => WatchVerdict::Wait,
        S::Connecting | S::Reconnecting => WatchVerdict::Failed(last_error.map(str::to_owned)),
    }
}

/// select 成功后的异步失败兜底: 起看门狗线程在有界宽限窗 (5s) 内轮询
/// handle.state(). 失败/超时 → 停掉本路流 (句柄是本次 select 自己的,
/// 不会误伤新一次 select) 并回 UI 线程; 出流或流已终结则静默退出.
/// UI 侧用 monitor-selected-id 属性做代次校验: 所有停监控路径 (切台/
/// 离开/通话抢占) 都已复位它为 -1, 属性不再是本路 id 就静默丢弃
fn spawn_monitor_watch(handle: ipcam_gst::GstStreamHandle, id: i32, weak: slint::Weak<MainWindow>) {
    const GRACE: Duration = Duration::from_secs(5);
    const INTERVAL: Duration = Duration::from_millis(300);
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        loop {
            let state = handle.state();
            let last_error = handle.stats().last_error;
            match watch_verdict(state, last_error.as_deref(), started.elapsed(), GRACE) {
                WatchVerdict::Wait => std::thread::sleep(INTERVAL),
                WatchVerdict::Connected | WatchVerdict::Stopped => return,
                WatchVerdict::Failed(detail) => {
                    // 停掉本路流 (含重试中的): 用户已切台时它早已 Ended,
                    // 这里的 stop 是幂等 no-op
                    handle.stop();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else { return };
                        if ui.get_monitor_selected_id() != id {
                            return; // 已切台/离开/被通话抢占, 静默丢弃
                        }
                        ui.set_monitor_selected_id(-1);
                        crate::video::clear_frame(&ui);
                        ui.invoke_notify(
                            NoticeLevel::Warn as i32,
                            NoticeKind::CameraConnectFailed as i32,
                            detail.unwrap_or_default().into(),
                        );
                    });
                    return;
                }
            }
        }
    });
}

/// 停止监控查看并把 UI 的 monitor-selected-id 复位为 -1.
/// 停监控的所有路径 (拨打/接听抢视频通路, 离开监控页) 都要走这里,
/// 否则侧边栏会残留高亮一个已停播的摄像头; 同时清 video-frame,
/// 否则画面残留最后一帧
pub(crate) fn stop_and_reset_ui(
    monitor: &Rc<RefCell<MonitorController>>,
    weak: &slint::Weak<MainWindow>,
) {
    monitor.borrow_mut().stop();
    if let Some(ui) = weak.upgrade() {
        ui.set_monitor_selected_id(-1);
        crate::video::clear_frame(&ui);
    }
}

/// 监控页装配: 选中查看 / 离开停止
///
///
///
/// # Arguments
///
/// * `ui`: 主窗口句柄 (回调内只持 weak 引用)
/// * `store`: 摄像头存储 (按 id 查行)
/// * `monitor`: 监控控制器 (单路全屏, 停旧起新)
/// * `tap`: 视频帧输出槽
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
pub(crate) fn wire_monitor(
    ui: &MainWindow,
    store: &Rc<Store>,
    monitor: &Rc<RefCell<MonitorController>>,
    tap: &VideoFrameSink,
) {
    {
        let (store, monitor, tap, weak) =
            (store.clone(), monitor.clone(), tap.clone(), ui.as_weak());
        ui.on_monitor_select(move |id| {
            let cam = store
                .cameras()
                .ok()
                .and_then(|list| list.into_iter().find(|c| c.id == id as i64));
            info!(?id, camera = ?cam, "selected monitor");
            let Some(cam) = cam else {
                warn!("cam is none");
                return;
            };
            let mut m = monitor.borrow_mut();
            match m.select(&cam, &tap) {
                Ok(()) => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_monitor_selected_id(id);
                    }
                    // 同步构建成功不代表 RTSP 连上了 (异步 bus 事件):
                    // 起看门狗盯宽限窗, 失败兜底 toast
                    if let Some(handle) = m.current_handle() {
                        spawn_monitor_watch(handle, id, weak.clone());
                    }
                }
                Err(e) => {
                    warn!(error = %e, uri = %cam.uri, "monitor select failed");
                    if let Some(ui) = weak.upgrade() {
                        ui.set_monitor_selected_id(-1);
                        ui.invoke_notify(
                            NoticeLevel::Warn as i32,
                            NoticeKind::CameraConnectFailed as i32,
                            slint::SharedString::from(e.to_string()),
                        );
                    }
                }
            }
        });
    }
    {
        let (monitor, weak) = (monitor.clone(), ui.as_weak());
        ui.on_monitor_leave(move || {
            stop_and_reset_ui(&monitor, &weak);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_stop_is_idempotent_and_clears_selection() {
        let mut c = MonitorController::new();
        assert!(c.selected_id().is_none());
        c.stop(); // 空状态 stop 不 panic
        assert!(c.selected_id().is_none());
    }

    #[test]
    fn watch_verdict_decides_by_state_and_grace() {
        use ipcam_gst::StreamState as S;
        let grace = Duration::from_secs(5);
        // 出流 = 成功, 静默
        assert!(matches!(
            watch_verdict(S::Playing, None, Duration::ZERO, grace),
            WatchVerdict::Connected
        ));
        // 用户切台/停止 (Ended) = 静默
        assert!(matches!(
            watch_verdict(S::Ended, None, Duration::ZERO, grace),
            WatchVerdict::Stopped
        ));
        // Failed (认证错等不可恢复) 立即失败, 带错误原文
        assert!(matches!(
            watch_verdict(S::Failed, Some("auth"), Duration::ZERO, grace),
            WatchVerdict::Failed(Some(d)) if d == "auth"
        ));
        // 宽限窗内 Connecting/Reconnecting 继续等
        assert!(matches!(
            watch_verdict(S::Connecting, None, Duration::from_secs(1), grace),
            WatchVerdict::Wait
        ));
        assert!(matches!(
            watch_verdict(S::Reconnecting, None, Duration::from_secs(4), grace),
            WatchVerdict::Wait
        ));
        // 超时未出流按失败处理 (网络不通走无限重连, 永远到不了 Failed)
        assert!(matches!(
            watch_verdict(S::Reconnecting, None, grace, grace),
            WatchVerdict::Failed(None)
        ));
        assert!(matches!(
            watch_verdict(S::Connecting, Some("timeout"), grace, grace),
            WatchVerdict::Failed(Some(d)) if d == "timeout"
        ));
    }
}
