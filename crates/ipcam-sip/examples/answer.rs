//! SIP 被叫 (接听) 冒烟工具: 注册 → 等来电 INVITE → 解析 offer SDP →
//! ringing + accept → 起 RTP 接收器 (存 ts + 本地播放) + 麦克风回传.
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
use ipcam_sip::sdp::{OfferMedias, parse_offer_all};
use ipcam_sip::{SipClient, SipClientConfig, run_dialog_state_loop};
use rsipstack::dialog::DialogId;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::headers::Header;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
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
    tokio::fs::create_dir_all(&args.output_dir).await?;

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
            spawn_call(dialog, local, output_dir.clone(), mic, routes.clone())
        }
    };
    let on_update = {
        let routes = routes.clone();
        move |id: DialogId, req: rsipstack::sip::Request| forward_update(routes.clone(), id, req)
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

/// 来电入口: 建 re-INVITE 通道并登记路由表, spawn 独立任务跑通话,
/// 不阻塞状态循环和后续来电.
///
/// # Arguments
///
/// * `dialog`: rsipstack 建的 server 端 InviteDialog, 带初始 offer 和应答通道
/// * `local`: 本机 IPv4, 构建 answer SDP 的 c= 行用
/// * `output_dir`: 录像 ts 文件的落盘目录
/// * `mic`: 是否回传麦克风 (--no-mic 时为 false, 只收不发)
/// * `routes`: 全局通话路由表, 本函数写入 dialog id -> re-INVITE 通道
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
fn spawn_call(
    dialog: InviteDialog,
    local: Ipv4Addr,
    output_dir: PathBuf,
    mic: bool,
    routes: CallRoutes,
) {
    let (update_tx, update_rx) = mpsc::channel(4);
    routes.lock().unwrap().insert(dialog.id(), update_tx);
    tokio::spawn(async move {
        if let Err(e) = process_call(
            dialog,
            IpAddr::V4(local),
            output_dir,
            mic,
            update_rx,
            routes,
        )
        .await
        {
            warn!(error = %e, "call handling failed");
        }
    });
}

/// re-INVITE/UPDATE (中继会周期会话刷新): 按 dialog id 转发给通话任务
/// 重新协商, 编码/对端地址变了它会重启对应媒体管线. 不回的话 rsipstack
/// 兜底 501, 中继会停转发媒体 (实测视频断流) 甚至拆通话.
///
/// 通话任务卡死时不能堵住状态循环, 2s 超时后回不带 SDP 的 200.
///
/// # Arguments
///
/// * `routes`: 全局通话路由表, 按 dialog id 找通话任务
/// * `id`: 被更新的 dialog id (状态循环从 Updated 事件带出)
/// * `req`: re-INVITE/UPDATE 完整请求, body 里是新 offer SDP
///
/// returns: Option<Vec<u8>> — 回给对端的 200 OK body, Some 是 answer SDP, None 不带 SDP
///
/// # Examples
///
/// ```
///
/// ```
async fn forward_update(
    routes: CallRoutes,
    id: DialogId,
    req: rsipstack::sip::Request,
) -> Option<Vec<u8>> {
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
    match tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx).await {
        Ok(Ok(body)) => body,
        _ => {
            warn!(dialog = %id, "call task did not answer re-INVITE in time");
            None
        }
    }
}

/// 可收发音频谓词 (复用 ipcam_gst 发送白名单 G.711): 协商与指纹比对共用.
/// offer 的 fmt 列表按对端偏好排序, negotiate::pick_audio 从前到后挑第一个
/// 谓词接受的 (典型 offer 是 opus 打头, PCMU/PCMA 跟在后面).
fn audio_pred(name: &str) -> Option<AudioCodec> {
    ipcam_gst::sendable_audio_codec(name)
}

/// 可收视频谓词: 只接 H264/H265, 其他 (VP8/VP9/AV1) 整路拒收, 但 SDP 仍带
/// audio 给对端一个能落地的 answer.
fn video_pred(name: &str) -> Option<VideoCodec> {
    match VideoCodec::from_name(name) {
        vc @ (VideoCodec::H264 | VideoCodec::H265) => Some(vc),
        _ => None,
    }
}

