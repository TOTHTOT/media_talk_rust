//! 通话 GUI: Slint 三态界面 (拨号/来电/通话中), SIP 信令在后台线程.
slint::include_modules!();

pub mod app;
pub mod bridge;
mod media;
pub mod session;
pub mod video;
pub use app::Phase;
pub use session::GuiSipConfig;
use tracing::{error, info};

/// 把 AppState 字段刷到 slint 属性 (UiHandle::post 里也是这段)
pub(crate) fn sync_props(ui: &MainWindow, app: &app::AppState) {
    ui.set_phase(app.phase as i32);
    ui.set_peer(app.peer.as_str().into());
    ui.set_error_text(app.error.as_str().into());
    ui.set_sip_registered(app.registered);
    ui.set_today_visitors(app.today_visitors as i32);
    // 主页"最近通话"条只显示最近 2 条
    let entries: Vec<CallLogEntry> = app
        .call_log
        .iter()
        .take(2)
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

/// 设置页的媒体源可读标签
fn src_label(src: &ipcam_gst::TrackSource) -> String {
    match src {
        ipcam_gst::TrackSource::File(p) => format!("文件 {}", p.display()),
        ipcam_gst::TrackSource::Rtsp { .. } => "RTSP 相机".into(),
        ipcam_gst::TrackSource::LocalCamera { .. } => "本机摄像头".into(),
        ipcam_gst::TrackSource::Mic => "麦克风".into(),
    }
}

/// 装配 UI 回调: 四个回调同一形态 — 状态机决策出 Effect, 翻译成
/// worker 命令发出, 再把状态刷到界面属性
fn wire_callbacks(
    ui: &MainWindow,
    app: &bridge::SharedApp,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<bridge::WorkerCmd>,
) {
    /// 决策落地: 有命令才发, 然后刷属性.
    /// 注意必须先 `let cmd = app.lock().xxx();` 绑定再调本函数 —
    /// 锁守卫若作为实参表达式存活到调用结束, 这里再 lock 会死锁
    /// (parking_lot Mutex 不可重入)
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
        let (app, cmd_tx, weak) = (app.clone(), cmd_tx.clone(), ui.as_weak());
        ui.on_dial(move |number| {
            let cmd = app.lock().dial(number.as_str().to_string());
            apply(&weak, &app, &cmd_tx, cmd);
        });
    }
    {
        let (app, cmd_tx, weak) = (app.clone(), cmd_tx.clone(), ui.as_weak());
        ui.on_accept_call(move || {
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
}

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
    // 主页静态配置信息 (账号/设置页), cfg 随后 move 进 worker
    ui.set_account(format!("{}@{}", cfg.username, cfg.server.ip()).into());
    ui.set_settings_text(
        format!(
            "SIP 服务器    {}\nSIP 账号      {}\n视频源        {}\n音频源        {}\nRTP 端口      音频 {} / 视频 {}\n视频限宽      {}",
            cfg.server,
            cfg.username,
            src_label(&cfg.video_src),
            src_label(&cfg.audio_src),
            cfg.rtp_ports.audio,
            cfg.rtp_ports.video,
            if cfg.max_width > 0 {
                cfg.max_width.to_string()
            } else {
                "不限".into()
            },
        )
        .into(),
    );
    // 主页时钟: 立即一次 + 每秒刷
    let weak_clock = ui.as_weak();
    let tick = move || {
        if let Some(ui) = weak_clock.upgrade() {
            let (clock, date, status) = clock_strings();
            ui.set_clock_text(clock.into());
            ui.set_date_text(date.into());
            ui.set_status_time_text(status.into());
        }
    };
    tick();
    let clock_timer = slint::Timer::default();
    clock_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(1),
        tick,
    );
    let _clock_timer = clock_timer; // 保活到 ui.run() 返回

    let app: bridge::SharedApp =
        std::sync::Arc::new(parking_lot::Mutex::new(app::AppState::default()));
    let handle = bridge::UiHandle::new(ui.as_weak(), app.clone());
    // 前端控件和后端程序通信
    let (cmd_tx, cmd_rx) = bridge::cmd_channel();
    // 视频接收的闭包
    let tap = video::make_frame_sink(ui.as_weak());
    let worker = session::spawn(cfg, cmd_rx, handle, tap)?;

    // UI 回调 → 状态机决策 → Effect 翻译 worker 命令 → 刷属性
    wire_callbacks(&ui, &app, &cmd_tx);
    ui.run()?;
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
