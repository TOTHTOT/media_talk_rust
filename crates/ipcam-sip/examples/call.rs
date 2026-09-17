//! 实机呼叫冒烟工具：注册 → INVITE（带音频 SDP offer）→ 解析 200 OK
//! 里的 answer, 打印协商出的对端 RTP 地址/编码. **只协商、不流真实
//! 媒体**——拿到 peer 地址后接 GStreamer 是下一步. Ctrl+C 发 BYE 挂断,
//! 然后走和 register example 相同的 best-effort 注销退出.
//!
//! ```bash
//! cargo run -p ipcam-sip --example call -- 1002
//! cargo run -p ipcam-sip --example call -- sip:1002@192.168.1.17:5062 -u 1001 -p changeme
//! ```

use anyhow::{Result, anyhow};
use clap::Parser;
use ipcam_sip::sdp::{PT_PCMA, build_av_offer, parse_answer_all};
use ipcam_sip::{SipClient, SipClientConfig};
use rsipstack::dialog::dialog::DialogState;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::sip as rsip;
use rsipstack::transaction::Endpoint;
use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(Parser)]
#[command(about = "SIP 实机呼叫冒烟：INVITE → 打印 SDP 协商结果 → Ctrl+C BYE 挂断")]
struct Args {
    /// 被叫：裸号码（自动补 @服务器）或完整 sip: URI
    callee: String,
    /// SIP 服务器地址
    #[arg(long, default_value = "192.168.1.17:5062")]
    server: SocketAddr,
    /// 主叫账号（同时作为用户名）
    #[arg(short, long, default_value = "9999")]
    username: String,
    /// 密码
    #[arg(short, long, default_value = "changeme")]
    password: String,
    /// 注册有效期（秒）
    #[arg(short, long, default_value_t = 60)]
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

    // 与 register example 相同：Contact/SDP 都要用对端可达的 LAN 地址
    let IpAddr::V4(local) = local_ip_address::local_ip()? else {
        anyhow::bail!("仅支持 IPv4");
    };
    let client_addr = SocketAddr::V4(SocketAddrV4::new(local, 0));

    let config = SipClientConfig::new(
        args.username.clone(),
        args.username.clone(),
        args.password.clone(),
        args.server,
        client_addr,
        Some(args.expires),
    );
    let credential = config.to_credential();
    let client = SipClient::new(config, CancellationToken::new()).await?;

    // endpoint 收包循环甩后台（从 inner 重建 owned Endpoint, 同 lib 测试的做法）
    let ep = Endpoint {
        inner: client.endpoint.inner.clone(),
    };
    tokio::spawn(async move { ep.serve().await });

    // 裸号码补全成完整 URI
    let callee_text = if args.callee.starts_with("sip:") || args.callee.starts_with("sips:") {
        args.callee.clone()
    } else {
        format!("sip:{}@{}", args.callee, args.server)
    };
    let callee = rsip::Uri::try_from(callee_text.as_str())?;

    // offer 里写真实绑定的 UDP 端口, 而不是随口编一个——对端会按
    // 它回送 RTP. 骨架阶段不收包, socket 保持存活避免 ICMP unreachable.
    // 音频和视频各绑一个
    let audio_rtp_sock = std::net::UdpSocket::bind((local, 0))?;
    let video_rtp_sock = std::net::UdpSocket::bind((local, 0))?;
    let audio_port = audio_rtp_sock.local_addr()?.port();
    let video_port = video_rtp_sock.local_addr()?.port();
    let offer = build_av_offer(IpAddr::V4(local), audio_port, video_port, PT_PCMA);
    info!(%callee, audio_port, video_port, "calling, SDP offer:\n{}", offer.to_string());

    let dialog_layer = client.dialog_layer.clone();
    let (state_sender, state_receiver) = dialog_layer.new_dialog_state_channel();
    spawn_state_printer(dialog_layer.clone(), state_receiver);

    let invite_option = InviteOption {
        callee,
        caller: client.contact.clone(),
        contact: client.contact.clone(),
        credential: Some(credential),
        offer: Some(offer.to_string().into_bytes()),
        ..Default::default()
    };
    // 注册循环和呼叫并行：INVITE 的 407/401 challenge 由 rsipstack
    // 用 credential 自动处理, 不必等注册完成
    let mut reg = Box::pin(client.process_register());
    select! {
        r = &mut reg => {
            warn!(result = ?r, "register loop exited unexpectedly");
        }
        r = call_until_hangup(dialog_layer, invite_option, state_sender) => {
            if let Err(e) = r {
                warn!(error = ?e, "call failed");
            }
        }
    }

    // 通话结束（或出错）后走标准退出：注销 + 收尾
    info!("stopping (unregister)");
    client.stop();
    let r = reg.await;
    info!(result = ?r, "register loop exited");
    Ok(())
}

/// INVITE → 等最终响应 → 打印 answer 协商结果 → 挂着等 Ctrl+C → BYE
async fn call_until_hangup(
    dialog_layer: Arc<DialogLayer>,
    invite_option: InviteOption,
    state_sender: rsipstack::dialog::dialog::DialogStateSender,
) -> Result<()> {
    let (dialog, resp) = dialog_layer.do_invite(invite_option, state_sender).await?;
    let resp = resp.ok_or_else(|| anyhow!("INVITE got no final response"))?;
    info!(code = %resp.status_code, "INVITE final response");
    if resp.status_code != rsip::StatusCode::OK {
        anyhow::bail!("call rejected: {}", resp.status_code);
    }

    let peers = parse_answer_all(resp.body())?;
    if let Some(audio) = &peers.audio {
        info!(
            addr = %audio.addr,
            payload_type = audio.payload_type,
            codec = %audio.codec,
            "SDP 协商结果: 音频"
        );
    }
    if let Some(video) = &peers.video {
        info!(
            addr = %video.addr,
            payload_type = video.payload_type,
            codec = %video.codec,
            "SDP 协商结果: 视频"
        );
    }
    if peers.video.is_none() {
        info!("对端没有接视频路 (纯音频设备)");
    }
    info!("call established (no media yet), ctrl+c to hang up");

    tokio::signal::ctrl_c().await.ok();
    dialog.bye_with_headers(None).await?;
    info!("BYE sent");
    Ok(())
}

/// 打印对话状态事件；Terminated 时按 rsipstack 文档要求 remove_dialog,
/// 否则 confirmed dialog 永远挂在 registry 里（内存泄漏）
fn spawn_state_printer(
    dialog_layer: Arc<DialogLayer>,
    mut state_receiver: rsipstack::dialog::dialog::DialogStateReceiver,
) {
    tokio::spawn(async move {
        while let Some(state) = state_receiver.recv().await {
            info!(%state, "dialog state");
            if let DialogState::Terminated(id, _) = state {
                dialog_layer.remove_dialog(&id);
            }
        }
    });
}
