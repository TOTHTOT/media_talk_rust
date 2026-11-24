//! SQLite 持久化的装配: 打开库 / 启动回填 / 通话结束落盘 / 通讯录 CRUD 回调.
//! 数据量小, 默认在 UI 线程同步读写; 通话记录在事件处理后立即交给一次性线程写库.

use std::rc::Rc;

use slint::ComponentHandle;
use tracing::error;

use crate::MainWindow;
use crate::bridge;
use crate::props;
use crate::store::Store;

/// 打开 (不存在则创建) 数据目录下的库
///
/// returns: anyhow::Result<Rc<Store>> — 已打开的库句柄; Err 是 SQLite 打开失败
///
/// # Examples
///
/// ```
///
/// ```
pub(crate) fn open_store() -> anyhow::Result<Rc<Store>> {
    Ok(Rc::new(Store::open()?))
}

/// 启动回填最近通话 (库里新的在前, 与 call_log 顺序一致)
pub(crate) fn backfill_call_log(app: &bridge::SharedApp, store: &Store) {
    match store.recent_call_records(20) {
        Ok(rows) => {
            app.lock().call_log = rows.iter().map(props::row_to_record).collect();
        }
        Err(e) => error!(err = %e, "load call records failed"),
    }
}

/// 通话记账立即落盘: 取走状态机的 unsaved 记录, 交给一次性线程写库.
/// 状态机所在线程 (UI / 事件闭包) 调用, 不阻塞界面.
pub(crate) fn flush_async(app: &bridge::SharedApp) {
    let records = app.lock().take_unsaved_records();
    if records.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let store = match Store::open() {
            Ok(store) => store,
            Err(e) => {
                error!(err = %e, "open store for call records failed");
                return;
            }
        };
        for r in &records {
            if let Err(e) = store.add_call_record(&props::record_to_row(r)) {
                error!(err = %e, "persist call record failed");
            }
        }
    });
}

/// 窗口关闭后最后一次落盘 (覆盖来不及 flush 的残留记录)
pub(crate) fn drain_unsaved(app: &bridge::SharedApp, store: &Store) {
    for r in &app.lock().take_unsaved_records() {
        if let Err(e) = store.add_call_record(&props::record_to_row(r)) {
            error!(err = %e, "final persist call record failed");
        }
    }
}

/// 通讯录 CRUD 回调: 操作失败后都全量重刷列表 (失败也刷, 界面与库保持一致)
pub(crate) fn wire_contacts(ui: &MainWindow, store: &Rc<Store>) {
    props::refresh_contacts(ui, store);
    {
        let (store, weak) = (store.clone(), ui.as_weak());
        ui.on_add_contact(move |name, number| {
            if let Err(e) = store.add_contact(&name, &number) {
                error!(err = %e, "add contact failed");
            }
            if let Some(ui) = weak.upgrade() {
                props::refresh_contacts(&ui, &store);
            }
        });
    }
    {
        let (store, weak) = (store.clone(), ui.as_weak());
        ui.on_update_contact(move |id, name, number| {
            if let Err(e) = store.update_contact(id as i64, &name, &number) {
                error!(err = %e, "update contact failed");
            }
            if let Some(ui) = weak.upgrade() {
                props::refresh_contacts(&ui, &store);
            }
        });
    }
    {
        let (store, weak) = (store.clone(), ui.as_weak());
        ui.on_delete_contact(move |id| {
            if let Err(e) = store.delete_contact(id as i64) {
                error!(err = %e, "delete contact failed");
            }
            if let Some(ui) = weak.upgrade() {
                props::refresh_contacts(&ui, &store);
            }
        });
    }
}
