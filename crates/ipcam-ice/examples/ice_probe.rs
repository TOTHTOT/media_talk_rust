//! ice_probe: 无头 ICE 连通性测试工具.
//!
//! 两台机器各跑一个实例 (同一房名), 经 sigserver 交换 blob, 打洞连通后
//! 双向各发一个 payload 验证数据面, 打印选中路径候选类型.
//!
//! 用法:
//!   ice_probe --sig ws://HOST:PORT [--room NAME] [--stun stun://...] [--timeout SECS]
//!   (完整参数与说明见 --help, clap 生成)
//!
//! 先 join 者为 slot 0 (offerer/controlling), 后 join 者为 slot 1.
//! 退出码: 0 = 连通且数据面互通, 1 = 任一步失败, 2 = 参数错误.

use std::time::Duration;

use clap::Parser as _;
use ipcam_ice::blob::IceBlob;
use ipcam_ice::sdp;
use ipcam_ice::signal;
use ipcam_ice::{IceAgent, IceConfig, SelectedKind};
use tokio::time::{Instant, timeout};
use tracing::{error, info};

/// ice_probe 命令行参数 (clap derive; 字段 doc 即 --help 文案).
#[derive(Debug, clap::Parser)]
#[command(
    name = "ice_probe",
    about = "无头 ICE 连通性测试工具: 两台机器各跑一个实例 (同一房名), 经 sigserver 交换 blob, 打洞后双向验证数据面"
)]
struct Args {
    /// sigserver WS 地址 (ws://HOST:PORT)
    #[arg(long, value_parser = parse_ws_url)]
    sig: String,
    /// 房间名 (同房两名自动配对; 先加入者为 slot 0 = offerer)
    #[arg(long, default_value = "ice-probe")]
    room: String,
    /// STUN server URI (如 stun://stun.miwifi.com:3478); 不给 = 仅 host 候选
    #[arg(long)]
    stun: Option<String>,
    /// 整体超时 (秒)
    #[arg(long, default_value_t = 30)]
    timeout: u64,
}

/// --sig 的协议校验: clap value_parser, 非法值按参数错误处理 (退出码 2).
fn parse_ws_url(s: &str) -> Result<String, String> {
    if s.starts_with("ws://") || s.starts_with("wss://") {
        Ok(s.to_string())
    } else {
        Err("expected a ws:// or wss:// URL".to_string())
    }
}

