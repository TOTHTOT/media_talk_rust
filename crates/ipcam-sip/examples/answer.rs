//! SIP 被叫 (接听) 冒烟工具: 注册 → 等来电 INVITE → 解析 offer SDP →
//! ringing + accept → 起 RTP 接收器 (存 mp4 + 本地播放) + 麦克风回传.
//! 对端 BYE 或本地 Ctrl+C 结束通话.
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
use ipcam_core::{AudioCodec, VideoCodec};
use ipcam_gst::{RtpReceiver, RtpRecvConfig, RtpSender, start_rtp_receiver};
use ipcam_sip::sdp::{OfferMedias, PT_PCMA, build_av_answer, parse_offer_all};
use ipcam_sip::{SipClient, SipClientConfig, run_dialog_state_loop};
use rsipstack::dialog::DialogId;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::headers::Header;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::select;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// 接收器监听端口 (冒烟工具一次只接一路, 固定即可)
const AUDIO_RTP_PORT: u16 = 40000;
const VIDEO_RTP_PORT: u16 = 40002;

/// re-INVITE 转发给通话任务的消息: 新 offer 请求 + 应答 oneshot
type UpdateMsg = (rsipstack::sip::Request, oneshot::Sender<Option<Vec<u8>>>);
/// 进行中的通话路由表: dialog id -> 通话任务的 re-INVITE 通道
type CallRoutes = Arc<Mutex<HashMap<DialogId, mpsc::Sender<UpdateMsg>>>>;

