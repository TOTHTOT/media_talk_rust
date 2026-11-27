//! UI 回调装配: 回调 → 状态机决策 → Effect 翻译 worker 命令 → 刷属性.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use slint::ComponentHandle;
use tracing::{error, info, warn};

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
        // 本地挂断/拒接等动作也可能产生通话记录, 立即落盘
        crate::persist::flush_async(app);
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

/// 天气城市设置: 写 kv (刷新线程自己读库) + 清定位缓存 + 立即触发一次刷新.
/// 留空 = IP 自动定位
pub(crate) fn wire_weather_settings(
    ui: &MainWindow,
    store: &Rc<crate::store::Store>,
    handle: crate::bridge::UiHandle,
) {
    {
        let store = store.clone();
        let handle = handle.clone();
        ui.on_set_city(move |city| {
            let city = city.trim();
            if let Err(e) = store.kv_set("weather_city", city) {
                error!(err = %e, "save weather city failed");
                return;
            }
            // 城市变了, 两家定位缓存都作废
            clear_geo_caches(&store);
            crate::weather::refresh_async(handle.clone());
        });
    }
    // 和风 key: 写 kv + 清缓存 + 立即刷新. key 变更时按新 key 重新定位
    let store = store.clone();
    let handle = handle.clone();
    ui.on_set_weather_key(move |key| {
        let key = key.trim();
        if let Err(e) = store.kv_set(crate::weather::KV_KEY, key) {
            error!(err = %e, "save weather key failed");
            return;
        }
        clear_geo_caches(&store);
        crate::weather::refresh_async(handle.clone());
    });
}

/// 屏幕键盘焦点门控: slint 官方 TextInputInterface.text-input-focused 镜像
/// → 控制器 set_focus (布尔, 方案 D 后无逐字段身份). 无聚焦输入框时控制器
/// 吞掉上屏与删框指令 (见 keyboard/mod.rs)
pub(crate) fn wire_keyboard(
    ui: &MainWindow,
    kb: &Rc<std::cell::RefCell<crate::keyboard::KeyboardController>>,
) {
    let kb = kb.clone();
    ui.on_kb_any_field_focused(move |focused| {
        tracing::debug!(focused, "kb any-field focus changed");
        kb.borrow_mut().set_focus(focused);
    });
}

/// 熄屏开关 + 亮度: RK3566 上做真实硬件开关, 其他平台状态栏电源键只弹 toast.
/// 硬件写失败时回滚 screen-off 并走通知管线提示, 避免界面显示黑屏但
/// 背光其实没关 (或反过来).
pub(crate) fn wire_screen(
    ui: &MainWindow,
    screen: &Rc<std::cell::RefCell<crate::screen::ScreenController>>,
    store: &Rc<crate::store::Store>,
) {
    ui.set_screen_control_available(screen.borrow().supported());

    // 自动熄屏计时用单调时钟: NTP 校时/手动改时间不会把空闲时间算成几小时.
    // evdev 线程只更新原子活动时间; 是否到点、是否允许熄屏都在 UI 线程判断.
    let epoch = Instant::now();
    let activity = Arc::new(AtomicU64::new(0));

    apply_saved_brightness(ui, screen, store);
    wire_brightness(ui, screen, store, screen.borrow().brightness_available());
    wire_power_button(ui, screen, &activity, epoch);
    wire_screen_wake(ui, screen, &activity, epoch);

    let sleep_timeout = load_sleep_timeout(ui, store);
    wire_sleep_timeout(ui, store, &sleep_timeout, &activity, epoch);
    wire_sleep_tick(ui, screen, &sleep_timeout, &activity, epoch);

    if screen.borrow().supported() {
        spawn_activity_tracker(activity, epoch);
    }
}

/// 启动时恢复 kv 中的亮度; 没有记录或平台不支持时只回显当前硬件值.
fn apply_saved_brightness(
    ui: &MainWindow,
    screen: &Rc<std::cell::RefCell<crate::screen::ScreenController>>,
    store: &Rc<crate::store::Store>,
) {
    if !screen.borrow().brightness_available() {
        return;
    }
    let current = screen.borrow().brightness_percent().unwrap_or(80);
    let saved = store
        .kv_get(crate::screen::KV_BRIGHTNESS)
        .ok()
        .flatten()
        .and_then(|s| s.parse::<u32>().ok());
    match saved {
        Some(saved) => {
            let target = saved.clamp(crate::screen::BRIGHTNESS_MIN_PERCENT, 100);
            match screen.borrow_mut().set_brightness_percent(target) {
                Ok(()) => ui.set_screen_brightness(target as f32),
                Err(e) => {
                    error!(err = %e, target, "apply saved brightness failed");
                    ui.set_screen_brightness(current as f32);
                }
            }
        }
        // 没有用户记录时沿用系统当前亮度, 不主动写硬件
        None => ui.set_screen_brightness(current as f32),
    }
}

