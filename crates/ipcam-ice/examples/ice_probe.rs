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
    /// sigserver WS 地址 (ws://HOST:PORT); NAT 判定模式 (--stun-probe) 不需要
    #[arg(long, value_parser = parse_ws_url, required_unless_present = "stun_probe")]
    sig: Option<String>,
    /// 房间名 (同房两名自动配对; 先加入者为 slot 0 = offerer)
    #[arg(long, default_value = "ice-probe")]
    room: String,
    /// STUN server URI (如 stun://stun.miwifi.com:3478); 不给 = 仅 host 候选
    #[arg(long)]
    stun: Option<String>,
    /// 整体超时 (秒)
    #[arg(long, default_value_t = 30)]
    timeout: u64,
    /// 只留 srflx/relay 候选 (丢全部 typ host 行), 强制走 NAT 打洞路径
    /// 验证 srflx 互穿; 需双方同时加本旗标, 同机自测受 NAT hairpin 限制
    #[arg(long)]
    srflx_only: bool,
    /// NAT 类型判定模式: 同一个 UDP socket 向两个 STUN 服务器发 binding
    /// request, 对比学到的公网映射 — 端口一致 = 锥形 (打洞可通), 不同 =
    /// 对称 (打洞必失败, 需 TURN). 本模式不需要 --sig/--room
    #[arg(long)]
    stun_probe: bool,
    /// NAT 判定模式的第二 STUN 服务器 (默认 stun://stun.l.google.com:19302)
    #[arg(long)]
    stun_b: Option<String>,
}

/// --sig 的协议校验: clap value_parser, 非法值按参数错误处理 (退出码 2).
fn parse_ws_url(s: &str) -> Result<String, String> {
    if s.starts_with("ws://") || s.starts_with("wss://") {
        Ok(s.to_string())
    } else {
        Err("expected a ws:// or wss:// URL".to_string())
    }
}

/// --srflx-only: 过滤本地 ICE blob, 丢掉全部 `typ host` 候选行, 只留
/// srflx/relay. 用于强制走 NAT 打洞路径 (需双方同时加本旗标;
/// 同机自测受 NAT hairpin 限制, 需双机跨 NAT 验证).
fn keep_only_public_mapped(blob: &ipcam_ice::blob::IceBlob) -> ipcam_ice::blob::IceBlob {
    let filtered: String = blob
        .as_str()
        .split_inclusive('\n')
        .filter(|line| !line.contains(" typ host"))
        .collect();
    ipcam_ice::blob::IceBlob::from_sdp(filtered)
}

// ---- NAT 类型判定 (--stun-probe) ----
// STUN binding request/response 按 RFC 5389 手写, probe 专用不入库.
// 判定依据: 同一 socket 问两个不同 STUN 服务器, 映射端口一致 = 锥形
// (映射与目的无关, P2P 可通); 不同 = 对称 (映射随目的变化, 需 TURN).

const STUN_MAGIC: u32 = 0x2112_A442;

fn stun_host_port(uri: &str) -> Result<(String, u16)> {
    let s = uri
        .trim()
        .trim_start_matches("stun://")
        .trim_start_matches("stun:");
    let (host, port) = match s.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().context("bad stun port")?),
        None => (s.to_string(), 3478),
    };
    if host.is_empty() {
        bail!("empty stun host in {uri}");
    }
    Ok((host, port))
}

/// 构造 binding request: header(type=0x0001, len=0, magic) + 12 字节事务 ID.
fn build_binding(tid: [u8; 12]) -> [u8; 20] {
    let mut req = [0u8; 20];
    req[0] = 0;
    req[1] = 1;
    req[4..8].copy_from_slice(&STUN_MAGIC.to_be_bytes());
    req[8..20].copy_from_slice(&tid);
    req
}

