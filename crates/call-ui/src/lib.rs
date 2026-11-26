//! 通话 GUI: Slint 三态界面 (拨号/来电/通话中), SIP 信令在后台线程.
//! 本文件只做编排 (run/smoke); 各职责在子模块:
//! app=状态机, bridge=UI↔worker 桥, props=状态→属性, wiring=回调装配,
//! keyboard=屏幕键盘引擎/控制器与属性回写, persist=SQLite 装配, session=SIP worker,
//! store=库, video=视频帧, weather=天气拉取 (Open-Meteo)
slint::include_modules!();

pub mod app;
pub mod bridge;
mod camera;
mod ice;
mod keyboard;
mod media;
mod persist;
mod props;
mod screen;
pub mod session;
pub mod sip_settings;
pub mod store;
pub mod video;
mod weather;
mod wiring;
use anyhow::Context as _;
pub use app::Phase;
pub use session::{GuiSipConfig, GuiSipOverrides};
use tracing::info;

/// 屏幕键盘退格: Slint 字符串没有 substring/char-at, 由 Rust 删除最后一个
/// Unicode 标量. 每个窗口注册一次, 所有 KbTextField 共用.
fn install_keyboard_trim(ui: &MainWindow) {
    ui.global::<KeyboardCtx>().on_trim_last(|text| {
        let mut text = text.to_string();
        text.pop();
        text.into()
    });
}

/// 启动通话 GUI: 主线程跑 slint 事件循环, SIP worker 在后台线程.
/// 窗口关闭后命令通道断开, worker 收到后 client.stop() 优雅退出.
///
/// # Arguments
///
/// * `over`: CLI 覆盖项; 缺省字段由 "kv > 内置默认" 补全 (sip_settings::resolve)
///
/// returns: anyhow::Result<()>
///
/// # Examples
///
/// ```
///
/// ```
pub fn run(over: GuiSipOverrides) -> anyhow::Result<()> {
    info!(gui_overrides = ?over, "start gui");

    let ui = MainWindow::new()?;
    // Kit 服务必须在窗口显示前安装 (弹层定位/选择/焦点/主题全局)
    slint_kit::install(&ui);
    install_keyboard_trim(&ui);
    let _clock_timer = props::start_clock_timer(&ui); // 保活到 ui.run() 返回

    let app: bridge::SharedApp =
        std::sync::Arc::new(parking_lot::Mutex::new(app::AppState::default()));

    // SQLite 持久化 (通讯录 + 通话记录 + kv 设置)
    let store = persist::open_store()?;
    persist::backfill_call_log(&app, &store);
    // SIP 设置合成: CLI 覆盖 > kv > 内置默认 (kv 缺失播种默认), 状态栏账号
    // 信息取自合成结果, cfg 随后 move 进 SIP worker
    let cfg = sip_settings::resolve(&store, &over)?;
    ui.set_account(format!("{}@{}", cfg.username, cfg.server.ip()).into());

    let handle = bridge::UiHandle::new(ui.as_weak(), app.clone());
    // 天气: UI Timer 每 30min 触发一次异步刷新任务; 启动先拉一次.
    // 设置页改城市/key 时也直接调 weather::refresh_async, 不走 channel.
    let weather_timer = slint::Timer::default();
    weather::refresh_async(handle.clone());
    {
        let handle = handle.clone();
        weather_timer.start(slint::TimerMode::Repeated, weather::REFRESH, move || {
            weather::refresh_async(handle.clone())
        });
    }
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
    // ICE 通话 worker: 命令通道驱动 (设置页 ICE 卡片, 本体在 settings/ice_card.slint).
    // 媒体源从 GuiSipConfig 摘录 (cfg 随后 move 进 SIP worker).
    let ice_sources = ice::IceSources {
        video_src: cfg.video_src.as_video_source(),
        audio_src: cfg.audio_src.as_audio_source(),
        max_width: cfg.max_width,
    };
    let (ice_ctl, _ice_worker) =
        ice::spawn(handle.clone(), ice_sources, tap.clone()).context("spawn ice worker")?;
    // ICE 设置初始值 (kv 读不到用默认值)
    if let Ok(v) = store.kv_get(ice::KV_SIGSERVER) {
        ui.set_ice_sigserver(v.unwrap_or_else(|| ice::DEFAULT_SIGSERVER.into()).into());
    }
    if let Ok(v) = store.kv_get(ice::KV_ROOM) {
        ui.set_ice_room(v.unwrap_or_else(|| ice::DEFAULT_ROOM.into()).into());
    }
    if let Ok(v) = store.kv_get(ice::KV_STUN) {
        ui.set_ice_stun(v.unwrap_or_else(|| ice::DEFAULT_STUN.into()).into());
    }
    // SIP 设置卡初始值: resolve 合成结果的回显字符串 (设置页 SIP 卡片,
    // 本体在 settings/sip_card.slint)
    let sip_ui = sip_settings::ui_strings(&cfg);
    ui.set_sip_server(sip_ui.server.as_str().into());
    ui.set_sip_username(sip_ui.username.as_str().into());
    ui.set_sip_password(sip_ui.password.as_str().into());
    ui.set_sip_expires(sip_ui.expires.as_str().into());
    ui.set_sip_stun(sip_ui.stun.as_str().into());
    ui.set_sip_local_ip(sip_ui.local_ip.as_str().into());
    ui.set_sip_video_src(sip_ui.video_src.as_str().into());
    ui.set_sip_audio_src(sip_ui.audio_src.as_str().into());
    ui.set_sip_width_limit(sip_ui.max_width.as_str().into());
    ui.set_sip_rtp_audio(sip_ui.rtp_audio.as_str().into());
    ui.set_sip_rtp_video(sip_ui.rtp_video.as_str().into());
    ui.set_sip_ice_enabled(cfg.ice.enabled);
    let worker = session::spawn(cfg, cmd_rx, handle.clone(), tap.clone())?;

    // UI 回调 → 状态机决策 → Effect 翻译 worker 命令 → 刷属性
    let monitor = std::rc::Rc::new(std::cell::RefCell::new(camera::MonitorController::new()));
    wiring::wire_callbacks(&ui, &app, &cmd_tx, &monitor);
    wiring::wire_weather_settings(&ui, &store, handle.clone());
    ice::wire_ice(&ui, &store, &ice_ctl);
    sip_settings::wire_sip_settings(&ui, &store);
    wiring::wire_keyboard(&ui, &kb);
    // 熄屏 + 亮度: RK3566 走背光/DPMS, 其他平台只由状态栏按钮弹 toast
    let screen = std::rc::Rc::new(std::cell::RefCell::new(screen::ScreenController::new()));
    wiring::wire_screen(&ui, &screen, &store);
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
    // 停掉周期天气刷新; 若恰有一次刷新在途, 异步任务拉完自己结束
    drop(weather_timer);
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
    slint_kit::install(&ui);
    install_keyboard_trim(&ui);
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
