//! ICE 设置页回调装配: 三个文本项写 kv, 呼叫/挂断直通控制器.

use std::rc::Rc;

use slint::ComponentHandle as _;
use tracing::error;

use crate::MainWindow;
use crate::app::{NoticeKind, NoticeLevel};
use crate::ice::{IceController, KV_ROOM, KV_SIGSERVER, KV_STUN};
use crate::store::Store;

/// 装配 ICE 设置回调: 三个文本项写 kv (下次呼叫生效), 呼叫/挂断直通控制器.
///
/// # Arguments
///
/// * `ui`: 主窗口
/// * `store`: kv 持久化
/// * `ctl`: ICE 控制器
pub(crate) fn wire_ice(ui: &MainWindow, store: &Rc<Store>, ctl: &IceController) {
    /// 设置保存成功的 toast.
    fn toast_saved(ui: &MainWindow) {
        ui.invoke_notify(
            NoticeLevel::Info as i32,
            NoticeKind::Raw as i32,
            "已保存, 下次呼叫生效".into(),
        );
    }

    // sigserver 地址
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_ice_sigserver(move |v| {
            if let Err(e) = store.kv_set(KV_SIGSERVER, v.trim()) {
                error!(err = %e, "save ice sigserver failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 房间名
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_ice_room(move |v| {
            if let Err(e) = store.kv_set(KV_ROOM, v.trim()) {
                error!(err = %e, "save ice room failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // STUN URI (留空 = 仅 host 候选)
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_ice_stun(move |v| {
            if let Err(e) = store.kv_set(KV_STUN, v.trim()) {
                error!(err = %e, "save ice stun failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 呼叫 / 挂断
    {
        let ctl = ctl.clone();
        ui.on_ice_call(move || ctl.call());
    }
    {
        let ctl = ctl.clone();
        ui.on_ice_hangup(move || ctl.hangup());
    }
}
