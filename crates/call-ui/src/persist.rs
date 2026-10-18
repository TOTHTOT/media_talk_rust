//! SQLite 持久化的装配: 打开库 / 启动回填 / 定时落盘 / 通讯录 CRUD 回调.
//! 数据量小, 全部同步读写, 直接在 UI 线程跑.

use std::rc::Rc;

use slint::ComponentHandle;
use tracing::error;

use crate::MainWindow;
use crate::bridge;
use crate::props;
use crate::store::Store;

/// 打开 (不存在则创建) 数据目录下的库
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

/// 通话记账落盘: 状态机只往 unsaved 队列塞, 这里 1s 批量写库.
/// 返回的 Timer 需调用方保活
pub(crate) fn start_persist_timer(app: &bridge::SharedApp, store: &Rc<Store>) -> slint::Timer {
    let (app, store) = (app.clone(), store.clone());
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(1),
        move || {
            let records = app.lock().take_unsaved_records();
            for r in &records {
                if let Err(e) = store.add_call_record(&props::record_to_row(r)) {
                    error!(err = %e, "persist call record failed");
                }
            }
        },
    );
    timer
}

/// 窗口关闭后最后一次落盘 (通话可能刚结束, 1s 定时器还没来得及写)
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
