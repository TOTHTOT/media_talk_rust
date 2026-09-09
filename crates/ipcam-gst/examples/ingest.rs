//! Minimal ingest bisect: pull N seconds of video from a camera and count
//! frames — no tokio, no web server, just `ipcam_gst::start`.
//! Usage: cargo run -p ipcam-gst --example ingest -- <rtsp_url> [secs]

use std::time::{Duration, Instant};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "rtsp://admin:changeme@192.168.1.168:8554/ch01".into());
    let secs: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);

    // webrtcsink needs the signalling server to register with; host it
    // in-process so the example is self-contained.
    ipcam_gst::ensure_signalling_server().expect("signalling server");

    let cfg = ipcam_gst::GstStreamConfig {
        uri: url,
        stream_name: "ingest-example".into(),
        ..Default::default()
    };
    let handle = ipcam_gst::start(cfg).expect("start");

    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(1));
        let s = handle.stats();
        println!(
            "t={}s state={:?} frames={} bytes={}",
            t0.elapsed().as_secs(),
            handle.state(),
            s.frames_video,
            s.bytes
        );
    }
    handle.stop();
    println!("DONE frames={}", handle.stats().frames_video);
}