/// 无 rand 依赖的弱事务 ID (诊断工具, 唯一性要求低).
fn make_tid() -> [u8; 12] {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let mut tid = [0u8; 12];
    tid[0..4].copy_from_slice(&nanos.to_be_bytes());
    tid[4..8].copy_from_slice(&pid.to_be_bytes());
    tid[8..12].copy_from_slice(&(nanos ^ pid).to_be_bytes());
    tid
}

/// 从 binding response 提取 XOR-MAPPED-ADDRESS (0x0020, 优先) 或
/// MAPPED-ADDRESS (0x0001). 属性按 4 字节对齐步进.
fn mapped_addr(resp: &[u8], tid: [u8; 12]) -> Option<std::net::SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    if resp.len() < 20 || resp[8..20] != tid {
        return None;
    }
    let mut off = 20;
    while off + 4 <= resp.len() {
        let atype = u16::from_be_bytes([resp[off], resp[off + 1]]);
        let alen = u16::from_be_bytes([resp[off + 2], resp[off + 3]]) as usize;
        let val = off + 4;
        if val + alen > resp.len() {
            return None;
        }
        match atype {
            0x0020 if alen >= 8 => {
                let xport = u16::from_be_bytes([resp[val + 2], resp[val + 3]]);
                match resp[val + 1] {
                    0x01 => {
                        let xip = u32::from_be_bytes([
                            resp[val + 4],
                            resp[val + 5],
                            resp[val + 6],
                            resp[val + 7],
                        ]);
                        return Some(SocketAddr::new(
                            IpAddr::V4(Ipv4Addr::from(xip ^ STUN_MAGIC)),
                            xport ^ (STUN_MAGIC >> 16) as u16,
                        ));
                    }
                    0x02 if alen >= 20 => {
                        let mut xip = [0u8; 16];
                        xip.copy_from_slice(&resp[val + 4..val + 20]);
                        let mut mask = [0u8; 16];
                        mask[0..4].copy_from_slice(&STUN_MAGIC.to_be_bytes());
                        mask[4..16].copy_from_slice(&tid);
                        for i in 0..16 {
                            xip[i] ^= mask[i];
                        }
                        return Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(xip)), xport));
                    }
                    _ => return None,
                }
            }
            0x0001 if alen >= 8 => {
                let port = u16::from_be_bytes([resp[val + 2], resp[val + 3]]);
                match resp[val + 1] {
                    0x01 => {
                        let ip = Ipv4Addr::new(
                            resp[val + 4],
                            resp[val + 5],
                            resp[val + 6],
                            resp[val + 7],
                        );
                        return Some(SocketAddr::new(IpAddr::V4(ip), port));
                    }
                    0x02 if alen >= 20 => {
                        let mut ip = [0u8; 16];
                        ip.copy_from_slice(&resp[val + 4..val + 20]);
                        return Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port));
                    }
                    _ => return None,
                }
            }
            _ => {}
        }
        off = val + alen.div_ceil(4) * 4;
    }
    None
}

/// 等匹配 tid 的 binding response (同 socket 上有两路并发事务).
async fn recv_mapped(
    sock: &tokio::net::UdpSocket,
    tid: [u8; 12],
    what: &str,
) -> Option<std::net::SocketAddr> {
    let mut buf = [0u8; 512];
    for _ in 0..4 {
        match timeout(Duration::from_secs(4), sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _from))) => {
                if let Some(addr) = mapped_addr(&buf[..n], tid) {
                    return Some(addr);
                }
                // tid 不匹配的包 (另一路事务) 继续等
            }
            _ => {
                println!("{what}: STUN 无响应 (超时/不可达)");
                return None;
            }
        }
    }
    None
}

/// 显式解析并选定 IPv4 地址. 判定 socket 绑 0.0.0.0 (IPv4); macOS 等系统
/// 的 getaddrinfo 会把有 AAAA 记录的域名排在前面 (如 stun.l.google.com),
/// 直接 (host, port) 发送会对 IPv4 socket 报 EINVAL (os error 22).
async fn resolve_v4(host: &str, port: u16) -> Result<std::net::SocketAddr> {
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve {host}:{port}"))?;
    addrs
        .into_iter()
        .find(|a| a.is_ipv4())
        .with_context(|| format!("{host} 无 IPv4 地址 (判定 socket 为 IPv4)"))
}