#[derive(Parser)]
#[command(about = "SIP 被叫冒烟: 注册 → 等来电 → 播放 + 存盘 + 麦克风回传")]
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
    /// 强制 client socket 绑到指定 IPv4 网卡. 默认走
    /// `ipcam_sip::local_ipv4()` (取本机第一个非 loopback IPv4),
    /// 多网卡 / VPN / Tailscale 场景下可能选错 (例如选到 100.x Tailscale
    /// 而对端物理网段不可达), 此时用这个选项显式指定 192.168.x.x 网卡
    #[arg(long)]
    local_ip: Option<IpAddr>,
    /// 不回传麦克风给对方 (默认回传; 没接 AEC, 同机外放+麦克风会回声,
    /// 测试建议插耳机)
    #[arg(long)]
    no_mic: bool,
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

    // 多网卡机器使用 local_ip 参数绑定ip
    let local = match args.local_ip {
        Some(IpAddr::V4(v4)) => v4,
        Some(_) => anyhow::bail!("only support IPv4 for now!"),
        None => ipcam_sip::local_ipv4()?,
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
    let _endpoint = client.spawn_endpoint();

    let (state_sender, state_receiver) = client.dialog_layer.new_dialog_state_channel();
    // 进行中通话的路由表: re-INVITE 按 dialog id 转发给对应通话任务
    let routes: CallRoutes = Arc::new(Mutex::new(HashMap::new()));
    // 每通来电 spawn 独立任务处理, 不阻塞状态循环和后续来电
    let on_incoming_call = {
        let output_dir = args.output_dir.clone();
        let mic = !args.no_mic;
        let routes = routes.clone();
        move |dialog: InviteDialog| {
            let dir = output_dir.clone();
            let routes = routes.clone();
            let (update_tx, update_rx) = mpsc::channel(4);
            routes.lock().unwrap().insert(dialog.id(), update_tx);
            tokio::spawn(async move {
                if let Err(e) =
                    process_call(dialog, IpAddr::V4(local), dir, mic, update_rx, routes).await
                {
                    warn!(error = %e, "call handling failed");
                }
            });
        }
    };
    // re-INVITE/UPDATE (中继会周期会话刷新): 转发给通话任务重新协商,
    // 编码/对端地址变了就重启对应媒体管线. 不回的话 rsipstack 兜底 501,
    // 中继会停转发媒体 (实测视频断流) 甚至拆通话
    let on_update = {
        let routes = routes.clone();
        move |id: DialogId, req: rsipstack::sip::Request| {
            let routes = routes.clone();
            async move {
                let tx = routes.lock().unwrap().get(&id).cloned();
                let Some(tx) = tx else {
                    warn!(dialog = %id, "re-INVITE for unknown dialog, reply 200 without SDP");
                    return None;
                };
                let (reply_tx, reply_rx) = oneshot::channel();
                if tx.send((req, reply_tx)).await.is_err() {
                    warn!(dialog = %id, "call task gone, reply 200 without SDP");
                    return None;
                }
                // 通话任务卡死时不能堵住状态循环, 超时让 rsipstack 兜底
                match tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx).await {
                    Ok(Ok(body)) => body,
                    _ => {
                        warn!(dialog = %id, "call task did not answer re-INVITE in time");
                        None
                    }
                }
            }
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
        r = run_dialog_state_loop(client.dialog_layer.clone(), state_receiver, on_incoming_call, on_update) => {
            if let Err(e) = r {
                warn!(error = %e, "dialog state loop error");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl+c received, shutting down");
        }
    }

    // select! 里的分支完成后, reg future 已被 select! 消费,
    // 不能 shutdown(reg).await (会 double-await panic).
    // 只需 cancel token 让 endpoint 收包循环退出即可.
    info!("stopping");
    client.stop();
    Ok(())
}

/// 从 offer 协商编码并构建 answer SDP (初始 INVITE 和 re-INVITE 共用).
/// 只接 G.711 音频和 H264/H265 视频; offer 的 fmt 列表按对端偏好排序,
/// 从前到后挑第一个我们支持的 (典型 offer 是 opus 打头, PCMU/PCMA 跟在
/// 后面); answer 的 pt/fmtp 回显选中的编码 (RFC 3264). 不支持的编码那
/// 一路不存盘 (answer 里仍带着, 对端发了也收, 只是不落盘).
/// 返回 (选中的音频 pt+编码, 选中的视频 pt+编码+fmtp, answer 文本)
/// 协商结果: (选中的音频 pt+编码, 选中的视频 pt+编码+fmtp, answer 文本)
type NegotiatedAnswer = (
    Option<(u8, ipcam_core::AudioCodec)>,
    Option<(u8, VideoCodec, Option<String>)>,
    String,
);

fn negotiate_answer(offer: &ipcam_sip::sdp::OfferMedias, local_ip: IpAddr) -> NegotiatedAnswer {
    let audio_pt = match offer.audio.as_ref() {
        Some(a) => match a
            .codecs
            .iter()
            .find_map(|c| ipcam_gst::sendable_audio_codec(&c.codec).map(|ac| (c, ac)))
        {
            Some((c, ac)) => Some((c.payload_type, ac)),
            None => {
                warn!(codecs = ?a.codecs, "no supported audio codec in offer, skip saving audio");
                None
            }
        },
        None => None,
    };
    let video = match offer.video.as_ref() {
        Some(v) => match v
            .codecs
            .iter()
            .find_map(|c| match VideoCodec::from_name(&c.codec) {
                vc @ (VideoCodec::H264 | VideoCodec::H265) => Some((c, vc)),
                _ => None,
            }) {
            Some((c, vc)) => Some((c.payload_type, vc, c.fmtp.clone())),
            None => {
                warn!(codecs = ?v.codecs, "no supported video codec in offer, skip saving video");
                None
            }
        },
        None => None,
    };

    let answer = build_av_answer(
        local_ip,
        AUDIO_RTP_PORT,
        VIDEO_RTP_PORT,
        audio_pt.map(|(pt, _)| pt).unwrap_or_else(|| {
            warn!("audio_pt unwarp failed, used: {}", PT_PCMA);
            PT_PCMA
        }),
        video.as_ref().map(|(pt, _, _)| *pt).unwrap_or_else(|| {
            warn!("video unwarp failed, used: {}", 96);
            96
        }),
        video.as_ref().and_then(|(_, _, fmtp)| fmtp.as_deref()),
    );
    (audio_pt, video, answer.to_string())
}

/// 麦克风回传参数指纹: (对端收音频地址, pt, 编码). re-INVITE 后变了
/// 才重启 sender
fn mic_params(offer: &OfferMedias) -> Option<(SocketAddr, u8, AudioCodec)> {
    offer.audio.as_ref().and_then(|a| {
        a.codecs
            .iter()
            .find_map(|c| ipcam_gst::sendable_audio_codec(&c.codec).map(|ac| (a, c, ac)))
            .map(|(a, c, ac)| (SocketAddr::new(a.addr, a.port), c.payload_type, ac))
    })
}

/// 起麦克风回传. 没有可用编码或没有麦克风设备就只收不发, 通话继续
fn start_mic_sender(params: (SocketAddr, u8, AudioCodec)) -> Option<RtpSender> {
    let (addr, payload_type, codec) = params;
    match ipcam_gst::start_rtp_sender(ipcam_gst::RtpSendConfig {
        audio: Some((
            ipcam_gst::TrackSource::Mic,
            ipcam_gst::AudioDest {
                addr,
                payload_type,
                codec,
            },
        )),
        video: None,
    }) {
        Ok(s) => {
            info!(%addr, payload_type, "mic sender started");
            Some(s)
        }
        Err(e) => {
            warn!(error = %e, "failed to start mic sender, receive only");
            None
        }
    }
}

/// 起 RTP 接收器 (音视频合进同一个 ts, G.711 转码 opus; 同时 tee 出
/// 播放链送本机扬声器/屏幕)
fn start_receiver(
    output_dir: &std::path::Path,
    audio: Option<AudioCodec>,
    video: Option<VideoCodec>,
) -> Option<RtpReceiver> {
    match start_rtp_receiver(RtpRecvConfig {
        path: Some(output_dir.join("call.ts")),
        playback: true,
        video: video.map(|c| (VIDEO_RTP_PORT, c)),
        audio: audio.map(|c| (AUDIO_RTP_PORT, c)),
    }) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!(error = %e, "failed to start RTP receiver");
            None
        }
    }
}

