//! 通话 GUI: Slint 三态界面 (拨号/来电/通话中), SIP 信令在后台线程.
//! 本文件只做编排 (run/smoke); 各职责在子模块:
//! app=状态机, bridge=UI↔worker 桥, props=状态→属性, wiring=回调装配,
//! persist=SQLite 装配, session=SIP worker, store=库, video=视频帧
slint::include_modules!();

pub mod app;
pub mod bridge;
mod camera;
mod media;
mod persist;
mod props;
pub mod session;
pub mod store;
pub mod video;
mod wiring;
pub use app::Phase;
pub use session::GuiSipConfig;
use tracing::info;

/// 启动通话 GUI: 主线程跑 slint 事件循环, SIP worker 在后台线程.
/// 窗口关闭后命令通道断开, worker 收到后 client.stop() 优雅退出.
///
/// # Arguments
///
/// * `cfg`: SIP 服务器账号与本机媒体源配置
///
/// returns: anyhow::Result<()>
///
/// # Examples
///
/// ```
///
/// ```
pub fn run(cfg: GuiSipConfig) -> anyhow::Result<()> {
    info!(gui_cfg = ?cfg,"start gui");

    let ui = MainWindow::new()?;
    // 状态栏账号信息, cfg 随后 move 进 worker
    ui.set_account(format!("{}@{}", cfg.username, cfg.server.ip()).into());
    let _clock_timer = props::start_clock_timer(&ui); // 保活到 ui.run() 返回

    let app: bridge::SharedApp =
        std::sync::Arc::new(parking_lot::Mutex::new(app::AppState::default()));

    // SQLite 持久化 (通讯录 + 通话记录)
    let store = persist::open_store()?;
    persist::backfill_call_log(&app, &store);
    let _persist_timer = persist::start_persist_timer(&app, &store); // 保活

    let handle = bridge::UiHandle::new(ui.as_weak(), app.clone());
    // 前端控件和后端程序通信
    let (cmd_tx, cmd_rx) = bridge::cmd_channel();
    // 视频接收的闭包 (监控查看与对讲通话共享同一个 tap)
    let tap = video::make_frame_sink(ui.as_weak());
    let worker = session::spawn(cfg, cmd_rx, handle.clone(), tap.clone())?;

    // UI 回调 → 状态机决策 → Effect 翻译 worker 命令 → 刷属性
    let monitor = std::rc::Rc::new(std::cell::RefCell::new(camera::MonitorController::new()));
    wiring::wire_callbacks(&ui, &app, &cmd_tx, &monitor);
    persist::wire_contacts(&ui, &store);
    camera::wire_settings(&ui, &store);
    camera::wire_monitor(&ui, &store, &monitor, &tap, &handle);

    ui.run()?;
    persist::drain_unsaved(&app, &store);
    // 窗口已关: 显式 drop 本作用域持有的命令发送端 (回调里的克隆随窗口
    // 销毁), 通道关闭 worker 才退出; 再 join 等它补 BYE + 注销
    // (500ms + ≤2s, 收尾有界)
    drop(cmd_tx);
    worker
        .join()
        .map_err(|e| anyhow::anyhow!("worker thread panicked: {e:?}"))?;
    Ok(())
}

/// 临时冒烟入口: 开窗验证 slint 编译和显示, 不接 SIP (正式入口是 run).
///
/// returns: anyhow::Result<()>
///
/// # Examples
///
/// ```
///
/// ```
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