/// 提取麦克风回传所需参数 (对端收音频地址, pt, 编码), 同时充当指纹:
/// re-INVITE 之后这三个字段任一变了就重启 sender (地址变了 udpsink 找
/// 不到对端, pt/编码变了 RTP 头和 depay 链对不上).
fn mic_params(offer: &OfferMedias) -> Option<ipcam_gst::AudioDest> {
    let a = offer.audio.as_ref()?;
    let (payload_type, codec) = ipcam_sip::negotiate::pick_audio(offer, audio_pred)?;
    Some(ipcam_gst::AudioDest {
        addr: SocketAddr::new(a.addr, a.port),
        payload_type,
        codec,
    })
}

/// 起麦克风回传: 在 wasapisrc/alsasrc 上拉一条 RTP 发送链, 把本机麦的
/// 声音灌给对端给的地址. socket 是通话的音频共享 socket (对称 RTP:
/// 源端口 == answer 声明的接收端口).
///
/// 没有麦克风设备就返回 None, 通话继续 (只收不发). 失败会 warn 但不
/// 抛 — 通话不因麦克风挂掉.
fn start_mic_sender(
    dest: ipcam_gst::AudioDest,
    socket: &ipcam_gst::SharedUdpSocket,
) -> Option<RtpSender> {
    match ipcam_gst::start_rtp_sender(ipcam_gst::RtpSendConfig {
        audio: Some((ipcam_gst::TrackSource::Mic, dest)),
        video: None,
        audio_socket: Some(socket.clone()),
        video_socket: None,
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
/// G.711 音频在 mux 前转 opus (TS 不认 G.711).
///
/// 失败返回 None, 通话继续 — 跟 start_mic_sender 一样不抛错.
/// 重启接收器 (re-INVITE 改编码) 时调用方负责先 stop 旧的再调本函数.
///
/// # Arguments
///
/// * `output_dir`: 录像 ts 的落盘目录 (写到 `<dir>/call.ts`)
/// * `audio`: 要收的音频编码, None 表示音频路不建
/// * `video`: 要收的视频编码, None 表示视频路不建
///
/// returns: Option<RtpReceiver> — 运行中的接收器句柄, stop 即拆管线
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
    ports: (u16, u16),
    sockets: &(ipcam_gst::SharedUdpSocket, ipcam_gst::SharedUdpSocket),
) -> Option<RtpReceiver> {
    match start_rtp_receiver(RtpRecvConfig {
        path: Some(output_dir.join("call.ts")),
        playback: true,
        video_tap: None,
        video: video.map(|c| (ports.1, c)),
        audio: audio.map(|c| (ports.0, c)),
        video_socket: video.map(|_| sockets.1.clone()),
        audio_socket: audio.map(|_| sockets.0.clone()),
    }) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!(error = %e, "failed to start RTP receiver");
            None
        }
    }
}

/// 一路通话的媒体面: RTP 接收器 + 麦克风回传, 各带一份指纹.
/// re-INVITE 来了按指纹比对, 变了才重启对应管线, 没变只回 answer.
struct CallMedia {
    output_dir: PathBuf,
    local_ip: IpAddr,
    mic: bool,
    receiver: Option<RtpReceiver>,
    sender: Option<RtpSender>,
    /// 接收端指纹: 只含编码. pt 不参与 — 接收器 udpsrc 不按 pt 过滤,
    /// 端口是我们在 answer 里声明的固定值, 不会变
    recv_fingerprint: (Option<AudioCodec>, Option<VideoCodec>),
    /// 回传指纹: (对端收音频地址, pt, 编码)
    send_fingerprint: Option<ipcam_gst::AudioDest>,
    /// 本机 RTP 接收端口 (音频, 视频): answer SDP 里声明的实际值
    /// (固定端口被占时回退过临时端口), re-INVITE 的 answer 要复用
    ports: (u16, u16),
    /// 对称 RTP 共享 socket (音频, 视频): udpsrc 收/udpsink 发同一对,
    /// 管线重启时复用, 端口在 dialog 存续期不变
    sockets: (ipcam_gst::SharedUdpSocket, ipcam_gst::SharedUdpSocket),
}

