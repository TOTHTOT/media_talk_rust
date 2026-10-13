//! 通话 GUI: Slint 三态界面 (拨号/来电/通话中), SIP 信令在后台线程.
slint::include_modules!();

pub mod app;
pub mod bridge;
mod media;
pub mod session;
pub mod video;
pub use app::Phase;
pub use session::GuiSipConfig;

/// 把 AppState 字段刷到 slint 属性 (UiHandle::post 里也是这段)
fn sync_props(ui: &MainWindow, app: &app::AppState) {
    ui.set_phase(app.phase as i32);
    ui.set_peer(app.peer.as_str().into());
    ui.set_error_text(app.error.as_str().into());
}

/// 装配 UI 回调: 四个回调同一形态 — 状态机决策出 Effect, 翻译成
/// worker 命令发出, 再把状态刷到界面属性
fn wire_callbacks(
    ui: &MainWindow,
    app: &bridge::SharedApp,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<bridge::WorkerCmd>,
) {
    /// 决策落地: 有 Effect 才发命令, 然后刷属性.
    /// 注意必须先 `let eff = app.lock().xxx();` 绑定再调本函数 —
    /// 锁守卫若作为实参表达式存活到调用结束, 这里再 lock 会死锁
    /// (parking_lot Mutex 不可重入)
    fn apply(
        weak: &slint::Weak<MainWindow>,
        app: &bridge::SharedApp,
        cmd_tx: &tokio::sync::mpsc::UnboundedSender<bridge::WorkerCmd>,
        eff: Option<app::Effect>,
    ) {
        if let Some(eff) = eff {
            let cmd = match eff {
                app::Effect::Dial(n) => bridge::WorkerCmd::Dial(n),
                app::Effect::Accept => bridge::WorkerCmd::Accept,
                app::Effect::Reject => bridge::WorkerCmd::Reject,
                app::Effect::Hangup => bridge::WorkerCmd::Hangup,
            };
            let _ = cmd_tx.send(cmd);
        }
        if let Some(ui) = weak.upgrade() {
            sync_props(&ui, &app.lock());
        }
    }

    {
        let (app, cmd_tx, weak) = (app.clone(), cmd_tx.clone(), ui.as_weak());
        ui.on_dial(move |number| {
            let eff = app.lock().dial(number.as_str().to_string());
            apply(&weak, &app, &cmd_tx, eff);
        });
    }
    {
        let (app, cmd_tx, weak) = (app.clone(), cmd_tx.clone(), ui.as_weak());
        ui.on_accept_call(move || {
            let eff = app.lock().accept();
            apply(&weak, &app, &cmd_tx, eff);
        });
    }
    {
        let (app, cmd_tx, weak) = (app.clone(), cmd_tx.clone(), ui.as_weak());
        ui.on_reject_call(move || {
            let eff = app.lock().reject();
            apply(&weak, &app, &cmd_tx, eff);
        });
    }
    {
        let (app, cmd_tx, weak) = (app.clone(), cmd_tx.clone(), ui.as_weak());
        ui.on_hangup(move || {
            let eff = app.lock().hangup();
            apply(&weak, &app, &cmd_tx, eff);
        });
    }
}

/// 启动通话 GUI: 主线程跑 slint 事件循环, SIP worker 在后台线程.
/// 窗口关闭后命令通道断开, worker 收到后 client.stop() 优雅退出
pub fn run(cfg: GuiSipConfig) -> anyhow::Result<()> {
    let ui = MainWindow::new()?;
    let app: bridge::SharedApp =
        std::sync::Arc::new(parking_lot::Mutex::new(app::AppState::default()));
    let handle = bridge::UiHandle::new(ui.as_weak(), app.clone());
    let (cmd_tx, cmd_rx) = bridge::cmd_channel();
    let tap = video::make_frame_sink(ui.as_weak());
    let worker = session::spawn(cfg, cmd_rx, handle, tap);

    // UI 回调 → 状态机决策 → Effect 翻译 worker 命令 → 刷属性
    wire_callbacks(&ui, &app, &cmd_tx);
    ui.run()?;
    // 窗口已关: 显式 drop 本作用域持有的命令发送端 (回调里的克隆随窗口
    // 销毁), 通道关闭 worker 才退出; 再 join 等它补 BYE + 注销
    // (500ms + ≤2s, 收尾有界)
    drop(cmd_tx);
    worker?
        .join()
        .map_err(|e| anyhow::anyhow!("worker thread panicked: {e:?}"))?;
    Ok(())
}

/// 临时冒烟入口: 开窗验证 slint 编译和显示, 不接 SIP (正式入口是 run)
pub fn smoke() -> anyhow::Result<()> {
    let ui = MainWindow::new()?;
    let weak = ui.as_weak();
    ui.on_dial(move |number| {
        // 回调触发时窗口必然存活; 万一已销毁则忽略本次拨号
        if let Some(ui) = weak.upgrade() {
            ui.set_peer(number.as_str().into());
            ui.set_phase(app::Phase::Outgoing as i32);
        }
    });
    ui.on_hangup(move || {});
    ui.on_accept_call(move || {});
    ui.on_reject_call(move || {});
    ui.run()?;
    Ok(())
}
