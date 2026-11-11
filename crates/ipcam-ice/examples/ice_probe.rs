//! ice_probe: 无头 ICE 连通性测试工具.
//!
//! 两台机器各跑一个实例 (同一房名), 经 sigserver 交换 blob, 打洞连通后
//! 双向各发一帧 payload 验证数据面, 打印选中路径候选类型.
//!
//! 用法:
//!   ice_probe --sig ws://HOST:PORT [--room NAME] [--stun stun://...] [--timeout SECS]
//!   (完整参数与说明见 --help, clap 生成)
//!
//! 先 join 者为 slot 0 (offerer/controlling), 后 join 者为 slot 1。
//! 退出码: 0 = 连通且数据面互通; 1 = 任一步失败; 2 = 参数错误。

use std::time::Duration;

use clap::Parser as _;
use futures_util::{SinkExt, StreamExt};
use ipcam_ice::blob::IceBlob;
use ipcam_ice::{IceAgent, IceConfig, IceState, SelectedKind};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{error, info, warn};

/// sigserver 线协议 (字段对齐 media_talk_rust/src/bin/sigserver.rs 的 Frame;
/// probe 只与同版本 probe 互通, 宽字段 + default 保证未知帧不炸).
#[derive(Debug, Serialize, Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    room: String,
    #[serde(default)]
    payload: String,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    slot: u8,
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsTx = futures_util::stream::SplitSink<Ws, Message>;
type WsRx = futures_util::stream::SplitStream<Ws>;

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
    /// STUN server URI (如 stun://stun.l.google.com:19302); 不给 = 仅 host 候选
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
        Err("需为 ws:// 或 wss:// 地址".to_string())
    }
}

/// 收一帧文本 Frame; 忽略非文本/无法解析的帧直到 deadline.
/// (必须是 loop 而非递归 — async fn 自递归需要 boxing, E0733)
async fn recv_frame(rx: &mut WsRx, deadline: Instant) -> Result<Frame, i32> {
    loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        let msg = match timeout(remain, rx.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => {
                error!("WS 连接错误: {e}");
                return Err(1);
            }
            Ok(None) => {
                error!("WS 连接被服务端关闭");
                return Err(1);
            }
            Err(_) => {
                error!("等待对端超时");
                return Err(1);
            }
        };
        match msg.into_text() {
            Ok(text) => match serde_json::from_str::<Frame>(&text) {
                Ok(f) => return Ok(f),
                Err(e) => warn!("忽略无法解析的帧: {e}"),
            },
            Err(_) => warn!("忽略非文本帧"),
        }
    }
}

async fn send_frame(tx: &mut WsTx, kind: &str, payload: &str) -> Result<(), i32> {
    let text = serde_json::json!({ "type": kind, "payload": payload }).to_string();
    tx.send(Message::text(text)).await.map_err(|e| {
        error!("WS 发送失败: {e}");
        1
    })
}

/// 等到一帧 type=blob 并返回 payload; error 帧直接失败退出.
async fn recv_blob(rx: &mut WsRx, deadline: Instant) -> Result<String, i32> {
    loop {
        let f = recv_frame(rx, deadline).await?;
        match f.kind.as_str() {
            "blob" => return Ok(f.payload),
            "error" => {
                error!("服务端/对端错误: {}", f.detail);
                return Err(1);
            }
            other => warn!(kind = other, "blob 交换中忽略无关帧"),
        }
    }
}

