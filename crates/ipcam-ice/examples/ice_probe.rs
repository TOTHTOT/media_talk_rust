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

use anyhow::{Context as _, Result, bail};
use clap::Parser as _;
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

async fn run(args: Args) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(args.timeout);
    let remaining = || deadline.saturating_duration_since(Instant::now());

    // 1. 连 sigserver 并 join 房间 (agent 尚未创建, 失败无需清理)
    let (ws, slot) = timeout(remaining(), signal::ws_connect_join(&args.sig, &args.room))
        .await
        .context("timed out connecting to the signaling server")??;
    info!(room = %args.room, slot, "joined room");

    // 2. 建 agent; slot 0 = offerer = controlling; 纯连通性测试单 component 即可
    let agent = IceAgent::new(IceConfig {
        stun: args.stun.clone(),
        controlling: signal::is_offerer(slot),
        components: 1,
    })
    .context("failed to create ICE agent (libnice missing?)")?;
    info!(
        controlling = signal::is_offerer(slot),
        stun = ?args.stun,
        "ICE agent ready"
    );

    // 业务主体无论成败, 都必须先 close agent 再向上报错 — 拆成内层函数,
    // 让 anyhow 的 ? 传播和 close 的收尾互不干扰.
    let outcome = probe(&agent, ws, slot, remaining).await;
    agent.close();
    outcome
}

/// 打洞 + 数据面验证主体 (agent 存活期内). 各步失败经 anyhow 上下文链
/// 上抛, 由 run() 在 close 之后统一打印 (退出码 1).
async fn probe(
    agent: &IceAgent,
    ws: signal::WsClient,
    slot: u8,
    remaining: impl Fn() -> Duration,
) -> Result<()> {
    // attach 必须在等 Connected 之前 (同环回测试: attach 才把 component 的
    // socket I/O 路由到被迭代的 ctx, 否则连通性检查回包无人收).
    let (data_tx, mut data_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    agent
        .attach_recv(move |d| {
            let _ = data_tx.try_send(d.to_vec());
        })
        .await
        .context("attach_recv failed")?;

    // 3. 本地 ICE + 标准媒体 SDP 信令交换 (call-ui 与 probe 共用).
    //    sigserver 会缓存 offerer 的第一条 offer, answerer 晚加入也能收到.
    let local_ice = timeout(remaining(), agent.local_blob())
        .await
        .context("gathering local ICE blob timed out")??;
    info!("local ICE SDP:\n{}", local_ice.pretty());
    let local = sdp::build_offer_blob(&local_ice, None).context("failed to build media SDP")?;
    info!(
        bytes = local.as_str().len(),
        "local media SDP gathered, sending to peer"
    );

    let remote = timeout(
        remaining(),
        signal::ws_exchange(ws, slot, &local, signal::BLOB, signal::BLOB),
    )
    .await
    .context("timed out waiting for peer")??;
    info!(bytes = remote.as_str().len(), "remote media SDP received");
    let remote_ice = sdp::parse_offer(remote.as_str())
        .context("failed to parse remote media SDP")?
        .video
        .context("remote media SDP has no video media")?;
    info!("remote ICE SDP:\n{}", remote_ice.pretty());
    agent
        .add_remote(&remote_ice)
        .await
        .context("remote ICE blob rejected")?;

    // 4. 等 Connected
    let t0 = Instant::now();
    agent
        .wait_connected(remaining())
        .await
        .context("ICE not connected")?;
    info!(
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "ICE Connected"
    );

    // 5. 数据面: 各发一个 payload, 各收对端一个
    const PING: &[u8] = b"ice-probe-ping";
    agent.send(PING).await.context("data plane send failed")?;
    match timeout(Duration::from_secs(10), data_rx.recv()).await {
        Ok(Some(got)) if got == PING => {
            info!(bytes = got.len(), "peer payload received, data plane works");
        }
        Ok(Some(got)) => bail!("payload mismatch: {:?}", String::from_utf8_lossy(&got)),
        Ok(None) => bail!("data channel closed"),
        Err(_) => bail!("timed out waiting for peer payload (10s), data plane failed"),
    }

    // 6. 选中路径候选类型 + 真实地址 + 结果输出
    let pair = agent.selected_pair(1).await;
    let name = |k: SelectedKind| match k {
        SelectedKind::Host => "host",
        SelectedKind::Srflx => "srflx",
        SelectedKind::Prflx => "prflx",
        SelectedKind::Relay => "relay",
    };
    match pair {
        Some(p) => println!(
            "ICE OK local={}({}) remote={}({}) slot={}",
            name(p.local_kind),
            p.local,
            name(p.remote_kind),
            p.remote,
            slot
        ),
        None => println!("ICE OK (selected pair unavailable) slot={slot}"),
    }
    info!(?pair, "selected pair");
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    // clap 解析失败自带用法 + 退出码 2 (与退出码契约一致); 运行期失败
    // 打印 anyhow 上下文链 (冒号分隔), 退出码 1.
    std::process::exit(match run(Args::parse()).await {
        Ok(()) => 0,
        Err(e) => {
            error!("{:#}", e);
            1
        }
    });
}