async fn run(args: Args) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(args.timeout);
    let remaining = || deadline.saturating_duration_since(Instant::now());

    // 1. 连 sigserver 并 join 房间
    let (ws, slot) =
        match timeout(remaining(), signal::ws_connect_join(&args.sig, &args.room)).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                error!(sig = %args.sig, "failed to connect/join sigserver: {e}");
                return 1;
            }
            Err(_) => {
                error!(sig = %args.sig, "connecting to sigserver timed out");
                return 1;
            }
        };
    info!(room = %args.room, slot, "joined room");

    // 2. 建 agent; slot 0 = offerer = controlling; 纯连通性测试单 component 即可
    let agent = match IceAgent::new(IceConfig {
        stun: args.stun.clone(),
        controlling: signal::is_offerer(slot),
        components: 1,
    }) {
        Ok(a) => a,
        Err(e) => {
            error!("failed to create IceAgent (libnice missing?): {e}");
            return 1;
        }
    };
    info!(
        controlling = signal::is_offerer(slot),
        stun = ?args.stun,
        "ICE agent ready"
    );

    // attach 必须在等 Connected 之前 (同环回测试: attach 才把 component 的
    // socket I/O 路由到被迭代的 ctx, 否则连通性检查回包无人收).
    let (data_tx, mut data_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    if let Err(e) = agent
        .attach_recv(move |d| {
            let _ = data_tx.try_send(d.to_vec());
        })
        .await
    {
        error!("attach_recv failed: {e}");
        agent.close();
        return 1;
    }

    // 3. 本地 ICE + 标准媒体 SDP 信令交换 (call-ui 与 probe 共用).
    //    sigserver 会缓存 offerer 的第一条 offer, answerer 晚加入也能收到.
    let local_ice = match timeout(remaining(), agent.local_blob()).await {
        Ok(Ok(blob)) => blob,
        Ok(Err(e)) => {
            error!("failed to gather local ICE blob: {e}");
            agent.close();
            return 1;
        }
        Err(_) => {
            error!("gathering local ICE blob timed out");
            agent.close();
            return 1;
        }
    };
    {
        let pretty = local_ice.as_str().replace("\r\n", "\n");
        info!("local ICE SDP:\n{}", pretty.trim_end());
    }
    let local_sdp = match sdp::build_offer(&local_ice, None) {
        Ok(sdp) => sdp,
        Err(e) => {
            error!("failed to build media SDP: {e}");
            agent.close();
            return 1;
        }
    };
    let local = IceBlob::from_sdp(local_sdp);
    info!(
        bytes = local.as_str().len(),
        "local media SDP gathered, sending to peer"
    );

    let remote = match timeout(
        remaining(),
        signal::ws_exchange(ws, slot, &local, signal::BLOB, signal::BLOB),
    )
    .await
    {
        Ok(Ok(blob)) => blob,
        Ok(Err(e)) => {
            error!("signaling exchange failed: {e}");
            agent.close();
            return 1;
        }
        Err(_) => {
            error!("timed out waiting for peer");
            agent.close();
            return 1;
        }
    };
    info!(bytes = remote.as_str().len(), "remote media SDP received");
    let remote_ice = match sdp::parse_offer(remote.as_str()) {
        Ok(media) => match media.video {
            Some(video) => video,
            None => {
                error!("remote media SDP has no video media");
                agent.close();
                return 1;
            }
        },
        Err(e) => {
            error!("failed to parse remote media SDP: {e}");
            agent.close();
            return 1;
        }
    };
    {
        let pretty = remote_ice.as_str().replace("\r\n", "\n");
        info!("remote ICE SDP:\n{}", pretty.trim_end());
    }
    if let Err(e) = agent.add_remote(&remote_ice).await {
        error!("remote ICE blob rejected: {e}");
        agent.close();
        return 1;
    }

    // 4. 等 Connected
    let t0 = Instant::now();
    if let Err(e) = agent.wait_connected(remaining()).await {
        error!("ICE not connected: {e}");
        agent.close();
        return 1;
    }
    info!(
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "ICE Connected"
    );

    // 5. 数据面: 各发一个 payload, 各收对端一个
    const PING: &[u8] = b"ice-probe-ping";
    if let Err(e) = agent.send(PING).await {
        error!("data plane send failed: {e}");
        agent.close();
        return 1;
    }
    match timeout(Duration::from_secs(10), data_rx.recv()).await {
        Ok(Some(got)) if got == PING => {
            info!(bytes = got.len(), "peer payload received, data plane works");
        }
        Ok(Some(got)) => {
            error!("payload mismatch: {:?}", String::from_utf8_lossy(&got));
            agent.close();
            return 1;
        }
        Ok(None) => {
            error!("data channel closed");
            agent.close();
            return 1;
        }
        Err(_) => {
            error!("timed out waiting for peer payload (10s), data plane failed");
            agent.close();
            return 1;
        }
    }

    // 6. 选中路径候选类型 + 结果输出
    let pair = agent.selected_pair(1).await;
    let name = |k: SelectedKind| match k {
        SelectedKind::Host => "host",
        SelectedKind::Srflx => "srflx",
        SelectedKind::Prflx => "prflx",
        SelectedKind::Relay => "relay",
    };
    match pair {
        Some((local, remote)) => println!(
            "ICE OK local={} remote={} slot={}",
            name(local),
            name(remote),
            slot
        ),
        None => println!("ICE OK local=unknown remote=unknown slot={slot}"),
    }
    info!(?pair, "selected pair");
    agent.close();
    0
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    // clap 解析失败自带用法 + 退出码 2 (与退出码契约一致)
    std::process::exit(run(Args::parse()).await);
}
