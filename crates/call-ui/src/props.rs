//! 状态 → slint 属性: AppState/Store 数据刷到界面属性的全部转换函数.
//! 这层只做"读出 → 转换 → set_*", 不改状态, 不写库.

use tracing::error;

use slint::ComponentHandle;

use crate::{CallLogEntry, ContactEntry, MainWindow, app, store};

/// 把 AppState 字段刷到 slint 属性 (UiHandle::post 里也是这段)
pub(crate) fn sync_props(ui: &MainWindow, app: &app::AppState) {
    ui.set_phase(app.phase as i32);
    ui.set_peer(app.peer.as_str().into());
    ui.set_sip_registered(app.registered);
    ui.set_today_visitors(app.today_visitors as i32);
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

/// 时钟字符串: (大时钟 "12:58", 日期 "10月16日 星期四", 状态栏 "12:58  周四 10/16")
fn clock_strings() -> (String, String, String) {
    use chrono::Datelike;
    let now = chrono::Local::now();
    const WD: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    let wd = WD[now.weekday().num_days_from_monday() as usize];
    let clock = now.format("%H:%M").to_string();
    let date = format!(
        "{} {}",
        now.format("%m月%d日"),
        wd.replacen('周', "星期", 1)
    );
    let status = format!("{}  {} {}", clock, wd, now.format("%m/%d"));
    (clock, date, status)
}

/// 主页时钟定时器: 立即一次 + 每秒刷. 返回的 Timer 需调用方保活
pub(crate) fn start_clock_timer(ui: &MainWindow) -> slint::Timer {
    let weak = ui.as_weak();
    let tick = move || {
        if let Some(ui) = weak.upgrade() {
            let (clock, date, status) = clock_strings();
            ui.set_clock_text(clock.into());
            ui.set_date_text(date.into());
            ui.set_status_time_text(status.into());
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

/// 设置页的媒体源可读标签
pub(crate) fn src_label(src: &ipcam_gst::TrackSource) -> String {
    match src {
        ipcam_gst::TrackSource::File(p) => format!("文件 {}", p.display()),
        ipcam_gst::TrackSource::Rtsp { .. } => "RTSP 相机".into(),
        ipcam_gst::TrackSource::LocalCamera { .. } => "本机摄像头".into(),
        ipcam_gst::TrackSource::Mic => "麦克风".into(),
    }
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
