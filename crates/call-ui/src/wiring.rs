//! UI 回调装配: 回调 → 状态机决策 → Effect 翻译 worker 命令 → 刷属性.

use std::rc::Rc;

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

/// 天气城市设置: 写 kv (weather worker 自己读库) + 清定位缓存 +
/// 唤醒 worker 立即重定位. 留空 = IP 自动定位
pub(crate) fn wire_weather_settings(
    ui: &MainWindow,
    store: &Rc<crate::store::Store>,
    wake: std::sync::mpsc::Sender<()>,
) {
    {
        let store = store.clone();
        let wake = wake.clone();
        ui.on_set_city(move |city| {
            let city = city.trim();
            if let Err(e) = store.kv_set("weather_city", city) {
                error!(err = %e, "save weather city failed");
                return;
            }
            // 城市变了, 两家定位缓存都作废
            clear_geo_caches(&store);
            // 唤醒失败只丢一次立即重拉, 下一轮周期仍会刷, 不算错误
            let _ = wake.send(());
        });
    }
    // 和风 key: 写 kv + 清缓存 + 唤醒. key 变更时按新 key 重新定位
    let store = store.clone();
    let wake = wake.clone();
    ui.on_set_weather_key(move |key| {
        let key = key.trim();
        if let Err(e) = store.kv_set(crate::weather::KV_KEY, key) {
            error!(err = %e, "save weather key failed");
            return;
        }
        clear_geo_caches(&store);
        let _ = wake.send(());
    });
}

/// 屏幕键盘焦点登记: KbTextField 聚焦/全失焦 → 控制器 set_focus.
/// 无聚焦输入框时控制器吞掉上屏与删框指令 (见 keyboard/mod.rs)
pub(crate) fn wire_keyboard(
    ui: &MainWindow,
    kb: &Rc<std::cell::RefCell<crate::keyboard::KeyboardController>>,
) {
    let kb = kb.clone();
    ui.on_kb_field_focused(move |id| {
        kb.borrow_mut()
            .set_focus(if id >= 0 { Some(id as u32) } else { None });
    });
}

/// 清空 Open-Meteo (weather_geo) 与和风 (weather_loc) 两份定位缓存
fn clear_geo_caches(store: &crate::store::Store) {
    for k in [crate::weather::KV_GEO, crate::weather::KV_LOC] {
        if let Err(e) = store.kv_set(k, "") {
            error!(err = %e, key = k, "clear geo cache failed");
        }
    }
}
