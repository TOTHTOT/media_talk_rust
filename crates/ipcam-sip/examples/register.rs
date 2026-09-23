//! 实机注册冒烟工具：向真实 SIP 服务器注册并循环续期，
//! Ctrl+C 时 best-effort 注销（Expires:0）后退出。
//!
//! ```bash
//! cargo run -p ipcam-sip --example register                    # 全默认
//! cargo run -p ipcam-sip --example register -- 192.168.1.17:5062 -u 1001 -p changeme -e 60
//! ```

use anyhow::Result;
use clap::Parser;
use ipcam_sip::{SipClient, SipClientConfig};
use std::net::{SocketAddr, SocketAddrV4};
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(Parser)]
#[command(about = "SIP 实机注册冒烟：注册 → 循环续期 → Ctrl+C 注销退出")]
struct Args {
    /// SIP 服务器地址
    #[arg(default_value = "192.168.1.17:5062")]
    server: SocketAddr,
    /// 注册账号（同时作为用户名）
    #[arg(short, long, default_value = "9999")]
    username: String,
    /// 注册密码
    #[arg(short, long, default_value = "changeme")]
    password: String,
    /// 注册有效期（秒）
    #[arg(short, long, default_value_t = 10)]
    expires: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();

    // Contact 用本机 LAN IP（不能是 127.0.0.1，服务器要按它回送 INVITE），
    // 端口 0 交给系统分配
    let client_addr = SocketAddr::V4(SocketAddrV4::new(ipcam_sip::local_ipv4()?, 0));

    let config = SipClientConfig::new(
        args.username.clone(),
        args.username,
        args.password,
        args.server,
        client_addr,
        Some(args.expires),
    );
    let client = SipClient::new(config, CancellationToken::new()).await?;
    let mut endpoint = client.spawn_endpoint();
    info!(server = %args.server, "registering, ctrl+c to stop");

    // Box::pin：select! 分支里用 &mut 复用同一个 future，
    // ctrl+c 后 shutdown 还能等它把注销流程跑完
    let mut reg = Box::pin(client.process_register());
    select! {
        r = &mut endpoint => {
            warn!(result = ?r, "SIP endpoint exited");
        }
        r = &mut reg => {
            warn!(result = ?r, "register loop exited unexpectedly");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl+c received, stopping (unregister)");
        }
    }
    client.shutdown(reg).await
}
