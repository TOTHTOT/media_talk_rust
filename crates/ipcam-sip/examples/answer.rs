//! SIP 被叫 (接听) 冒烟工具: 注册 → 等来电 INVITE → 解析 offer SDP →
//! ringing + accept → 起 RTP 接收器保存音视频. 对端 BYE 或本地 Ctrl+C
//! 结束通话.
//!
//! 结构参考 rsipstack examples/client/main.rs, 分三层:
//! - process_incoming_request: 事务层分发 (in-dialog → match_dialog 交给
//!   已有 dialog; 新 INVITE → 建 server dialog 并 spawn handle)
//! - process_dialog: dialog 状态循环, Calling(server 角色) → 起通话任务,
//!   Terminated → 从 dialog_layer 移除
//! - process_call: 每通电话一个独立任务, 不阻塞后续来电
//!
//! ```bash
//! cargo run -p ipcam-sip --example answer
//! ```

use anyhow::Result;
use clap::Parser;
use ipcam_core::{AudioCodec, VideoCodec};
use ipcam_gst::{RtpRecvConfig, start_rtp_receiver};
use ipcam_sip::sdp::{PT_PCMA, build_av_answer, parse_offer_all};
use ipcam_sip::{SipClient, SipClientConfig};
use rsipstack::dialog::dialog::{Dialog, DialogState, DialogStateReceiver, DialogStateSender};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip as rsip;
use rsipstack::sip::HeadersExt;
use rsipstack::sip::headers::Header;
use rsipstack::transaction::TransactionReceiver;
use rsipstack::transaction::key::TransactionRole;
use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::Arc;
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
    let client = SipClient::new(config, CancellationToken::new()).await?;

    // endpoint 收包循环甩后台 (从 inner 重建 owned Endpoint, 同 call example)
    let ep = rsipstack::transaction::Endpoint {
        inner: client.endpoint.inner.clone(),
    };
    tokio::spawn(async move { ep.serve().await });

    let dialog_layer = Arc::new(DialogLayer::new(client.endpoint.inner.clone()));
    let (state_sender, state_receiver) = dialog_layer.new_dialog_state_channel();
    let incoming = client.endpoint.incoming_transactions()?;

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
        r = process_incoming_request(
            dialog_layer.clone(), incoming, state_sender, client.contact.clone(),
        ) => {
            if let Err(e) = r {
                warn!(error = %e, "incoming request loop error");
            }
        }
        r = process_dialog(
            dialog_layer.clone(), state_receiver, IpAddr::V4(local), args.output_dir.clone(),
        ) => {
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
    client.stop();
    let r = reg.await;
    info!(result = ?r, "register loop exited");
    Ok(())
}

/// 事务层分发: 只做路由, 不碰 SDP/媒体. in-dialog 请求 (To 带 tag) 交给
/// 已跟踪的 dialog; 新 INVITE 建 server dialog, handle 甩进独立任务 --
/// dialog 生命周期事件走状态通道, 由 process_dialog 接管.
async fn process_incoming_request(
    dialog_layer: Arc<DialogLayer>,
    mut incoming: TransactionReceiver,
    state_sender: DialogStateSender,
    contact: rsip::Uri,
) -> Result<()> {
    while let Some(mut tx) = incoming.recv().await {
        info!(key = ?tx.key, method = %tx.original.method, "received transaction");

        // to_header().and_then(h.tag()) 返回 Some(None) 时 flatten 后是 None
        let has_to_tag = tx
            .original
            .to_header()
            .ok()
            .and_then(|h| h.tag().ok())
            .flatten()
            .is_some();

        if has_to_tag {
            match dialog_layer.match_dialog(&tx) {
                Some(mut dialog) => {
                    tokio::spawn(async move {
                        if let Err(e) = dialog.handle(&mut tx).await {
                            warn!(error = %e, "dialog handle failed");
                        }
                    });
                }
                None => {
                    info!("dialog not found for in-dialog request");
                    let _ = tx
                        .reply(rsip::StatusCode::CallTransactionDoesNotExist)
                        .await;
                }
            }
            continue;
        }

        match tx.original.method {
            rsip::Method::Invite => {
                let mut dialog = match dialog_layer.get_or_create_server_invite(
                    &tx,
                    state_sender.clone(),
                    None,
                    Some(contact.clone()),
                ) {
                    Ok(d) => d,
                    Err(e) => {
                        warn!(error = %e, "failed to create server invite dialog");
                        let _ = tx.reply(rsip::StatusCode::BusyHere).await;
                        continue;
                    }
                };
                tokio::spawn(async move {
                    if let Err(e) = dialog.handle(&mut tx).await {
                        warn!(error = %e, "dialog handle failed");
                    }
                });
            }
            // 非 INVITE 新请求 (OPTIONS 保活之类), 回 OK 完事
            _ => {
                let _ = tx.reply(rsip::StatusCode::OK).await;
            }
        }
    }
    Ok(())
}

/// dialog 状态循环: 来电 (Calling + server 角色) 起独立通话任务,
/// Terminated 从 dialog_layer 移除 (BYE 由 dialog.handle 处理完会走到这).
async fn process_dialog(
    dialog_layer: Arc<DialogLayer>,
    mut state_receiver: DialogStateReceiver,
    local_ip: IpAddr,
    output_dir: PathBuf,
) -> Result<()> {
    while let Some(state) = state_receiver.recv().await {
        info!(%state, "dialog state");
        match state {
            DialogState::Calling(id) => {
                let Some(Dialog::Invite(d)) = dialog_layer.get_dialog(&id) else {
                    continue;
                };
                if d.role() != TransactionRole::Server {
                    continue;
                }
                let dir = output_dir.clone();
                tokio::spawn(async move {
                    if let Err(e) = process_call(d, local_ip, dir).await {
                        warn!(error = %e, "call handling failed");
                    }
                });
            }
            DialogState::Terminated(id, reason) => {
                info!(dialog = %id, ?reason, "dialog terminated");
                dialog_layer.remove_dialog(&id);
            }
            _ => {}
        }
    }
    Ok(())
}

/// 一路通话: 解析 offer → ringing + accept (带 answer SDP) → 起 RTP
/// 接收器存盘 → 等对端 BYE (dialog cancel_token) 或本地 Ctrl+C (发 BYE).
async fn process_call(dialog: InviteDialog, local_ip: IpAddr, output_dir: PathBuf) -> Result<()> {
    let offer = parse_offer_all(dialog.initial_request().body())?;
    info!(?offer, "incoming offer");

    // 只接 G.711 音频和 H264/H265 视频; 不支持的编码那一路不存盘
    // (answer 里仍带着, 对端发了也收, 只是不落盘)
    let audio_pt = match offer.audio.as_ref() {
        Some(a) => match AudioCodec::from_name(&a.codec) {
            c @ (AudioCodec::G711A | AudioCodec::G711U) => Some((a.payload_type, c)),
            other => {
                warn!(codec = %a.codec, ?other, "unsupported audio codec, skip saving audio");
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
    let receiver = match start_rtp_receiver(RtpRecvConfig {
        video_path: video.map(|_| output_dir.join("video.ts")),
        audio_path: audio_pt.map(|_| output_dir.join("audio.wav")),
        video_port: VIDEO_RTP_PORT,
        audio_port: AUDIO_RTP_PORT,
        video_codec: video.map(|(_, c)| c).unwrap_or(VideoCodec::H264),
        audio_codec: audio_pt.map(|(_, c)| c).unwrap_or(AudioCodec::G711A),
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
