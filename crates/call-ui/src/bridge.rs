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
/// returns: (UnboundedSender<WorkerCmd>, UnboundedReceiver<WorkerCmd>) — 发送端给 UI，接收端给 worker
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

    /// 投递事件: 主线程上先过状态机, 再把字段刷到 slint 属性
    pub fn post(&self, ev: UiEvent) {
        let weak = self.weak.clone();
        let app = self.app.clone();
        if let Err(e) = slint::invoke_from_event_loop(move || {
            // 错误条: 事件直接推, 不走 sync_props (slint Timer 自动消化)
            let error_msg = match &ev {
                UiEvent::RegisterFailed(r) => Some(format!("注册失败: {r}")),
                UiEvent::Failed { reason } => Some(reason.clone()),
                _ => None,
            };
            app.lock().on_event(ev);
            if let Some(ui) = weak.upgrade() {
                if let Some(msg) = error_msg {
                    ui.set_error_text(msg.into());
                }
                crate::props::sync_props(&ui, &app.lock());
            }
        }) {
            // 事件循环已停 (窗口关闭流程中): 事件丢弃即可
            warn!(error = %e, "UI 事件投递失败, 事件循环可能已退出");
        }
    }
}
