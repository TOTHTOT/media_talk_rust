//! 全进程统一的关停信号层。三条纪律：
//! 1. 只有二进制（这里）碰 OS 信号，库 crate 一律收 `CancellationToken`
//! 2. cancel 语义 = 停止接新活 + 快速收尾，不是立即死
//! 3. 每个子系统的 run()/serve() 必须在收尾完成后 resolve，
//!    由调用方等待（带硬 deadline）

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// 安装全进程唯一的信号监听，返回 root token 分发给各子系统。
///
/// - 第一次 SIGINT（Ctrl+C）或 SIGTERM（systemd `systemctl stop`）：
///   cancel root token，各子系统开始优雅收尾
/// - 第二次信号：用户没耐心了，`process::exit(2)` 强退，
///   保证任何卡死的子系统都拖不住关停
pub fn install() -> CancellationToken {
    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        wait_signal().await;
        info!("shutdown signal received, graceful stop (signal again to force)");
        t.cancel();
        wait_signal().await;
        warn!("second shutdown signal, forcing exit");
        std::process::exit(2);
    });
    token
}

/// 等待一次关停信号：SIGINT 全平台，SIGTERM 仅 Unix（systemd 停服走它）
async fn wait_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