async fn run(args: Args) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(args.timeout);

    // 1. 连 sigserver 并 join 房间
    let ws = match timeout(
        Duration::from_secs(args.timeout),
        tokio_tungstenite::connect_async(&args.sig),
    )
    .await
    {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => {
            error!(sig = %args.sig, "连接 sigserver 失败: {e}");
            return 1;
        }
        Err(_) => {
            error!(sig = %args.sig, "连接 sigserver 超时");
            return 1;
        }
    };
    let (mut tx, mut rx) = ws.split();
    let join = serde_json::json!({ "type": "join", "room": args.room }).to_string();
    if let Err(e) = tx.send(Message::text(join)).await {
        error!("发送 join 失败: {e}");
        return 1;
    }
    let slot = loop {
        match recv_frame(&mut rx, deadline).await {
            Ok(f) => match f.kind.as_str() {
                "joined" => break f.slot,
                "error" => {
                    error!("join 被拒: {}", f.detail);
                    return 1;
                }
                other => warn!(kind = other, "join 阶段忽略无关帧"),
            },
            Err(code) => return code,
        }
    };
    info!(room = %args.room, slot, "已加入房间");

    // 2. 建 agent; slot 0 = offerer = controlling
    let agent = match IceAgent::new(IceConfig {
        stun: args.stun.clone(),
        controlling: slot == 0,
    }) {
        Ok(a) => a,
        Err(e) => {
            error!("IceAgent 创建失败 (libnice 缺失?): {e}");
            return 1;
        }
    };
    info!(controlling = slot == 0, stun = ?args.stun, "ICE agent 就绪");

    // attach 必须在等 Connected 之前 (同环回测试: attach 才把 component 的
    // socket I/O 路由到被迭代的 ctx, 否则连通性检查回包无人收).
    let (data_tx, mut data_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    if let Err(e) = agent
        .attach_recv(move |d| {
            let _ = data_tx.try_send(d.to_vec());
        })
        .await
    {
        error!("attach_recv 失败: {e}");
        agent.close();
        return 1;
    }

    // 3. blob 交换: offerer 先发包, answerer 先收包 (同 call-ui 握手序)
    if slot == 0 {
        let blob = match agent.local_blob().await {
            Ok(b) => b,
            Err(e) => {
                error!("生成本地 blob 失败: {e}");
                agent.close();
                return 1;
            }
        };
        info!(bytes = blob.as_str().len(), "本地 blob 已生成, 发给对端");
        if let Err(code) = send_frame(&mut tx, "blob", blob.as_str()).await {
            agent.close();
            return code;
        }
        let peer = match recv_blob(&mut rx, deadline).await {
            Ok(p) => p,
            Err(code) => {
                agent.close();
                return code;
            }
        };
        if let Err(e) = agent.add_remote(&IceBlob::from_sdp(peer)).await {
            error!("喂入对端 blob 被拒: {e}");
            agent.close();
            return 1;
        }
    } else {
        let peer = match recv_blob(&mut rx, deadline).await {
            Ok(p) => p,
            Err(code) => {
                agent.close();
                return code;
            }
        };
        let blob = match agent.local_blob().await {
            Ok(b) => b,
            Err(e) => {
                error!("生成本地 blob 失败: {e}");
                agent.close();
                return 1;
            }
        };
        if let Err(e) = agent.add_remote(&IceBlob::from_sdp(peer)).await {
            error!("喂入对端 blob 被拒: {e}");
            agent.close();
            return 1;
        }
        if let Err(code) = send_frame(&mut tx, "blob", blob.as_str()).await {
            agent.close();
            return code;
        }
    }

    // 4. 等 Connected
    let t0 = Instant::now();
    let mut state_rx = agent.state_rx();
    loop {
        match *state_rx.borrow() {
            IceState::Connected => break,
            IceState::Failed => {
                error!("ICE 打洞失败 (Failed)");
                agent.close();
                return 1;
            }
            _ => {}
        }
        tokio::select! {
            _ = state_rx.changed() => {}
            _ = tokio::time::sleep_until(deadline) => {
                error!("等待 Connected 超时");
                agent.close();
                return 1;
            }
        }
    }
    info!(
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "ICE Connected"
    );

    // 5. 数据面: 各发一帧 payload, 各收对端一帧
    const PING: &[u8] = b"ice-probe-ping";
    if let Err(e) = agent.send(PING).await {
        error!("数据面发送失败: {e}");
        agent.close();
        return 1;
    }
    match timeout(Duration::from_secs(10), data_rx.recv()).await {
        Ok(Some(got)) if got == PING => {
            info!(bytes = got.len(), "收到对端 payload, 数据面互通");
        }
        Ok(Some(got)) => {
            error!("payload 内容不符: {:?}", String::from_utf8_lossy(&got));
            agent.close();
            return 1;
        }
        Ok(None) => {
            error!("数据通道已关闭");
            agent.close();
            return 1;
        }
        Err(_) => {
            error!("等对端 payload 超时 (10s), 数据面不通");
            agent.close();
            return 1;
        }
    }

    // 6. 选中路径候选类型 + 结果行
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