/// 亮度滑块: 仅平台支持时写硬件; 不支持时松手提示, 不写.
fn wire_brightness(
    ui: &MainWindow,
    screen: &Rc<std::cell::RefCell<crate::screen::ScreenController>>,
    store: &Rc<crate::store::Store>,
    brightness_available: bool,
) {
    let (screen, store, weak) = (screen.clone(), store.clone(), ui.as_weak());
    ui.on_set_screen_brightness(move |value| {
        let Some(ui) = weak.upgrade() else { return };
        if !brightness_available {
            ui.invoke_notify(
                crate::app::NoticeLevel::Warn as i32,
                crate::app::NoticeKind::BrightnessUnsupported as i32,
                "".into(),
            );
            return;
        }
        let percent = value
            .clamp(crate::screen::BRIGHTNESS_MIN_PERCENT as f32, 100.0)
            .round() as u32;
        match screen.borrow_mut().set_brightness_percent(percent) {
            Ok(()) => {
                ui.set_screen_brightness(percent as f32);
                if let Err(e) = store.kv_set(crate::screen::KV_BRIGHTNESS, &percent.to_string()) {
                    error!(err = %e, percent, "save brightness failed");
                }
            }
            Err(e) => {
                error!(err = %e, percent, "set brightness failed");
                let current = screen.borrow().brightness_percent().unwrap_or(percent);
                ui.set_screen_brightness(current as f32);
                ui.invoke_notify(
                    crate::app::NoticeLevel::Warn as i32,
                    crate::app::NoticeKind::ScreenFailed as i32,
                    e.to_string().into(),
                );
            }
        }
    });
}

fn wire_power_button(
    ui: &MainWindow,
    screen: &Rc<std::cell::RefCell<crate::screen::ScreenController>>,
    activity: &Arc<AtomicU64>,
    epoch: Instant,
) {
    let (screen, weak, activity) = (screen.clone(), ui.as_weak(), activity.clone());
    ui.on_screen_power_pressed(move || {
        let target = !screen.borrow().is_off();
        let result = screen.borrow_mut().set_off(target);
        let Some(ui) = weak.upgrade() else { return };
        match result {
            Ok(()) => {
                ui.set_screen_off(target);
                if !target {
                    // 手动开屏后重新计时, 否则可能马上又被自动熄屏.
                    activity.store(elapsed_secs(epoch), Ordering::SeqCst);
                }
            }
            Err(e) => {
                error!(err = %e, "screen power toggle failed");
                ui.set_screen_off(screen.borrow().is_off());
                ui.invoke_notify(
                    crate::app::NoticeLevel::Warn as i32,
                    crate::app::NoticeKind::ScreenFailed as i32,
                    e.to_string().into(),
                );
            }
        }
    });
}

fn wire_screen_wake(
    ui: &MainWindow,
    screen: &Rc<std::cell::RefCell<crate::screen::ScreenController>>,
    activity: &Arc<AtomicU64>,
    epoch: Instant,
) {
    let (screen, weak, activity) = (screen.clone(), ui.as_weak(), activity.clone());
    ui.on_screen_wake(move || match screen.borrow_mut().set_off(false) {
        Ok(()) => {
            // 唤醒即算活动, 不依赖 evdev 是否及时读到这次触摸.
            activity.store(elapsed_secs(epoch), Ordering::SeqCst);
            if let Some(ui) = weak.upgrade() {
                ui.set_screen_off(false);
            }
        }
        Err(e) => {
            // 保持遮罩, 用户可继续点击重试; 提示走通知管线
            error!(err = %e, "screen wake failed");
            if let Some(ui) = weak.upgrade() {
                ui.invoke_notify(
                    crate::app::NoticeLevel::Warn as i32,
                    crate::app::NoticeKind::ScreenFailed as i32,
                    e.to_string().into(),
                );
            }
        }
    });
}

