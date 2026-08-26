//! Minimal ingest bisect: pull N seconds of video from a camera and count
//! frames — no tokio, no web server, just `ipcam_gst::start`.
//! Usage: cargo run -p ipcam-gst --example ingest -- <rtsp_url> [secs]

use std::sync::atomic::{AtomicU64, Ordering};
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

    let frames = std::sync::Arc::new(AtomicU64::new(0));
    let bytes = std::sync::Arc::new(AtomicU64::new(0));
    let f = frames.clone();
    let b = bytes.clone();

    let cfg = ipcam_gst::GstStreamConfig {
        uri: url,
        ..Default::default()
    };
    let handle = ipcam_gst::start(
        cfg,
        move |pkt| {
            f.fetch_add(1, Ordering::Relaxed);
            b.fetch_add(pkt.data.len() as u64, Ordering::Relaxed);
        },
        |_audio| {},
    )
    .expect("start");

    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(1));
        println!(
            "t={}s state={:?} frames={} bytes={}",
            t0.elapsed().as_secs(),
            handle.state(),
            frames.load(Ordering::Relaxed),
            bytes.load(Ordering::Relaxed)
        );
    }
    handle.stop();
    println!("DONE frames={}", frames.load(Ordering::Relaxed));
}