/// 一路通话: 解析 offer → ringing + accept (带 answer SDP) → 起 RTP
/// 接收器 (存盘 + 本地播放) + 麦克风回传. 通话中的 re-INVITE 经
/// run_dialog_state_loop → 路由表 → update_rx 进到这里: 重新协商,
/// 编码/对端地址变了就重启对应的媒体管线, 应答 body 走 oneshot 回去.
async fn process_call(
    dialog: InviteDialog,
    local_ip: IpAddr,
    output_dir: PathBuf,
    mic: bool,
    mut update_rx: mpsc::Receiver<UpdateMsg>,
    routes: CallRoutes,
) -> Result<()> {
    let offer = parse_offer_all(dialog.initial_request().body())?;
    info!(?offer, "incoming offer");

    let (audio_pt, video, answer) = negotiate_answer(&offer, local_ip);
    let headers = vec![Header::ContentType("application/sdp".into())];
    let answer_body = answer.clone().into_bytes();
    dialog.ringing(Some(headers.clone()), Some(answer_body.clone()))?;
    dialog.accept(Some(headers), Some(answer_body))?;
    info!("call accepted, answer SDP:\n{}", answer);

    if audio_pt.is_none() && video.is_none() {
        warn!("no supported media in offer, call kept up without recording");
    }

    // 接收端指纹: 只含编码. pt 不参与 — 接收器 udpsrc 不按 pt 过滤,
    // 端口是我们在 answer 里声明的固定值, 不会变
    let mut recv_fingerprint = (audio_pt.map(|(_, c)| c), video.map(|(_, c, _)| c));
    let mut receiver = start_receiver(&output_dir, recv_fingerprint.0, recv_fingerprint.1);

    let mut send_fingerprint = if mic { mic_params(&offer) } else { None };
    let mut sender = send_fingerprint.and_then(start_mic_sender);
    if mic && send_fingerprint.is_none() {
        warn!("no sendable audio codec in offer, mic muted");
    }
    if !mic {
        info!("mic disabled (--no-mic)");
    }

    // 对端 BYE → dialog.cancel_token 取消; 本地 Ctrl+C → 发 BYE;
    // re-INVITE → 重新协商, 按需重启媒体
    loop {
        select! {
            _ = dialog.cancel_token().cancelled() => {
                info!("call ended by peer");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl+c, sending BYE");
                dialog.bye_with_headers(None).await?;
                break;
            }
            msg = update_rx.recv() => {
                let Some((req, reply)) = msg else { break };
                let body = match parse_offer_all(req.body()) {
                    Ok(new_offer) => {
                        let (na, nv, answer) = negotiate_answer(&new_offer, local_ip);
                        info!("re-INVITE accepted, answer SDP:\n{}", answer);
                        // 编码变了 → 重启接收器 (会重新落一个新的 ts 文件)
                        let new_recv = (na.map(|(_, c)| c), nv.map(|(_, c, _)| c));
                        if new_recv != recv_fingerprint {
                            info!(old = ?recv_fingerprint, new = ?new_recv, "codec changed, restarting receiver");
                            if let Some(r) = receiver.take() {
                                r.stop();
                            }
                            receiver = start_receiver(&output_dir, new_recv.0, new_recv.1);
                            recv_fingerprint = new_recv;
                        }
                        // 对端收音频的地址/pt/编码变了 → 重启麦克风回传
                        let new_send = if mic { mic_params(&new_offer) } else { None };
                        if new_send != send_fingerprint {
                            info!(old = ?send_fingerprint, new = ?new_send, "mic dest changed, restarting sender");
                            if let Some(s) = sender.take() {
                                s.stop();
                            }
                            sender = new_send.and_then(start_mic_sender);
                            send_fingerprint = new_send;
                        }
                        Some(answer.into_bytes())
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to parse re-INVITE offer, reply 200 without SDP");
                        None
                    }
                };
                let _ = reply.send(body);
            }
        }
    }
    if let Some(s) = sender {
        s.stop();
    }
    if let Some(r) = receiver {
        r.stop();
    }
    routes.lock().unwrap().remove(&dialog.id());
    info!("call finished");
    Ok(())
}
