//! End-to-end test for the RTSP client.
//!
//! Spins up a fake RTSP server on a random localhost port. The fake server:
//!   - Replies to OPTIONS, DESCRIBE, SETUP, PLAY with proper RTSP responses
//!   - Streams interleaved RTP packets carrying a single H.264 IDR access
//!     unit (Annex-B framed)
//!   - Verifies the RtspClient calls `on_video` once per NAL, with
//!     `is_keyframe=true` for the IDR slice, and that the marker bit
//!     is propagated to `EncodedPacket.marker`.
//!
//! This is the "happy path" integration test that backs the v1 streaming
//! claim: RTSP → RTP → depacketizer → EncodedPacket.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use ipcam_core::{EncodedPacket, VideoCodec};
use ipcam_rtsp::{RtspClient, RtspConfig};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

static TRACING: std::sync::OnceLock<()> = std::sync::OnceLock::new();

fn init_tracing() {
    TRACING.get_or_init(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
    });
}

const SDP_H264: &str = "v=0\r\n\
o=- 0 0 IN IP4 127.0.0.1\r\n\
s=Fake\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=control:track1\r\n\
a=fmtp:96 profile-level-id=42C01E;sprop-parameter-sets=Z0LAHtkA,aM4G4g==\r\n";

/// Read one full RTSP request (header + optional body) up to \r\n\r\n.
async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 256];
    loop {
        let n = stream.read(&mut tmp).await.expect("server read");
        if n == 0 {
            return buf;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return buf;
        }
    }
}

