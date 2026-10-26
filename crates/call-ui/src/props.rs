//! 状态 → slint 属性: AppState/Store 数据刷到界面属性的全部转换函数.
//! 这层只做"读出 → 转换 → set_*", 不改状态, 不写库.

use tracing::error;

use slint::ComponentHandle;

use crate::{CallLogEntry, CameraEntry, ContactEntry, MainWindow, app, store};

/// 把 AppState 字段刷到 slint 属性 (UiHandle::post 里也是这段)
pub(crate) fn sync_props(ui: &MainWindow, app: &app::AppState) {
    ui.set_phase(app.phase as i32);
    ui.set_peer(app.peer.as_str().into());
    ui.set_sip_registered(app.registered);
    ui.set_today_visitors(app.today_visitors as i32);
    // 天气: 无数据时 icon=-1, 状态栏隐藏天气块
    match &app.weather {
        Some((icon, temp)) => {
            ui.set_weather_icon(*icon);
            ui.set_weather_temp(temp.as_str().into());
        }
        None => {
            ui.set_weather_icon(-1);
            ui.set_weather_temp("".into());
        }
    }
    // 主页"最近通话": 全量传给 ScrollView 渲染, 每条 72px, 溢出自动滚动
    let entries: Vec<CallLogEntry> = app
        .call_log
        .iter()
        .map(|r| CallLogEntry {
            outgoing: r.outgoing,
            missed: r.missed,
            connected: r.connected,
            peer: r.peer.as_str().into(),
            time: r.time.as_str().into(),
            duration_s: r.duration_secs as i32,
        })
        .collect();
    ui.set_call_log(slint::ModelRc::new(slint::VecModel::from(entries)));
}

/// 主页时钟定时器: 立即一次 + 每秒刷. 只发原始时间组件 (时/分/月/日/
/// 星期索引), 格式化与翻译全在 slint 侧 @tr — 后端不感知语言,
/// 新增语言只改 lang/*.po. 返回的 Timer 需调用方保活
pub(crate) fn start_clock_timer(ui: &MainWindow) -> slint::Timer {
    let weak = ui.as_weak();
    let tick = move || {
        if let Some(ui) = weak.upgrade() {
            use chrono::{Datelike, Timelike};
            let now = chrono::Local::now();
            ui.set_clock_hour(now.hour() as i32);
            ui.set_clock_minute(now.minute() as i32);
            ui.set_date_month(now.month() as i32);
            ui.set_date_day(now.day() as i32);
            ui.set_weekday_index(now.weekday().num_days_from_monday() as i32);
        }
    };
    tick();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(1),
        tick,
    );
    timer
}

/// 从 SQLite 全量刷新通讯录到 slint 属性 (数据量小, 每次变更后整体重刷)
pub(crate) fn refresh_contacts(ui: &MainWindow, store: &store::Store) {
    match store.contacts() {
        Ok(list) => {
            let entries: Vec<ContactEntry> = list
                .iter()
                .map(|c| ContactEntry {
                    id: c.id as i32,
                    name: c.name.as_str().into(),
                    number: c.number.as_str().into(),
                })
                .collect();
            ui.set_contacts(slint::ModelRc::new(slint::VecModel::from(entries)));
        }
        Err(e) => error!(err = %e, "load contacts failed"),
    }
}

/// 从 SQLite 全量刷新摄像头列表到 slint 属性 (同 refresh_contacts 模式)
pub(crate) fn refresh_cameras(ui: &MainWindow, store: &store::Store) {
    match store.cameras() {
        Ok(list) => set_camera_entries(ui, &list),
        Err(e) => error!(err = %e, "load cameras failed"),
    }
}

/// CameraRow 列表 → slint cameras 属性 (跨线程回刷路径只带得动纯数据,
/// 与 refresh_cameras 共用转换逻辑)
pub(crate) fn set_camera_entries(ui: &MainWindow, list: &[store::CameraRow]) {
    let entries: Vec<CameraEntry> = list
        .iter()
        .map(|c| CameraEntry {
            id: c.id as i32,
            name: c.name.as_str().into(),
            uri: c.uri.as_str().into(),
            kind: kind_label(&c.kind).into(),
        })
        .collect();
    ui.set_cameras(slint::ModelRc::new(slint::VecModel::from(entries)));
}

/// kind 标签 → 显示文案 (空表示未知, 列表不显示)
fn kind_label(kind: &str) -> &'static str {
    match kind {
        "h264" => "H264",
        "h265" => "H265",
        "jpeg" => "JPEG",
        _ => "",
    }
}

/// store 行 → 状态机记录 (启动回填用); 两类型同构, 分开放是避免 store 依赖 app
pub(crate) fn row_to_record(r: &store::CallRecordRow) -> app::CallRecord {
    app::CallRecord {
        outgoing: r.outgoing,
        missed: r.missed,
        connected: r.connected,
        peer: r.peer.clone(),
        time: r.time.clone(),
        duration_secs: r.duration_secs,
    }
}

/// 状态机记录 → store 行 (落盘用)
pub(crate) fn record_to_row(r: &app::CallRecord) -> store::CallRecordRow {
    store::CallRecordRow {
        outgoing: r.outgoing,
        missed: r.missed,
        connected: r.connected,
        peer: r.peer.clone(),
        time: r.time.clone(),
        duration_secs: r.duration_secs,
    }
}