/// 读 kv 超时秒值并规范化到选项表, 返回供心跳回调共享的原子值.
fn load_sleep_timeout(ui: &MainWindow, store: &Rc<crate::store::Store>) -> Arc<AtomicU64> {
    let saved_secs = store
        .kv_get(crate::screen::KV_SLEEP_TIMEOUT)
        .ok()
        .flatten()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(SLEEP_CHOICES[DEFAULT_SLEEP_INDEX]);
    let init_index = sleep_index_for(saved_secs);
    let init_secs = SLEEP_CHOICES[init_index];
    ui.set_sleep_timeout_index(init_index as i32);
    if saved_secs != init_secs {
        // 旧版本/手改的 kv 值可能不在选项表里; 回写规范化值, 保证 UI 与定时器一致.
        if let Err(e) = store.kv_set(crate::screen::KV_SLEEP_TIMEOUT, &init_secs.to_string()) {
            error!(err = %e, secs = init_secs, "normalize sleep timeout failed");
        }
    }
    Arc::new(AtomicU64::new(init_secs))
}

/// 设置页下拉: 先落盘再改内存/UI; 失败恢复原选项并重新计时.
fn wire_sleep_timeout(
    ui: &MainWindow,
    store: &Rc<crate::store::Store>,
    sleep_timeout: &Arc<AtomicU64>,
    activity: &Arc<AtomicU64>,
    epoch: Instant,
) {
    let (store, weak, sleep_timeout, activity) = (
        store.clone(),
        ui.as_weak(),
        sleep_timeout.clone(),
        activity.clone(),
    );
    ui.on_set_sleep_timeout_index(move |index| {
        let index = (index.max(0) as usize).min(SLEEP_CHOICES.len() - 1);
        let secs = SLEEP_CHOICES[index];
        let Some(ui) = weak.upgrade() else { return };
        let previous = ui.get_sleep_timeout_index();
        // 先落盘再改内存/UI; 失败时把下拉恢复原选项.
        if let Err(e) = store.kv_set(crate::screen::KV_SLEEP_TIMEOUT, &secs.to_string()) {
            error!(err = %e, secs, "save sleep timeout failed");
            ui.set_sleep_timeout_index(previous);
            return;
        }
        sleep_timeout.store(secs, Ordering::Relaxed);
        activity.store(elapsed_secs(epoch), Ordering::SeqCst);
        ui.set_sleep_timeout_index(index as i32);
        ui.invoke_notify(
            crate::app::NoticeLevel::Info as i32,
            crate::app::NoticeKind::Raw as i32,
            "已保存".into(),
        );
    });
}

/// UI 线程 1s 心跳: 空闲到点才熄屏; 通话/来电/去电/监控页保持亮屏.
fn wire_sleep_tick(
    ui: &MainWindow,
    screen: &Rc<std::cell::RefCell<crate::screen::ScreenController>>,
    sleep_timeout: &Arc<AtomicU64>,
    activity: &Arc<AtomicU64>,
    epoch: Instant,
) {
    let (weak, screen, sleep_timeout, activity) = (
        ui.as_weak(),
        screen.clone(),
        sleep_timeout.clone(),
        activity.clone(),
    );
    ui.on_sleep_tick(move || {
        let Some(ui) = weak.upgrade() else { return };
        let secs = sleep_timeout.load(Ordering::Relaxed);
        if secs == 0 || ui.get_screen_off() {
            return;
        }
        // 通话中/来电/去电/监控页保持亮屏; 同时刷新活动时间, 避免退出这些
        // 页面后因为屏幕保持期间没触摸而立刻熄屏.
        if ui.get_phase() != 0 || ui.get_page() == 3 {
            activity.store(elapsed_secs(epoch), Ordering::SeqCst);
            return;
        }
        let idle = elapsed_secs(epoch).saturating_sub(activity.load(Ordering::SeqCst));
        if idle < secs {
            return;
        }
        match screen.borrow_mut().set_off(true) {
            Ok(()) => {
                ui.set_screen_off(true);
                info!(idle_s = idle, "auto screen off");
            }
            Err(e) => {
                error!(err = %e, idle_s = idle, "auto screen off failed");
                // 失败后重新计时, 避免每秒重试刷屏; 下一个超时周期再试.
                activity.store(elapsed_secs(epoch), Ordering::SeqCst);
                ui.invoke_notify(
                    crate::app::NoticeLevel::Warn as i32,
                    crate::app::NoticeKind::ScreenFailed as i32,
                    e.to_string().into(),
                );
            }
        }
    });
}

