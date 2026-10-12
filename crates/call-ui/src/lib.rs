//! 通话 GUI: Slint 三态界面 (拨号/来电/通话中), SIP 信令在后台线程.
slint::include_modules!();

/// 界面阶段, 与 main.slint 的 phase 属性一一对应
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Phase {
    Idle = 0,
    Outgoing = 1,
    Incoming = 2,
    InCall = 3,
}

/// 临时冒烟入口: 开窗验证 slint 编译和显示, 不接 SIP (Task 4 替换为 run)
pub fn smoke() -> anyhow::Result<()> {
    let ui = MainWindow::new()?;
    let weak = ui.as_weak();
    ui.on_dial(move |number| {
        // 回调触发时窗口必然存活; 万一已销毁则忽略本次拨号
        if let Some(ui) = weak.upgrade() {
            ui.set_peer(number.as_str().into());
            ui.set_phase(Phase::Outgoing as i32);
        }
    });
    ui.on_hangup(move || {});
    ui.on_accept_call(move || {});
    ui.on_reject_call(move || {});
    ui.run()?;
    Ok(())
}
