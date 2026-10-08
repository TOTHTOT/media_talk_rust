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

/// 启动日志 + CLI 解析 + 创建输出目录; 关键步骤:
///   1. 解析 CLI (`Args`), `tracing_subscriber` 走 RUST_LOG
///   2. 选本地 IPv4 (--local-ip 显式给, 否则 `ipcam_sip::local_ipv4()`)
///   3. `SipClient::new` 注册循环, `spawn_endpoint` 拉起收包 task
///   4. 建路由表 `CallRoutes`, 把来电/更新转发到对应通话任务
///   5. `select!` 同时等: 注册循环 / 入向请求 / dialog 状态 / Ctrl+C
///   6. 任一分支退出 → `client.stop()` 让 endpoint 收包循环收尾
///
/// 注意: select! 里的 `reg` future 被 pin, 退出时已被消费,
/// 不能 `shutdown(reg).await` (double-await panic). 只需要
/// `client.stop()` 让 cancel token 通知 endpoint 即可.
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

/// 协商结果: (选中的音频 pt+编码, 选中的视频 pt+编码+fmtp, answer 文本)
type NegotiatedAnswer = (
    Option<(u8, ipcam_core::AudioCodec)>,
    Option<(u8, VideoCodec, Option<String>)>,
    String,
);

/// 从 offer 协商编码并构建 answer SDP (初始 INVITE 和 re-INVITE 共用).
///
/// 只接 G.711 音频和 H264/H265 视频; offer 的 fmt 列表按对端偏好排序,
/// 从前到后挑第一个我们支持的 (典型 offer 是 opus 打头, PCMU/PCMA 跟在
/// 后面); answer 的 pt/fmtp 回显选中的编码 (RFC 3264). 不支持的编码那
/// 一路不存盘 (answer 里仍带着, 对端发了也收, 只是不落盘).
///
/// 选编码时复用 ipcam_gst::sendable_audio_codec 的 G.711 白名单; 视频
/// 只接 H264/H265, 其他编 (VP8/VP9/AV1) 整路拒收, 但 SDP 仍带 audio
/// 给对端一个能落地的 answer.
///
/// # Arguments
///
/// * `offer`: 解析后的offer SDP媒体列表, 包含音视频地址和编码
/// * `local_ip`: 本机IPv4地址, answer SDP里的c=行用它
///
/// returns: NegotiatedAnswer
///
/// # Examples
///
/// ```
///
/// ```
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

/// 提取麦克风回传所需参数: 对端收音频地址, pt, 编码.
///
/// 用于指纹比对: re-INVITE 之后这三个字段任一变了就重启 sender (地址
/// 变了 udpsink 找不到对端, pt/编码变了 RTP 头和 depay 链对不上).
///
/// # Arguments
///
/// * `offer`: 解析后的offer SDP, 从中找对端音频地址和编码参数
///
/// returns: Option<(SocketAddr, u8, AudioCodec)>
///
/// # Examples
///
/// ```
///
/// ```
fn mic_params(offer: &OfferMedias) -> Option<ipcam_gst::AudioDest> {
    offer.audio.as_ref().and_then(|a| {
        a.codecs
            .iter()
            .find_map(|c| ipcam_gst::sendable_audio_codec(&c.codec).map(|ac| (a, c, ac)))
            .map(|(a, c, ac)| ipcam_gst::AudioDest {
                addr: SocketAddr::new(a.addr, a.port),
                payload_type: c.payload_type,
                codec: ac,
            })
    })
}

/// 起麦克风回传: 在 wasapisrc/alsasrc 上拉一条 RTP 发送链, 把本机麦的
/// 声音灌给对端给的地址.
///
/// 没有可用编码或没有麦克风设备就返回 None, 通话继续 (只收不发).
/// 失败会 warn 但不抛 — 通话不因麦克风挂掉.
///
/// # Arguments
///
/// * `dest`: 对端收音频的地址、payload type和编码, 直接来自AudioDest
///
/// returns: Option<RtpSender>
///
/// # Examples
///
/// ```
///
/// ```
fn start_mic_sender(dest: ipcam_gst::AudioDest) -> Option<RtpSender> {
    match ipcam_gst::start_rtp_sender(ipcam_gst::RtpSendConfig {
        audio: Some((ipcam_gst::TrackSource::Mic, dest)),
        video: None,
    }) {
        Ok(s) => {
            info!(%dest.addr, dest.payload_type, "mic sender started");
            Some(s)
        }
        Err(e) => {
            warn!(error = %e, "failed to start mic sender, receive only");
            None
        }
    }
}

/// 起 RTP 接收器: 从 AUDIO_RTP_PORT / VIDEO_RTP_PORT 收对端的流, 同时
/// 干两件事 — 存到 `output_dir/call.ts`, 本地播放到默认音频输出/屏幕.
///
/// 内部走 mp4mux(实为 mpegtsmux, 改名 ts 容器) + tee: 音视频合进同
/// 一个 ts 文件; G.711 音频在 mux 前转 opus (mp4 不认 G.711, ts 容器
/// 也按这套路避免出坑).
///
/// 失败返回 None, 通话继续 — 跟 start_mic_sender 一样不抛错.
/// 重启接收器 (re-INVITE 改编码) 时调用方负责先 stop 旧的再调本函数.
///
/// # Arguments
///
/// * `output_dir`: 保存音视频的目录路径, ts 文件写入此目录
/// * `audio`: 接收的音频编码, None 表示不接收音频
/// * `video`: 接收的视频编码, None 表示不接收视频
///
/// returns: Option<RtpReceiver>
///
/// # Examples
///
/// ```
///
/// ```
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

/// 一路通话生命周期: 解析 offer → ringing + accept (带 answer SDP) →
/// 起 RTP 接收器 (存盘 + 本地播放) + 麦克风回传. 通话中的 re-INVITE
/// 经 run_dialog_state_loop → 路由表 → update_rx 进到这里: 重新协商,
/// 编码/对端地址变了就重启对应的媒体管线, 应答 body 走 oneshot 回去.
///
/// 退出条件 (loop 里的 select):
///   - 对端发 BYE → dialog.cancel_token() 触发, 收尾
///   - 本地 Ctrl+C → 主动 dialog.bye_with_headers 发 BYE
///   - update_rx 关闭 → 路由表摘掉 (理论上只有 cancel_token 才会到这)
///
/// 退出时先 stop sender 再 stop receiver, 顺序不能反 — receiver 在
/// stop() 里给 udpsrc 推 EOS 等 mux 写 moov, 跟 sender 无关; 但若
/// sender 没停, 它继续往外发 RTP, 对端以为我们没挂断.
///
/// # Arguments
///
/// * `dialog`: 来电InviteDialog, 包含初始offer和TU通道
/// * `local_ip`: 本机IPv4地址, 用于构建answer SDP
/// * `output_dir`: 音视频保存目录
/// * `mic`: true则开启麦克风回传, false则只收不发
/// * `update_rx`: re-INVITE更新消息通道, 由dialog状态循环写入
/// * `routes`: 通话路由表, 退出时需从中摘除本dialog
///
/// returns: Result<(), Error>
///
/// # Examples
///
/// ```
///
/// ```
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