// 选项表: 秒值 ↔ UI 下标. 0 = 永不.
const SLEEP_CHOICES: [u64; 6] = [0, 15, 30, 60, 120, 300];
const DEFAULT_SLEEP_INDEX: usize = 2;

fn sleep_index_for(secs: u64) -> usize {
    SLEEP_CHOICES
        .iter()
        .position(|&v| v == secs)
        .unwrap_or(DEFAULT_SLEEP_INDEX)
}

/// 活动追踪: 轮询 /dev/input/event* 的 evdev 事件流, 只把键/相对/绝对轴事件
/// 算作用户活动; 每设备一线程阻塞读. 非 Linux 平台没有 /dev/input, 自然空跑
/// (自动熄屏只在 screen.supported() 时启用).
fn spawn_activity_tracker(activity: Arc<AtomicU64>, epoch: Instant) {
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        warn!("evdev input unavailable; idle detection will ignore input");
        return;
    };
    let mut opened = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("event") {
            continue;
        }
        let Ok(file) = std::fs::OpenOptions::new().read(true).open(entry.path()) else {
            continue;
        };
        opened += 1;
        spawn_evdev_reader(name, file, activity.clone(), epoch);
    }
    if opened == 0 {
        warn!("no evdev event devices opened; idle detection will ignore input");
    }
}

/// 单设备阻塞读线程: 只把用户输入事件刷新到共享活动时间.
fn spawn_evdev_reader(name: String, file: std::fs::File, activity: Arc<AtomicU64>, epoch: Instant) {
    use std::io::Read as _;
    std::thread::Builder::new()
        .name(format!("evdev-{name}"))
        .spawn(move || {
            let mut buf = [0u8; EVENT_SIZE * 16];
            loop {
                match (&file).read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if has_user_input(&buf[..n]) {
                            activity.store(elapsed_secs(epoch), Ordering::SeqCst);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .ok();
}

const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

#[cfg(target_pointer_width = "64")]
const EVENT_SIZE: usize = 24;
#[cfg(target_pointer_width = "64")]
const EVENT_TYPE_OFFSET: usize = 16;
#[cfg(not(target_pointer_width = "64"))]
const EVENT_SIZE: usize = 16;
#[cfg(not(target_pointer_width = "64"))]
const EVENT_TYPE_OFFSET: usize = 8;

/// evdev 的 input_event: timeval + type + code + value. 只看 event type,
/// 过滤 EV_SYN/EV_MSC 等非用户操作, 避免设备心跳事件让屏幕永不熄灭.
fn has_user_input(buf: &[u8]) -> bool {
    buf.chunks_exact(EVENT_SIZE).any(|event| {
        let ty = u16::from_ne_bytes([event[EVENT_TYPE_OFFSET], event[EVENT_TYPE_OFFSET + 1]]);
        ty == EV_KEY || ty == EV_REL || ty == EV_ABS
    })
}

fn elapsed_secs(epoch: Instant) -> u64 {
    epoch.elapsed().as_secs()
}

/// 清空 Open-Meteo (weather_geo) 与和风 (weather_loc) 两份定位缓存
fn clear_geo_caches(store: &crate::store::Store) {
    for k in [crate::weather::KV_GEO, crate::weather::KV_LOC] {
        if let Err(e) = store.kv_set(k, "") {
            error!(err = %e, key = k, "clear geo cache failed");
        }
    }
}

#[cfg(test)]
mod sleep_tests {
    use super::*;

    #[test]
    fn sleep_index_normalizes_unknown_values() {
        assert_eq!(sleep_index_for(45), DEFAULT_SLEEP_INDEX);
        assert_eq!(SLEEP_CHOICES[sleep_index_for(45)], 30);
        assert_eq!(sleep_index_for(120), 4);
    }

    #[test]
    fn evdev_filters_non_input_events() {
        let event = |ty: u16| {
            let mut bytes = [0u8; EVENT_SIZE];
            bytes[EVENT_TYPE_OFFSET..EVENT_TYPE_OFFSET + 2].copy_from_slice(&ty.to_ne_bytes());
            bytes
        };
        assert!(!has_user_input(&event(0x00))); // EV_SYN
        assert!(has_user_input(&event(EV_KEY)));
        assert!(has_user_input(&event(EV_REL)));
        assert!(has_user_input(&event(EV_ABS)));
    }
}
