//! 摄像头功能: 设置页管理 (搜索/添加/删除, 见 settings 子模块) + 监控页
//! 查看控制 (Task 6, 本文件).
//! ONVIF 搜索是异步网络操作, 在独立线程跑 tokio runtime, 结果经
//! invoke_from_event_loop 回 UI 线程刷属性, UI 线程零阻塞.

mod settings;

pub(crate) use settings::wire_settings;

use std::cell::RefCell;
use std::rc::Rc;

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
    /// 异步失败 (RTSP 连不通) 由 ingest 侧判定: 配置里的
    /// `ReconnectPolicy::max_attempts` 是唯一的"多久算失败"标准, 达到上限
    /// 后 ingest 转失败终态并触发 `on_terminal` 回调, 回调把
    /// `UiEvent::MonitorFailed` 投进修/UI 事件漏斗, 由 UI 侧统一做
    /// 代次校验 + 复位选中 + toast. UI 侧不再有宽限窗概念.
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `cam`: 目标摄像头行 (uri/凭据/id 取自它)
    /// * `tap`: 视频帧输出槽 (与对讲共用, 靠 phase 互斥不并发)
    /// * `handle`: UI 事件投递句柄 (终态回调经它进事件漏斗)
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
        handle: &crate::bridge::UiHandle,
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
            // 重连次数上限: 网络不通时 ingest 连续失败 3 次就转失败终态
            // (退避 1s→2s→4s, 约 7s 后放弃), 终态由 on_terminal 回调通知
            // UI. 出流成功后 attempt 归零, 之后的网络抖动照样自愈
            reconnect: ipcam_gst::ReconnectPolicy {
                max_attempts: Some(3),
                ..Default::default()
            },
            // stream_name 本地模式用不到 (不建 webrtcsink), 随便给个唯一的
            stream_name: format!("monitor-{}", cam.id),
            on_terminal: Some(ipcam_gst::TerminalHook::new(on_monitor_terminal(
                cam.id as i32,
                handle.clone(),
            ))),
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
}

/// 终态回调: ingest 判定流已终结时在会话线程上触发.
///
/// 失败 → 把 `UiEvent::MonitorFailed` 投进修/UI 事件漏斗 (代次校验/
/// 复位选中/清画面/toast 全在 bridge 投递点与状态机里); 结束 (用户切台 /
/// 离开 / 通话抢占) → 静默. 回调只投递不碰 UI, 线程纪律由漏斗保证.
///
/// 回调在 ingest 会话线程上跑, 这里只做一次事件投递就返回.
///
/// # Arguments
///
/// * `id`: 本路摄像头 id (代次校验用, 在 UI 侧比对 monitor-selected-id)
/// * `handle`: UI 事件投递句柄
///
/// returns: impl Fn(ipcam_gst::StreamTerminal) + Send + Sync + 'static
///
/// # Examples
///
/// ```
///
/// ```
fn on_monitor_terminal(
    id: i32,
    handle: crate::bridge::UiHandle,
) -> impl Fn(ipcam_gst::StreamTerminal) + Send + Sync + 'static {
    move |terminal| {
        let ipcam_gst::StreamTerminal::Failed { error } = terminal else {
            // Ended: 用户自己停的 (切台 / 离开 / 通话抢占), 不提示
            return;
        };
        handle.post(crate::app::UiEvent::MonitorFailed { id, error });
    }
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
/// * `handle`: UI 事件投递句柄 (透传给 select 的终态回调)
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
    handle: &crate::bridge::UiHandle,
) {
    {
        let (store, monitor, tap, weak, handle) = (
            store.clone(),
            monitor.clone(),
            tap.clone(),
            ui.as_weak(),
            handle.clone(),
        );
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
            match m.select(&cam, &tap, &handle) {
                Ok(()) => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_monitor_selected_id(id);
                    }
                    // 同步构建成功不代表 RTSP 连上了 (异步 bus 事件):
                    // 失败由 ingest 侧判定终态后经 on_terminal 回调投进
                    // UI 事件漏斗, 由 bridge 投递点做 toast + 复位选中
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
    fn terminal_hook_posts_failed_and_ignores_ended() {
        // Ended 静默不投递; Failed 经 UiHandle 漏斗投递 (测试无事件循环,
        // 投递失败被吞成 warn, 不 panic). 通知文案/级别的断言在
        // app::tests::monitor_failed_yields_warn_notice_without_touching_call_state
        let ui = MainWindow::new().expect("window");
        let app: crate::bridge::SharedApp = Default::default();
        let handle = crate::bridge::UiHandle::new(ui.as_weak(), app);
        let hook = on_monitor_terminal(7, handle);
        hook(ipcam_gst::StreamTerminal::Ended);
        hook(ipcam_gst::StreamTerminal::Failed {
            error: Some("401".into()),
        });
    }
}