impl CallMedia {
    /// 建媒体面: 按指纹起接收器和麦克风回传. mic=false 或 offer 里没有
    /// 可发编码时 sender 为 None (只收不发), 不抛错.
    ///
    /// # Arguments
    ///
    /// * `output_dir`: 录像 ts 的落盘目录
    /// * `mic`: 是否回传麦克风
    /// * `recv_fingerprint`: 接收端指纹 (音频编码, 视频编码), 也是起接收器的输入
    /// * `send_fingerprint`: 回传指纹 (对端收音频地址, pt, 编码), None 表示不回传
    ///
    /// returns: Self
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    fn new(
        output_dir: PathBuf,
        local_ip: IpAddr,
        mic: bool,
        recv_fingerprint: (Option<AudioCodec>, Option<VideoCodec>),
        send_fingerprint: Option<ipcam_gst::AudioDest>,
        ports: (u16, u16),
        sockets: (ipcam_gst::SharedUdpSocket, ipcam_gst::SharedUdpSocket),
    ) -> Self {
        let receiver = start_receiver(
            &output_dir,
            recv_fingerprint.0,
            recv_fingerprint.1,
            ports,
            &sockets,
        );
        let sender = if mic {
            if send_fingerprint.is_none() {
                warn!("no sendable audio codec in offer, mic muted");
            }
            send_fingerprint.and_then(|dest| start_mic_sender(dest, &sockets.0))
        } else {
            info!("mic disabled (--no-mic)");
            None
        };
        Self {
            output_dir,
            local_ip,
            mic,
            receiver,
            sender,
            recv_fingerprint,
            send_fingerprint,
            ports,
            sockets,
        }
    }

    /// re-INVITE 重新协商: 编码变了重启接收器 (会重新落一个新的 ts
    /// 文件); 对端收音频地址/pt/编码变了重启麦克风回传.
    fn renegotiate(&mut self, offer: &OfferMedias) {
        let (audio, video, _) = ipcam_sip::negotiate::build_answer(
            offer,
            self.local_ip,
            self.ports.0,
            self.ports.1,
            audio_pred,
            video_pred,
        );
        let new_recv = (audio.map(|(_, c)| c), video.map(|(_, c, _)| c));
        if new_recv != self.recv_fingerprint {
            info!(old = ?self.recv_fingerprint, new = ?new_recv, "codec changed, restarting receiver");
            if let Some(r) = self.receiver.take() {
                r.stop();
            }
            self.receiver = start_receiver(
                &self.output_dir,
                new_recv.0,
                new_recv.1,
                self.ports,
                &self.sockets,
            );
            self.recv_fingerprint = new_recv;
        }
        let new_send = if self.mic { mic_params(offer) } else { None };
        if new_send != self.send_fingerprint {
            info!(old = ?self.send_fingerprint, new = ?new_send, "mic dest changed, restarting sender");
            if let Some(s) = self.sender.take() {
                s.stop();
            }
            self.sender = new_send.and_then(|dest| start_mic_sender(dest, &self.sockets.0));
            self.send_fingerprint = new_send;
        }
    }

    /// 先停 sender 再停 receiver, 顺序不能反 — sender 没停会继续往外
    /// 发 RTP, 对端以为我们没挂断.
    fn stop(mut self) {
        if let Some(s) = self.sender.take() {
            s.stop();
        }
        if let Some(r) = self.receiver.take() {
            r.stop();
        }
    }
}

