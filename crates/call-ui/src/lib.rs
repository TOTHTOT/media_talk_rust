//! 通话 GUI: Slint 三态界面 (拨号/来电/通话中), SIP 信令在后台线程.
slint::include_modules!();

pub mod app;
pub mod bridge;
mod media;
pub mod session;
pub mod video;
pub use app::Phase;
pub use session::GuiSipConfig;

/// 启动通话 GUI: 主线程跑 slint 事件循环, SIP worker 在后台线程.
/// 窗口关闭后命令通道断开, worker 收到后 client.stop() 优雅退出
pub fn run(cfg: GuiSipConfig) -> anyhow::Result<()> {
    let ui = MainWindow::new()?;
    let app: bridge::SharedApp =
        std::sync::Arc::new(parking_lot::Mutex::new(app::AppState::default()));
    let handle = bridge::UiHandle::new(ui.as_weak(), app.clone());
    let (cmd_tx, cmd_rx) = bridge::cmd_channel();
    let tap = video::make_frame_sink(ui.as_weak());
    session::spawn(cfg, cmd_rx, handle, tap);

    // 把 AppState 字段刷到 slint 属性 (UiHandle::post 里也是这段)
    fn sync_props(ui: &MainWindow, app: &app::AppState) {
        ui.set_phase(app.phase as i32);
        ui.set_peer(app.peer.as_str().into());
        ui.set_error_text(app.error.as_str().into());
    }

    // UI 回调 → 状态机决策 → Effect 翻译 worker 命令 → 刷属性
    {
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        let weak = ui.as_weak();
        ui.on_dial(move |number| {
            let eff = app.lock().dial(number.as_str().to_string());
            if let Some(app::Effect::Dial(n)) = eff {
                let _ = cmd_tx.send(bridge::WorkerCmd::Dial(n));
            }
            if let Some(ui) = weak.upgrade() {
                sync_props(&ui, &app.lock());
            }
        });
    }
    {
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        let weak = ui.as_weak();
        ui.on_accept_call(move || {
            if app.lock().accept().is_some() {
                let _ = cmd_tx.send(bridge::WorkerCmd::Accept);
            }
            if let Some(ui) = weak.upgrade() {
                sync_props(&ui, &app.lock());
            }
        });
    }
    {
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        let weak = ui.as_weak();
        ui.on_reject_call(move || {
            if app.lock().reject().is_some() {
                let _ = cmd_tx.send(bridge::WorkerCmd::Reject);
            }
            if let Some(ui) = weak.upgrade() {
                sync_props(&ui, &app.lock());
            }
        });
    }
    {
        let app = app.clone();
        let cmd_tx = cmd_tx.clone();
        let weak = ui.as_weak();
        ui.on_hangup(move || {
            if app.lock().hangup().is_some() {
                let _ = cmd_tx.send(bridge::WorkerCmd::Hangup);
            }
            if let Some(ui) = weak.upgrade() {
                sync_props(&ui, &app.lock());
            }
        });
    }
    ui.run()?;
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
