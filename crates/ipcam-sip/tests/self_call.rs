//! 自收自发集成测试: 本机 SipClient 绑 127.0.0.1, IP 直拨自己, 被叫侧
//! 自动接听, 主叫侧把 assets/oceans.mp4 的音视频按协商结果发回去, 被叫
//! 侧收流计数 + 视频 tap 计帧 + 落盘 mp4, 断言全链路通畅.
//! 回归保险: 信令 (INVITE/200/ACK/BYE) / SDP 协商 / RTP 收发任一环节
//! 改坏都会挂这个测试.
//!
//! 需要本机装 GStreamer (plugins-base/good/bad/ugly/libav), CI rust.yml
//! 的 test job 已装. 不需要声卡/显示器: 视频走 tap, 音频走 mp4 存盘,
//! 不碰 autoaudiosink/autovideosink.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use ipcam_core::{AudioCodec, VideoCodec};
use ipcam_gst::{
    AudioDest, RtpDest, RtpRecvConfig, RtpSendConfig, TrackSource, VideoFrameSink,
    sendable_audio_codec, start_rtp_receiver, start_rtp_sender,
};
use ipcam_sip::sdp::{PT_PCMA, PT_PCMU, build_av_offer, parse_answer_all, parse_offer_all};
use ipcam_sip::{SipClient, SipClientConfig, negotiate, run_dialog_state_loop};
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip as rsip;
use rsipstack::sip::headers::Header;
use tokio_util::sync::CancellationToken;

/// 被叫 (收流) 端口; 主叫 offer 里声明的端口收不到流 (被叫不回传), 仅作占位
const CALLEE_AUDIO_PORT: u16 = 43600;
const CALLEE_VIDEO_PORT: u16 = 43602;
const CALLER_AUDIO_PORT: u16 = 43610;
const CALLER_VIDEO_PORT: u16 = 43612;

const LOCAL: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// 测试片源 (workspace 根的 assets/oceans.mp4, H264 + AAC)
fn oceans() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/oceans.mp4")
}

