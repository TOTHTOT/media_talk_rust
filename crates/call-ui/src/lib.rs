//! 通话 GUI: Slint 三态界面 (拨号/来电/通话中), SIP 信令在后台线程.
//! 本文件只做编排 (run/smoke); 各职责在子模块:
//! app=状态机, bridge=UI↔worker 桥, props=状态→属性, wiring=回调装配,
//! keyboard=屏幕键盘引擎/控制器与属性回写, persist=SQLite 装配, session=SIP worker,
//! store=库, video=视频帧, weather=天气拉取 (Open-Meteo)
slint::include_modules!();

pub mod app;
pub mod bridge;
mod camera;
mod keyboard;
mod media;
mod persist;
mod props;
pub mod session;
pub mod store;
pub mod video;
mod weather;
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
    // 天气 worker: 启动即拉, 30min 周期; 城市变更经 wake channel 唤醒
    let (weather_wake_tx, weather_wake_rx) = std::sync::mpsc::channel();
    let _weather_worker = weather::spawn(handle.clone(), weather_wake_rx);
    // 城市输入框初始值 (kv 读不到就是空串 = IP 自动定位)
    if let Ok(city) = store.kv_get(weather::KV_CITY) {
        ui.set_weather_city(city.unwrap_or_default().into());
    }
    // 和风 key 初始值 (kv 读不到就是空串 = Open-Meteo)
    if let Ok(key) = store.kv_get(weather::KV_KEY) {
        ui.set_weather_key(key.unwrap_or_default().into());
    }

    // 屏幕键盘控制器: 回调驱动引擎, 输出经 kb_flush 回写 kb-* 属性, 由聚焦的
    // KbTextField 监听 commit-seq 拍振上屏 (属性表见 ui/main.slint)
    let kb = std::rc::Rc::new(std::cell::RefCell::new(keyboard::KeyboardController::new()));
    // 中文引擎仅 linux 可用 (Task 4 接 librime); 不可用时隐藏中文模式键并回退英文
    let zh_available = kb.borrow().zh_available();
    ui.set_kb_zh_available(zh_available);
    ui.set_kb_mode(match kb.borrow().mode() {
        keyboard::Mode::Zh => 0,
        keyboard::Mode::En => 1,
        keyboard::Mode::Num => 2,
    });

    // 前端控件和后端程序通信
    let (cmd_tx, cmd_rx) = bridge::cmd_channel();
    // 视频接收的闭包 (监控查看与对讲通话共享同一个 tap)
    let tap = video::make_frame_sink(ui.as_weak());
    let worker = session::spawn(cfg, cmd_rx, handle.clone(), tap.clone())?;

    // UI 回调 → 状态机决策 → Effect 翻译 worker 命令 → 刷属性
    let monitor = std::rc::Rc::new(std::cell::RefCell::new(camera::MonitorController::new()));
    wiring::wire_callbacks(&ui, &app, &cmd_tx, &monitor);
    wiring::wire_weather_settings(&ui, &store, weather_wake_tx);
    wiring::wire_keyboard(&ui, &kb);
    persist::wire_contacts(&ui, &store);
    camera::wire_settings(&ui, &store);
    camera::wire_monitor(&ui, &store, &monitor, &tap, &handle);

    // 键盘按键回调 → 控制器 → 输出立即同步回写属性; 回调本身跑在事件循环
    // 线程, 直接 Weak::upgrade 设置即可, 无需 invoke_from_event_loop 再排队
    {
        let (kb, weak) = (kb.clone(), ui.as_weak());
        ui.on_kb_key_tapped(move |k| {
            let mut c = kb.borrow_mut();
            c.key_tapped(k.as_str());
            kb_flush(&weak, &mut c);
        });
    }
    {
        let (kb, weak) = (kb.clone(), ui.as_weak());
        ui.on_kb_backspace(move || {
            let mut c = kb.borrow_mut();
            c.backspace_tapped();
            kb_flush(&weak, &mut c);
        });
    }
    {
        let (kb, weak) = (kb.clone(), ui.as_weak());
        ui.on_kb_space(move || {
            let mut c = kb.borrow_mut();
            c.space_tapped();
            kb_flush(&weak, &mut c);
        });
    }
    {
        let (kb, weak) = (kb.clone(), ui.as_weak());
        ui.on_kb_candidate(move |i| {
            let mut c = kb.borrow_mut();
            c.candidate_tapped(i);
            kb_flush(&weak, &mut c);
        });
    }
    {
        let (kb, weak) = (kb.clone(), ui.as_weak());
        ui.on_kb_set_mode(move |m| {
            let mut c = kb.borrow_mut();
            c.mode_tapped(m);
            if let Some(ui) = weak.upgrade() {
                ui.set_kb_mode(m);
            }
            kb_flush(&weak, &mut c);
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_kb_dismiss(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_kb_visible(false);
            }
        });
    }

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

/// 键盘输出回写: 取走控制器 pending 输出并刷新 kb-* 属性; 仅当真的上屏
/// (有提交文本或请求删框) 时才拍振 kb-commit-seq, 避免模式切换等空输出
/// 触发聚焦输入框的 edited
fn kb_flush(weak: &slint::Weak<MainWindow>, kb: &mut keyboard::KeyboardController) {
    let Some(out) = kb.take_output() else { return };
    if let Some(ui) = weak.upgrade() {
        let candidates: Vec<slint::SharedString> = out.candidates.iter().map(Into::into).collect();
        ui.set_kb_preedit(out.preedit.as_str().into());
        ui.set_kb_candidates(slint::ModelRc::new(slint::VecModel::from(candidates)));
        ui.set_kb_commit_text(out.commits.join("").into());
        ui.set_kb_delete_field(out.delete_field);
        if out.delete_field || !out.commits.is_empty() {
            ui.set_kb_commit_seq(ui.get_kb_commit_seq() + 1);
        }
    }
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