/// 绑本路通话的 RTP socket 对: 优先固定端口 (AUDIO_RTP_PORT /
/// VIDEO_RTP_PORT), 被占 (同机另一个通话进程) 回退临时端口.
/// socket 由收发管线共享 (对称 RTP: 源端口 == answer 声明的接收端口),
/// 返回附带实际绑定的 (音频端口, 视频端口) — 写 answer SDP 用
fn bind_media_sockets(
    local_ip: IpAddr,
) -> Result<(
    ipcam_gst::SharedUdpSocket,
    ipcam_gst::SharedUdpSocket,
    (u16, u16),
)> {
    let bind = |audio_port: u16, video_port: u16| {
        let audio = ipcam_gst::bind_udp_socket(local_ip, audio_port)?;
        let video = ipcam_gst::bind_udp_socket(local_ip, video_port)?;
        Ok::<_, std::io::Error>((audio, video))
    };
    let (audio, video) = match bind(AUDIO_RTP_PORT, VIDEO_RTP_PORT) {
        Ok(pair) => pair,
        Err(e) => {
            warn!(error = %e, "fixed RTP ports unavailable, fall back to ephemeral");
            bind(0, 0)?
        }
    };
    let ports = (
        ipcam_gst::udp_socket_port(&audio)?,
        ipcam_gst::udp_socket_port(&video)?,
    );
    Ok((audio, video, ports))
}

/// 一路通话生命周期: 解析 offer → ringing + accept (带 answer SDP) →
/// 起媒体 (CallMedia: 接收存盘+播放, 麦克风回传). 通话中的 re-INVITE
/// 经 run_dialog_state_loop → 路由表 → update_rx 进到这里: 重新协商,
/// 按需重启管线, 应答 body 走 oneshot 回去.
///
/// 退出条件 (loop 里的 select):
///   - 对端发 BYE → dialog.cancel_token() 触发, 收尾
///   - 本地 Ctrl+C → 主动 dialog.bye_with_headers 发 BYE
///   - update_rx 关闭 → 路由表摘掉 (理论上只有 cancel_token 才会到这)
///
/// # Arguments
///
/// * `dialog`: 这通电话的 server 端 InviteDialog (取初始 offer, ringing/accept/BYE 都走它)
/// * `local_ip`: 本机 IPv4, 构建 answer 用
/// * `output_dir`: 录像 ts 落盘目录
/// * `mic`: 是否回传麦克风
/// * `update_rx`: re-INVITE 消息接收端 (状态循环经路由表写入)
/// * `routes`: 全局路由表, 函数退出前摘掉本 dialog
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

    // 端口要写进 answer SDP, socket 绑好后活到通话结束 (收发共享)
    let (audio_sock, video_sock, ports) = bind_media_sockets(local_ip)?;
    let (audio_pt, video, answer) = ipcam_sip::negotiate::build_answer(
        &offer,
        local_ip,
        ports.0,
        ports.1,
        audio_pred,
        video_pred,
    );
    let headers = vec![Header::ContentType("application/sdp".into())];
    let answer_body = answer.clone().into_bytes();
    dialog.ringing(Some(headers.clone()), Some(answer_body.clone()))?;
    dialog.accept(Some(headers), Some(answer_body))?;
    info!("call accepted, answer SDP:\n{}", answer);

    if audio_pt.is_none() && video.is_none() {
        warn!("no supported media in offer, call kept up without recording");
    }

    let mut media = CallMedia::new(
        output_dir,
        local_ip,
        mic,
        (audio_pt.map(|(_, c)| c), video.map(|(_, c, _)| c)),
        mic_params(&offer),
        ports,
        (audio_sock, video_sock),
    );

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
                        let (_, _, answer) = ipcam_sip::negotiate::build_answer(
                            &new_offer,
                            local_ip,
                            media.ports.0,
                            media.ports.1,
                            audio_pred,
                            video_pred,
                        );
                        info!("re-INVITE accepted, answer SDP:\n{}", answer);
                        media.renegotiate(&new_offer);
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
    media.stop();
    routes.lock().unwrap().remove(&dialog.id());
    info!("call finished");
    Ok(())
}