/// NAT 类型判定主流程.
async fn stun_nat_probe(args: &Args) -> Result<()> {
    let primary = args
        .stun
        .clone()
        .unwrap_or_else(|| "stun://stun.miwifi.com:3478".into());
    let secondary = args
        .stun_b
        .clone()
        .unwrap_or_else(|| "stun://stun.l.google.com:19302".into());
    let (h1, p1) = stun_host_port(&primary)?;
    let (h2, p2) = stun_host_port(&secondary)?;
    // 显式解析并只取 IPv4: 判定 socket 绑 0.0.0.0 (IPv4), 而 macOS 等系统的
    // getaddrinfo 会把有 AAAA 记录的域名排在前面 (如 stun.l.google.com),
    // 直接 (host, port) 发送会对 IPv4 socket 报 EINVAL (os error 22).
    let addr1 = resolve_v4(&h1, p1).await?;
    let addr2 = resolve_v4(&h2, p2).await?;
    info!("binding from one socket to {addr1} and {addr2}");

    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .context("bind udp")?;
    let tid1 = make_tid();
    sock.send_to(&build_binding(tid1), addr1)
        .await
        .context("send to primary stun")?;
    let tid2 = make_tid();
    sock.send_to(&build_binding(tid2), addr2)
        .await
        .context("send to secondary stun")?;

    let m1 = recv_mapped(&sock, tid1, "primary stun").await;
    let m2 = recv_mapped(&sock, tid2, "secondary stun").await;
    match (m1, m2) {
        (Some(a), Some(b)) => {
            println!("A ({primary}) 学到的映射: {a}");
            println!("B ({secondary}) 学到的映射: {b}");
            if a == b {
                println!("判定: 锥形 NAT (映射与目的地址无关, P2P 打洞可通)");
            } else {
                println!("判定: 对称 NAT (映射随目的地址变化, srflx 打洞必失败, 需 TURN/中继)");
            }
            Ok(())
        }
        (a, b) => {
            println!("判定失败: 只拿到单边映射 (A={a:?}, B={b:?}), 无法对比");
            bail!("stun probe incomplete");
        }
    }
}

async fn run(args: Args) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(args.timeout);
    let remaining = || deadline.saturating_duration_since(Instant::now());

    // 1. 连 sigserver 并 join 房间 (agent 尚未创建, 失败无需清理)
    let (ws, slot) = timeout(
        remaining(),
        signal::ws_connect_join(
            args.sig
                .as_deref()
                .expect("clap requires --sig without --stun-probe"),
            &args.room,
        ),
    )
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
    let outcome = probe(&agent, ws, slot, remaining, args.srflx_only).await;
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
    srflx_only: bool,
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
    // --srflx-only: 发送前滤掉 host 候选, 强制双方 srflx 互穿.
    let local_ice = if srflx_only {
        let kept = keep_only_public_mapped(&local_ice);
        info!(
            before = local_ice.candidates().len(),
            after = kept.candidates().len(),
            "srflx-only: host candidates filtered out"
        );
        kept
    } else {
        local_ice
    };
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
    // 打印 anyhow 上下文链 (冒号分隔), 退出码 1. --stun-probe 走 NAT 判定.
    let args = Args::parse();
    if args.stun_probe {
        std::process::exit(match stun_nat_probe(&args).await {
            Ok(()) => 0,
            Err(e) => {
                error!("{:#}", e);
                1
            }
        });
    }
    std::process::exit(match run(args).await {
        Ok(()) => 0,
        Err(e) => {
            error!("{:#}", e);
            1
        }
    });
}