fn make_rtsp_response(cseq: u64, status: u16, reason: &str, extra: &[(&str, &str)]) -> String {
    let mut s = format!("RTSP/1.0 {status} {reason}\r\nCSeq: {cseq}\r\n");
    for (k, v) in extra {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str("\r\n");
    s
}

fn make_rtp_h264_single_nal(nalu: &[u8], seq: u16, ts: u32, ssrc: u32, marker: bool) -> Vec<u8> {
    // RTP header (12 bytes): byte0=V/P/X/CC, byte1=M/PT, SEQ, TS, SSRC.
    let mut pkt = Vec::with_capacity(12 + nalu.len());
    pkt.push(0x80); // V=2, P=0, X=0, CC=0
    pkt.push(if marker { 0x80 | 96 } else { 96 }); // M=marker, PT=96
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&ts.to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    pkt.extend_from_slice(nalu);
    pkt
}

fn wrap_interleaved(channel: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.push(b'$');
    out.push(channel);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Build a minimal H.264 access unit (SPS + PPS + IDR) Annex-B framed.
fn make_idr_access_unit() -> Vec<Bytes> {
    let sps = vec![
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0, 0x47, 0xFE, 0xC8,
    ];
    let pps = vec![0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x38, 0x80];
    let idr = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC];
    vec![Bytes::from(sps), Bytes::from(pps), Bytes::from(idr)]
}

async fn run_fake_server(
    listener: TcpListener,
    captured_packets: Arc<Mutex<Vec<EncodedPacket>>>,
) -> SocketAddr {
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        // OPTIONS
        let _ = read_request(&mut stream).await;
        stream
            .write_all(make_rtsp_response(1, 200, "OK", &[]).as_bytes())
            .await
            .unwrap();

        // DESCRIBE
        let _ = read_request(&mut stream).await;
        stream
            .write_all(
                make_rtsp_response(
                    2,
                    200,
                    "OK",
                    &[
                        ("Content-Type", "application/sdp"),
                        ("Content-Length", &SDP_H264.len().to_string()),
                    ],
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.write_all(SDP_H264.as_bytes()).await.unwrap();

        // SETUP
        let _ = read_request(&mut stream).await;
        stream
            .write_all(
                make_rtsp_response(
                    3,
                    200,
                    "OK",
                    &[
                        ("Transport", "RTP/AVP/TCP;interleaved=0-1"),
                        ("Session", "FAKESESSION123"),
                    ],
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        // PLAY
        let _ = read_request(&mut stream).await;
        stream
            .write_all(
                make_rtsp_response(4, 200, "OK", &[("Session", "FAKESESSION123")]).as_bytes(),
            )
            .await
            .unwrap();

        // Now stream one H.264 IDR access unit as 3 RTP packets (one per NAL)
        // with marker=1 on the last (IDR slice).
        let nalus = make_idr_access_unit();
        let ssrc: u32 = 0xCAFEBABE;
        for (i, nalu) in nalus.iter().enumerate() {
            // NAL types 7=SPS, 8=PPS, 5=IDR → strip the Annex-B start code.
            let body = if nalu.starts_with(&[0, 0, 0, 1]) {
                &nalu[4..]
            } else {
                nalu.as_ref()
            };
            let is_last = i == nalus.len() - 1;
            let rtp =
                make_rtp_h264_single_nal(body, (i + 1) as u16, 90000 + i as u32, ssrc, is_last);
            let frame = wrap_interleaved(0, &rtp);
            stream.write_all(&frame).await.unwrap();
        }

        // Capture some packets by pumping for ~300ms so the client has
        // time to parse. The client reads in play_loop; the test will
        // capture packets via the on_video callback.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = captured_packets; // keep alive
    });
    addr
}

#[tokio::test]
async fn rtsp_client_handles_options_describe_setup_play() {
    init_tracing();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let captured: Arc<Mutex<Vec<EncodedPacket>>> = Arc::new(Mutex::new(Vec::new()));
    let addr = run_fake_server(listener, captured.clone()).await;
    let uri = format!("rtsp://{}/Streaming/Tracks/101", addr);

    let cfg = RtspConfig::new(uri);
    let client = RtspClient::new(cfg);
    let info = client.connect().await.expect("connect ok");
    assert_eq!(info.video_codec, VideoCodec::H264);

    let cap = captured.clone();
    let on_video = move |pkt: EncodedPacket| -> ipcam_core::CoreResult<()> {
        cap.lock().push(pkt);
        Ok(())
    };
    let on_audio = |_pkt: EncodedPacket| -> ipcam_core::CoreResult<()> { Ok(()) };

    // play_loop returns Err on timeout/EOF after the fake server closes.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        client.play_loop(on_video, on_audio),
    )
    .await;

    let pkts = captured.lock().clone();
    assert!(
        pkts.len() >= 3,
        "expected at least 3 NAL packets (SPS+PPS+IDR), got {}",
        pkts.len()
    );

    // Verify NAL types by inspecting the byte after the Annex-B start code.
    let mut types: Vec<u8> = Vec::new();
    for p in &pkts {
        if p.data.len() < 5 {
            continue;
        }
        // H264Depacketizer emits NAL units WITH the 00 00 00 01 Annex-B
        // start code prefix; the NAL type byte sits at index 4.
        types.push(p.data[4] & 0x1F);
    }
    assert!(
        types.contains(&7),
        "expected SPS NAL type 7, got {:?}",
        types
    );
    assert!(
        types.contains(&8),
        "expected PPS NAL type 8, got {:?}",
        types
    );
    assert!(
        types.contains(&5),
        "expected IDR NAL type 5, got {:?}",
        types
    );

    // The IDR packet must be flagged keyframe and marker=true.
    let idr = pkts.iter().find(|p| p.data[4] & 0x1F == 5).unwrap();
    assert!(idr.is_keyframe, "IDR must set is_keyframe=true");
    assert!(
        idr.marker,
        "IDR (last NAL of access unit) must have marker=true"
    );

    // SPS / PPS are not keyframes, marker may be 0 (we sent them as non-marker).
    let sps = pkts.iter().find(|p| p.data[4] & 0x1F == 7).unwrap();
    assert!(!sps.is_keyframe, "SPS must not set is_keyframe");
    assert!(!sps.marker, "non-last NALs must not set marker");
}

#[tokio::test]
async fn rtsp_client_connect_returns_session_info_with_tracks() {
    init_tracing();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let captured: Arc<Mutex<Vec<EncodedPacket>>> = Arc::new(Mutex::new(Vec::new()));
    let addr = run_fake_server(listener, captured.clone()).await;
    let uri = format!("rtsp://{}/test", addr);

    let client = RtspClient::new(RtspConfig::new(uri));
    let info = client.connect().await.expect("connect ok");
    assert_eq!(info.tracks.len(), 1);
    assert_eq!(info.tracks[0].codec, VideoCodec::H264);
    assert_eq!(info.tracks[0].clock_rate, 90000);
    assert_eq!(info.session_id.as_deref(), Some("FAKESESSION123"));
}

#[test]
fn rtp_single_nal_packet_construction_is_well_formed() {
    // Sanity check: an RTP packet for a 5-byte NAL must be 17 bytes:
    // 12-byte header + 5-byte NAL.
    let pkt = make_rtp_h264_single_nal(&[0x65, 1, 2, 3, 4], 1, 0, 0xCAFE, true);
    assert_eq!(pkt.len(), 17);
    assert_eq!(pkt[0], 0x80); // V=2, P=0, X=0, CC=0
    assert_eq!(pkt[1], 0x80 | 96); // M=1, PT=96
    let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
    assert_eq!(seq, 1);
    let frame = wrap_interleaved(0, &pkt);
    assert_eq!(frame.len(), 21);
    assert_eq!(frame[0], b'$');
    assert_eq!(frame[1], 0);
    let len = u16::from_be_bytes([frame[2], frame[3]]);
    assert_eq!(len, 17);
}

/// Run a fake server that demands Digest auth on the first OPTIONS,
/// accepts the retry, then answers DESCRIBE/SETUP/PLAY in sequence.
async fn run_fake_digest_server(
    listener: TcpListener,
    www_authenticate: &'static str,
) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let addr = listener.local_addr().unwrap();
    let captured_clone = captured.clone();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        // Helper: read a request and pull out the CSeq the client actually sent
        // (rtsp-runtime tracks its own CSeq counter internally — the test must
        // echo whatever the client wrote, not hardcoded values).
        let cseq_of = |req_bytes: &[u8]| -> u64 {
            let s = String::from_utf8_lossy(req_bytes);
            s.split("\r\n")
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.eq_ignore_ascii_case("CSeq") {
                        v.trim().parse().ok()
                    } else {
                        None
                    }
                })
                .expect("CSeq header")
        };

        // First OPTIONS (no Authorization) → 401.
        let req = read_request(&mut stream).await;
        let req_str = String::from_utf8_lossy(&req);
        let first_line = req_str.lines().next().unwrap_or("");
        assert!(
            first_line.starts_with("OPTIONS "),
            "expected first OPTIONS, got: {first_line:?}"
        );
        assert!(
            !req_str.to_ascii_lowercase().contains("authorization:"),
            "first OPTIONS must not have Authorization, got: {req_str}"
        );
        let cseq1 = cseq_of(&req);
        stream
            .write_all(
                make_rtsp_response(
                    cseq1,
                    401,
                    "Unauthorized",
                    &[("WWW-Authenticate", www_authenticate)],
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        // Second OPTIONS (retry with Authorization) → 200.
        let req = read_request(&mut stream).await;
        let req_str = String::from_utf8_lossy(&req);
        let first_line = req_str.lines().next().unwrap_or("");
        assert!(
            first_line.starts_with("OPTIONS "),
            "expected OPTIONS retry, got: {first_line:?}"
        );
        assert!(
            req_str.to_ascii_lowercase().contains("authorization:"),
            "OPTIONS retry must carry Authorization, got: {req_str}"
        );
        if let Some(line) = req_str
            .split("\r\n")
            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
        {
            captured_clone.lock().push(line.to_string());
        }
        let cseq2 = cseq_of(&req);
        stream
            .write_all(make_rtsp_response(cseq2, 200, "OK", &[]).as_bytes())
            .await
            .unwrap();

        // DESCRIBE → 200 + SDP body.
        let req = read_request(&mut stream).await;
        let first_line = String::from_utf8_lossy(&req)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        assert!(
            first_line.starts_with("DESCRIBE "),
            "expected DESCRIBE, got: {first_line:?}"
        );
        let cseq = cseq_of(&req);
        stream
            .write_all(
                make_rtsp_response(
                    cseq,
                    200,
                    "OK",
                    &[
                        ("Content-Type", "application/sdp"),
                        ("Content-Length", &SDP_H264.len().to_string()),
                    ],
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.write_all(SDP_H264.as_bytes()).await.unwrap();

        // SETUP → 200 with transport.
        let req = read_request(&mut stream).await;
        let first_line = String::from_utf8_lossy(&req)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        assert!(
            first_line.starts_with("SETUP "),
            "expected SETUP, got: {first_line:?}"
        );
        let cseq = cseq_of(&req);
        stream
            .write_all(
                make_rtsp_response(
                    cseq,
                    200,
                    "OK",
                    &[
                        ("Transport", "RTP/AVP/TCP;interleaved=0-1"),
                        ("Session", "FAKESESSION123"),
                    ],
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        // PLAY → 200 with session.
        let req = read_request(&mut stream).await;
        let first_line = String::from_utf8_lossy(&req)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        assert!(
            first_line.starts_with("PLAY "),
            "expected PLAY, got: {first_line:?}"
        );
        let cseq = cseq_of(&req);
        stream
            .write_all(
                make_rtsp_response(cseq, 200, "OK", &[("Session", "FAKESESSION123")]).as_bytes(),
            )
            .await
            .unwrap();
    });
    (addr, captured)
}

#[tokio::test]
async fn rtsp_client_retries_with_digest_after_401() {
    init_tracing();
    let www_auth = r#"Digest realm="mediatalk-test", nonce="abc123nonce", qop="auth""#;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (addr, captured) = run_fake_digest_server(listener, www_auth).await;
    let uri = format!("rtsp://{}/Streaming/Tracks/101", addr);

    let cfg = RtspConfig::new(uri).with_credentials("admin", "hunter2");
    let client = RtspClient::new(cfg);
    let info = client
        .connect()
        .await
        .expect("connect should succeed via digest");

    assert_eq!(info.video_codec, VideoCodec::H264);
    let caps = captured.lock().clone();
    assert_eq!(
        caps.len(),
        1,
        "expected one Authorization retry, captured: {caps:?}"
    );
    let line = &caps[0];
    assert!(line.contains("Digest"), "auth header: {line}");
    assert!(line.contains(r#"username="admin""#), "auth header: {line}");
    assert!(
        line.contains(r#"realm="mediatalk-test""#),
        "auth header: {line}"
    );
    assert!(
        line.contains(r#"nonce="abc123nonce""#),
        "auth header: {line}"
    );
    assert!(line.contains("qop=auth"), "auth header: {line}");
    assert!(line.contains("nc=00000001"), "auth header: {line}");
    assert!(line.contains("cnonce=\""), "auth header: {line}");
    let resp_val = line
        .split("response=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("response field");
    assert_eq!(
        resp_val.len(),
        32,
        "response must be 32 hex chars: {resp_val}"
    );
    assert!(
        resp_val.chars().all(|c| c.is_ascii_hexdigit()),
        "response must be hex: {resp_val}"
    );
}
