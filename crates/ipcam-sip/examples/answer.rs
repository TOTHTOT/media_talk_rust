//! SIP 被叫 (接听) 冒烟工具: 注册 → 等来电 INVITE → 解析 offer SDP →
//! ringing + accept → 起 RTP 接收器保存音视频. 对端 BYE 或本地 Ctrl+C
//! 结束通话.
//!
//! 结构参考 rsipstack examples/client/main.rs, 事务分发和状态循环已下沉
//! 到 ipcam_sip (run_incoming_loop / run_dialog_state_loop), 这里只剩
//! 媒体相关的 process_call.
//!
//! ```bash
//! cargo run -p ipcam-sip --example answer
//! ```

use anyhow::Result;
use clap::Parser;
use ipcam_core::VideoCodec;
use ipcam_gst::{RtpRecvConfig, start_rtp_receiver};
use ipcam_sip::sdp::{PT_PCMA, build_av_answer, parse_offer_all};
use ipcam_sip::{SipClient, SipClientConfig, run_dialog_state_loop};
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::headers::Header;
use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// 接收器监听端口 (冒烟工具一次只接一路, 固定即可)
const AUDIO_RTP_PORT: u16 = 40000;
const VIDEO_RTP_PORT: u16 = 40002;

#[derive(Parser)]
#[command(about = "SIP 被叫冒烟: 注册 → 等来电 → 保存音视频到 output-dir")]
struct Args {
    /// SIP 账号 (同时作为用户名)
    #[arg(short, long, default_value = "9998")]
    username: String,
    /// SIP 服务器地址
    #[arg(long, default_value = "192.168.11.17:5062")]
    server: SocketAddr,
    /// 密码
    #[arg(short, long, default_value = "changeme")]
    password: String,
    /// 注册有效期 (秒)
    #[arg(short, long, default_value_t = 60)]
    expires: u32,
    /// 保存音视频的目录
    #[arg(long, default_value = "temp")]
    output_dir: PathBuf,
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
    std::fs::create_dir_all(&args.output_dir)?;

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
    let client = SipClient::new(config, CancellationToken::new()).await?;
    let _endpoint = client.spawn_endpoint();

    let (state_sender, state_receiver) = client.dialog_layer.new_dialog_state_channel();
    // 每通来电 spawn 独立任务处理, 不阻塞状态循环和后续来电
    let on_incoming_call = {
        let output_dir = args.output_dir.clone();
        move |dialog: InviteDialog| {
            let dir = output_dir.clone();
            tokio::spawn(async move {
                if let Err(e) = process_call(dialog, IpAddr::V4(local), dir).await {
                    warn!(error = %e, "call handling failed");
                }
            });
        }
    };

    info!(
        username = %args.username,
        server = %args.server,
        output_dir = %args.output_dir.display(),
        "SIP answerer started, waiting for calls",
    );

    let mut reg = Box::pin(client.process_register());
    select! {
        r = &mut reg => {
            warn!(result = ?r, "register loop exited unexpectedly");
        }
        r = client.run_incoming_loop(&state_sender) => {
            if let Err(e) = r {
                warn!(error = %e, "incoming request loop error");
            }
        }
        r = run_dialog_state_loop(client.dialog_layer.clone(), state_receiver, on_incoming_call) => {
            if let Err(e) = r {
                warn!(error = %e, "dialog state loop error");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl+c received, shutting down");
        }
    }

    // 标准退出: 停注册循环 (best-effort 注销) + 收尾
    info!("stopping (unregister)");
    client.shutdown(reg).await
}

/// 一路通话: 解析 offer → ringing + accept (带 answer SDP) → 起 RTP
/// 接收器存盘 → 等对端 BYE (dialog cancel_token) 或本地 Ctrl+C (发 BYE).
async fn process_call(dialog: InviteDialog, local_ip: IpAddr, output_dir: PathBuf) -> Result<()> {
    let offer = parse_offer_all(dialog.initial_request().body())?;
    info!(?offer, "incoming offer");

    // 只接 G.711 音频和 H264/H265 视频; 不支持的编码那一路不存盘
    // (answer 里仍带着, 对端发了也收, 只是不落盘)
    let audio_pt = match offer.audio.as_ref() {
        Some(a) => match ipcam_gst::sendable_audio_codec(&a.codec) {
            Some(c) => Some((a.payload_type, c)),
            None => {
                warn!(codec = %a.codec, "unsupported audio codec, skip saving audio");
                None
            }
        },
        None => None,
    };
    let video = match offer.video.as_ref() {
        Some(v) => match VideoCodec::from_name(&v.codec) {
            c @ (VideoCodec::H264 | VideoCodec::H265) => Some((v.payload_type, c)),
            other => {
                warn!(codec = %v.codec, ?other, "unsupported video codec, skip saving video");
                None
            }
        },
        None => None,
    };

    // answer 的 pt 取 offer 里的值
    let answer = build_av_answer(
        local_ip,
        AUDIO_RTP_PORT,
        VIDEO_RTP_PORT,
        audio_pt.map(|(pt, _)| pt).unwrap_or(PT_PCMA),
        video.map(|(pt, _)| pt).unwrap_or(96),
    );
    let headers = vec![Header::ContentType("application/sdp".into())];
    let answer_body = answer.to_string().into_bytes();
    dialog.ringing(Some(headers.clone()), Some(answer_body.clone()))?;
    dialog.accept(Some(headers), Some(answer_body))?;
    info!("call accepted, answer SDP:\n{}", answer);

    if audio_pt.is_none() && video.is_none() {
        warn!("no supported media in offer, call kept up without recording");
    }
    // 音视频合进同一个 mp4; G.711 音频会被转码成 opus (mp4 不认 G.711)
    let receiver = match start_rtp_receiver(RtpRecvConfig {
        path: output_dir.join("call.mp4"),
        video: video.map(|(_, c)| (VIDEO_RTP_PORT, c)),
        audio: audio_pt.map(|(_, c)| (AUDIO_RTP_PORT, c)),
    }) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!(error = %e, "failed to start RTP receiver");
            None
        }
    };

    // 对端 BYE → dialog.handle 处理完 cancel_token 关闭; 本地 Ctrl+C →
    // 主动发 BYE. 全局退出由 main 的 select 兜底
    select! {
        _ = dialog.cancel_token().cancelled() => {
            info!("call ended by peer");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl+c, sending BYE");
            dialog.bye_with_headers(None).await?;
        }
    }
    if let Some(r) = receiver {
        r.stop();
    }
    info!("call finished");
    Ok(())
}
