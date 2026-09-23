//! 实机呼叫冒烟工具：注册 → INVITE（带音视频 SDP offer）→ 解析 200 OK
//! 里的 answer → 起 RTP 发送器把媒体文件 (默认 assets/oceans.mp4) 灌给
//! 对端. Ctrl+C 停流发 BYE 挂断, 然后走和 register example 相同的
//! best-effort 注销退出.
//!
//! ```bash
//! cargo run -p ipcam-sip --example call -- 1002
//! ```

use anyhow::{Result, anyhow};
use clap::Parser;
use ipcam_gst::TrackSource;
use ipcam_sip::sdp::{PT_PCMA, PT_PCMU, build_av_offer, parse_answer_all};
use ipcam_sip::{SipClient, SipClientConfig, run_dialog_state_loop};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::sip as rsip;
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
    #[arg(long, default_value = "192.168.11.17:5062")]
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
    /// 视频源: file:<路径> | rtsp://<uri> | camera[:<设备>]
    #[arg(long, default_value = "file:assets/oceans.mp4")]
    video_src: String,
    /// 音频源: file:<路径> | mic
    #[arg(long, default_value = "file:assets/oceans.mp4")]
    audio_src: String,
    /// 视频限宽 (对端解码能力, 超宽源会降采样, 保持宽高比); 0 = 不限
    #[arg(long, default_value_t = 640)]
    max_width: u32,
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
    let video_src = ipcam_gst::parse_track_source(&args.video_src).map_err(anyhow::Error::msg)?;
    let audio_src = ipcam_gst::parse_track_source(&args.audio_src).map_err(anyhow::Error::msg)?;

    // Contact/SDP 都要用对端可达的 LAN 地址 (不能是 127.0.0.1)
    let local = ipcam_sip::local_ipv4()?;
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
    let _endpoint = client.spawn_endpoint();

    // 裸号码补全成完整 URI
    let callee = client.callee_uri(&args.callee)?;

    // offer 里写真实绑定的 UDP 端口, 而不是随口编一个——对端会按
    // 它回送 RTP. 骨架阶段不收包, socket 保持存活避免 ICMP unreachable.
    // 音频和视频各绑一个
    let audio_rtp_sock = std::net::UdpSocket::bind((local, 0))?;
    let video_rtp_sock = std::net::UdpSocket::bind((local, 0))?;
    let audio_port = audio_rtp_sock.local_addr()?.port();
    let video_port = video_rtp_sock.local_addr()?.port();
    let offer = build_av_offer(
        IpAddr::V4(local),
        audio_port,
        video_port,
        &[PT_PCMU, PT_PCMA],
    );
    info!(%callee, audio_port, video_port, "calling, SDP offer:\n{}", offer.to_string());

    let dialog_layer = client.dialog_layer.clone();
    let (state_sender, state_receiver) = dialog_layer.new_dialog_state_channel();
    // 状态日志 + Terminated 清理 (防泄漏) 甩后台; 主叫没有来电, 回调用不上
    tokio::spawn(run_dialog_state_loop(
        dialog_layer.clone(),
        state_receiver,
        |_| {},
    ));

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
        r = call_until_hangup(dialog_layer, invite_option, state_sender, video_src, audio_src, args.max_width) => {
            if let Err(e) = r {
                warn!(error = ?e, "call failed");
            }
        }
    }

    // 通话结束（或出错）后走标准退出：注销 + 收尾
    info!("stopping (unregister)");
    client.shutdown(reg).await
}

/// INVITE → 等最终响应 → 打印协商结果 → 起 RTP 发送器往对端灌媒体文件 →
/// 挂着等 Ctrl+C → 停流 → BYE
async fn call_until_hangup(
    dialog_layer: Arc<DialogLayer>,
    invite_option: InviteOption,
    state_sender: rsipstack::dialog::dialog::DialogStateSender,
    video_src: TrackSource,
    audio_src: TrackSource,
    max_width: u32,
) -> Result<()> {
    let (dialog, resp) = dialog_layer.do_invite(invite_option, state_sender).await?;
    let resp = resp.ok_or_else(|| anyhow!("INVITE got no final response"))?;
    info!(code = %resp.status_code, "INVITE final response");
    if resp.status_code != rsip::StatusCode::OK {
        anyhow::bail!("call rejected: {}", resp.status_code);
    }

    let peers = parse_answer_all(resp.body())?;
    if let Some(audio) = &peers.audio {
        info!(?audio, "SDP consult result");
    }
    if let Some(video) = &peers.video {
        info!(?video, "SDP consult result");
    }

    // 发送 pt/编码必须用对端 answer 里的值 (动态 pt 分方向, 编码同理 --
    // answer 收窄成什么就发什么, 见 sdp 模块注释)
    let audio = peers.audio.and_then(|p| {
        ipcam_gst::sendable_audio_codec(&p.codec)
            .map(|codec| ipcam_gst::AudioDest {
                addr: p.addr,
                payload_type: p.payload_type + 1,
                codec,
            })
            .or_else(|| {
                warn!(codec = %p.codec, "answer picked an audio codec we cannot send, skip audio");
                None
            })
    });
    let sender = ipcam_gst::start_rtp_sender(ipcam_gst::RtpSendConfig {
        audio: audio.map(|d| (audio_src.clone(), d)),
        video: peers.video.map(|p| {
            (
                video_src.clone(),
                ipcam_gst::RtpDest {
                    addr: p.addr,
                    payload_type: p.payload_type,
                    // 0 = 对端不限宽; 门口机按 640 保守发
                    max_width: (max_width > 0).then_some(max_width),
                },
            )
        }),
    })?;
    info!("call established, streaming media file, ctrl+c to hang up");

    tokio::signal::ctrl_c().await.ok();
    sender.stop();
    dialog.bye_with_headers(None).await?;
    info!("BYE sent");
    Ok(())
}
