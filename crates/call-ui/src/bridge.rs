//! GUI↔SIP worker 桥: 命令 (UI→worker) 走 tokio unbounded channel
//! (slint 回调是同步上下文, unbounded send 不阻塞); 事件 (worker→UI)
//! 走 slint::invoke_from_event_loop 回主线程更新 AppState 和属性.

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::warn;

use crate::MainWindow;
use crate::app::AppState;
use crate::app::UiEvent;

/// UI → SIP worker 命令
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum WorkerCmd {
    Dial(String),
    Hangup,
    Accept,
    Reject,
}

/// UI → SIP worker 命令通道工厂.
///
/// returns: (UnboundedSender<WorkerCmd>, UnboundedReceiver<WorkerCmd>) — 发送端给 UI, 接收端给 worker
///
/// # Examples
///
/// ```
///
/// ```
pub fn cmd_channel() -> (UnboundedSender<WorkerCmd>, UnboundedReceiver<WorkerCmd>) {
    unbounded_channel()
}

/// AppState 的共享句柄. brief 原型是 Rc<RefCell<AppState>>, 但 UiHandle
/// 要跨线程进 worker 及其 tokio 任务, invoke_from_event_loop 的闭包也
/// 要求 Send —— Rc 过不了编译, 换成 Arc<parking_lot::Mutex> (call-ui
/// 已依赖 parking_lot). 锁只在 UI 线程的 invoke 闭包里短暂持有.
pub type SharedApp = Arc<Mutex<AppState>>;

/// worker → UI 回投: 任何线程可调, 排队进 slint 事件循环
#[derive(Clone)]
pub struct UiHandle {
    weak: slint::Weak<MainWindow>,
    app: SharedApp,
}

impl UiHandle {
    pub fn new(weak: slint::Weak<MainWindow>, app: SharedApp) -> Self {
        Self { weak, app }
    }

    /// 投递事件: 主线程上先过状态机, 状态机返回的通知进通知管线
    /// (slint 侧按 kind 组文案/按 level 选 toast 或错误条), 再把字段
    /// 刷到 slint 属性.
    /// 所有后台 → 前端的失败/状态上报统一走这一个漏斗: 新事件源
    /// 只加 UiEvent 变体, 不要自己开回调小灶
    pub fn post(&self, ev: UiEvent) {
        let weak = self.weak.clone();
        let app = self.app.clone();
        let ev_clone = ev.clone();
        // 通话终结事件: 接收器随后停止, 但 video-frame 属性会残留最后一
        // 帧, 回主线程时一并清掉 (监控画面不走这里, 停监控在 camera.rs)
        let call_ended = matches!(ev, UiEvent::Terminated { .. } | UiEvent::Failed { .. });
        // 监控失败事件: 代次校验与复位依赖 monitor-selected-id 属性,
        // 只能在这里 (UI 线程 + 窗口句柄) 做, 不进状态机
        let monitor_failed_id = match ev {
            UiEvent::MonitorFailed { id, .. } => Some(id),
            _ => None,
        };
        if let Err(e) = slint::invoke_from_event_loop(move || {
            if let Some(id) = monitor_failed_id {
                let Some(ui) = weak.upgrade() else { return };
                if ui.get_monitor_selected_id() != id {
                    return; // 已切台/离开/被通话抢占, 过期事件静默丢弃
                }
                ui.set_monitor_selected_id(-1);
                crate::video::clear_frame(&ui);
            }
            let notice = app.lock().on_event(ev);
            if let Some(ui) = weak.upgrade() {
                if let Some(n) = notice {
                    ui.invoke_notify(
                        n.level as i32,
                        n.kind as i32,
                        n.detail.unwrap_or_default().into(),
                    );
                }
                crate::props::sync_props(&ui, &app.lock());
                if call_ended {
                    crate::video::clear_frame(&ui);
                }
            }
        }) {
            // 事件循环已停 (窗口关闭流程中): 事件丢弃即可
            warn!(error = %e, event = ?ev_clone, "invoke event failed");
        }
    }
}
