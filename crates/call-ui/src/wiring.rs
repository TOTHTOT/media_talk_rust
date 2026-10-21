//! UI 回调装配: 回调 → 状态机决策 → Effect 翻译 worker 命令 → 刷属性.

use slint::ComponentHandle;
use tracing::error;

use crate::MainWindow;
use crate::bridge;
use crate::props::sync_props;

/// 装配 UI 回调: 四个通话回调同一形态 — 状态机决策出 Effect, 翻译成
/// worker 命令发出, 再把状态刷到界面属性
///
/// # Arguments
///
/// * `ui`: 主窗口句柄 (注册 on_* 回调)
/// * `app`: 共享状态机, 回调里决策出 Effect
/// * `cmd_tx`: UI → SIP worker 命令通道发送端
/// * `monitor`: 监控控制器 (起通话前停监控 ingest)
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
pub(crate) fn wire_callbacks(
    ui: &MainWindow,
    app: &bridge::SharedApp,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<bridge::WorkerCmd>,
    monitor: &std::rc::Rc<std::cell::RefCell<crate::camera::MonitorController>>,
) {
    /// 决策落地: 有命令才发, 然后刷属性.
    /// 注意必须先 `let cmd = app.lock().xxx();` 绑定再调本函数 —
    /// 锁守卫若作为实参表达式存活到调用结束, 这里再 lock 会死锁
    /// (parking_lot Mutex 不可重入)
    ///
    /// # Arguments
    ///
    /// * `weak`: 主窗口弱引用 (刷属性用)
    /// * `app`: 共享状态机 (刷属性时短暂加锁)
    /// * `cmd_tx`: UI → SIP worker 命令通道发送端
    /// * `cmd`: 状态机决策出的命令, None 表示无需下发
    ///
    /// returns: ()
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    fn apply(
        weak: &slint::Weak<MainWindow>,
        app: &bridge::SharedApp,
        cmd_tx: &tokio::sync::mpsc::UnboundedSender<bridge::WorkerCmd>,
        cmd: Option<bridge::WorkerCmd>,
    ) {
        if let Some(cmd) = cmd {
            if let Err(e) = cmd_tx.send(cmd) {
                error!(err = %e, "cmd send failed");
            }
        }
        if let Some(ui) = weak.upgrade() {
            sync_props(&ui, &app.lock());
        }
    }

    {
        let (app, cmd_tx, weak, monitor) =
            (app.clone(), cmd_tx.clone(), ui.as_weak(), monitor.clone());
        ui.on_dial(move |number| {
            // 监控查看与通话共用视频通路: 起通话前必须停掉监控 ingest,
            // 并复位监控页选中高亮 (stop_and_reset_ui)
            crate::camera::stop_and_reset_ui(&monitor, &weak);
            let cmd = app.lock().dial(number.as_str().to_string());
            apply(&weak, &app, &cmd_tx, cmd);
        });
    }
    {
        let (app, cmd_tx, weak, monitor) =
            (app.clone(), cmd_tx.clone(), ui.as_weak(), monitor.clone());
        ui.on_accept_call(move || {
            // 监控查看与通话共用视频通路: 起通话前必须停掉监控 ingest,
            // 并复位监控页选中高亮 (stop_and_reset_ui)
            crate::camera::stop_and_reset_ui(&monitor, &weak);
            let cmd = app.lock().accept();
            apply(&weak, &app, &cmd_tx, cmd);
        });
    }
    {
        let (app, cmd_tx, weak) = (app.clone(), cmd_tx.clone(), ui.as_weak());
        ui.on_reject_call(move || {
            let cmd = app.lock().reject();
            apply(&weak, &app, &cmd_tx, cmd);
        });
    }
    {
        let (app, cmd_tx, weak) = (app.clone(), cmd_tx.clone(), ui.as_weak());
        ui.on_hangup(move || {
            let cmd = app.lock().hangup();
            apply(&weak, &app, &cmd_tx, cmd);
        });
    }
    // 语言切换: 纯 UI 关注点, 不进状态机; 切换后 Slint 自动重算所有 @tr
    ui.on_set_language(move |lang| {
        if let Err(e) = slint::select_bundled_translation(&lang) {
            error!(lang = %lang, err = %e, "set language failed");
        }
    });
}