/// 可收视频谓词: H264/H265 (同 call-ui media::video_pred)
fn video_pred(name: &str) -> Option<VideoCodec> {
    match VideoCodec::from_name(name) {
        c @ (VideoCodec::H264 | VideoCodec::H265) => Some(c),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn self_call_send_recv_file() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    // --- 信令: 绑 127.0.0.1 随机端口, 不注册, IP 直拨自己 ---
    let cfg = SipClientConfig::new(
        "9999".to_string(),
        "9999".to_string(),
        "test".to_string(),
        // server_addr 仅用于拼 sip_uri, 本测试不注册, 填占位即可
        SocketAddr::V4(SocketAddrV4::new(LOCAL, 5060)),
        SocketAddr::V4(SocketAddrV4::new(LOCAL, 0)),
        None,
    );
    let client = Arc::new(SipClient::new(cfg, CancellationToken::new()).await?);
    let _endpoint = client.spawn_endpoint();
    let sip_port = client
        .contact
        .host_with_port
        .port
        .map(|p| p.0)
        .unwrap_or(5060);

    // --- 被叫: 来电即自动接听, answer 声明收流端口 ---
    let (state_sender, state_receiver) = client.dialog_layer.new_dialog_state_channel();
    let on_incoming = move |dialog: InviteDialog| {
        tokio::spawn(async move {
            let offer = parse_offer_all(dialog.initial_request().body())?;
            let (_audio, _video, answer) = negotiate::build_answer(
                &offer,
                IpAddr::V4(LOCAL),
                CALLEE_AUDIO_PORT,
                CALLEE_VIDEO_PORT,
                sendable_audio_codec,
                video_pred,
            );
            let headers = vec![Header::ContentType("application/sdp".into())];
            dialog.accept(Some(headers), Some(answer.into_bytes()))?;
            dialog.cancel_token().cancelled().await; // 挂着等对端 BYE
            Ok::<_, anyhow::Error>(())
        });
    };
    // 本测试不触发 re-INVITE, on_update 一律回 None (200 不带 SDP)
    let on_update = |_id: rsipstack::dialog::DialogId, _req: rsip::Request| async move { None };
    let state_loop = tokio::spawn(run_dialog_state_loop(
        client.dialog_layer.clone(),
        state_receiver,
        on_incoming,
        on_update,
    ));
    let c = client.clone();
    let ss = state_sender.clone();
    let incoming_loop = tokio::spawn(async move { c.run_incoming_loop(&ss).await });

    // --- 被叫媒体: 收流计数 (端口/编码与上面 answer 声明的一致) ---
    let frames = Arc::new(AtomicU64::new(0));
    let tap: VideoFrameSink = {
        let frames = frames.clone();
        Arc::new(parking_lot::Mutex::new(
            move |_f: ipcam_gst::RawVideoFrame<'_>| {
                frames.fetch_add(1, Ordering::Relaxed);
            },
        ))
    };
    let record_path = std::env::temp_dir().join("ipcam_sip_self_call_test.mp4");
    let _ = std::fs::remove_file(&record_path);
    // path=Some 让音频有去处 (解码转 opus 进 mp4), 不碰本机声卡
    let receiver = start_rtp_receiver(RtpRecvConfig {
        path: Some(record_path.clone()),
        playback: false,
        video_tap: Some(tap),
        video: Some((CALLEE_VIDEO_PORT, VideoCodec::H264)),
        audio: Some((CALLEE_AUDIO_PORT, AudioCodec::G711U)),
    })?;

    // --- 主叫: offer (PCMU/PCMA + H264) 直拨自己 ---
    let offer = build_av_offer(
        IpAddr::V4(LOCAL),
        CALLER_AUDIO_PORT,
        CALLER_VIDEO_PORT,
        &[PT_PCMU, PT_PCMA],
    );
    let callee = rsip::Uri::try_from(format!("sip:9999@127.0.0.1:{sip_port}").as_str())?;
    let invite = InviteOption {
        callee,
        caller: client.contact.clone(),
        contact: client.contact.clone(),
        credential: None,
        offer: Some(offer.to_string().into_bytes()),
        ..Default::default()
    };
    let (dialog, resp) = client.dialog_layer.do_invite(invite, state_sender).await?;
    let resp = resp.ok_or_else(|| anyhow::anyhow!("INVITE has no final response"))?;
    assert_eq!(resp.status_code, rsip::StatusCode::OK, "INVITE rejected");
    let peers = parse_answer_all(resp.body())?;
    let peer_audio = peers.audio.as_ref().expect("answer must have audio");
    let peer_video = peers.video.as_ref().expect("answer must have video");

    // --- 主叫媒体: 把片源音视频发回被叫 ---
    let sender = start_rtp_sender(RtpSendConfig {
        audio: Some((
            TrackSource::File(oceans()),
            AudioDest {
                addr: peer_audio.addr,
                payload_type: peer_audio.payload_type,
                codec: sendable_audio_codec(&peer_audio.codec).expect("answer codec must be G.711"),
            },
        )),
        video: Some((
            TrackSource::File(oceans()),
            RtpDest {
                addr: peer_video.addr,
                payload_type: peer_video.payload_type,
                max_width: None,
            },
        )),
    })?;

    // --- 断言: 收端包数/解码帧数持续增长 (任一环节断链都会卡住超时) ---
    let deadline = Instant::now() + Duration::from_secs(25);
    let (mut video_pkts, mut audio_pkts) = (0, 0);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        (video_pkts, audio_pkts) = receiver.packet_counts();
        if video_pkts > 60 && audio_pkts > 100 && frames.load(Ordering::Relaxed) > 5 {
            break;
        }
    }
    let decoded_frames = frames.load(Ordering::Relaxed);

    // --- 收尾: BYE -> 停管线 (mp4 moov 靠 stop() 写齐) -> 停 client ---
    dialog.bye_with_headers(None).await?;
    sender.stop();
    receiver.stop();
    client.stop();
    state_loop.abort();
    incoming_loop.abort();

    assert!(video_pkts > 60, "video packets too few: {video_pkts}");
    assert!(audio_pkts > 100, "audio packets too few: {audio_pkts}");
    assert!(
        decoded_frames > 5,
        "decoded frames too few: {decoded_frames}"
    );
    let mp4_size = std::fs::metadata(&record_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let _ = std::fs::remove_file(&record_path);
    assert!(mp4_size > 10_000, "recorded mp4 too small: {mp4_size}");
    Ok(())
}
