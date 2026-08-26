//! Bisect 3: call web-display's `spawn_streaming` directly (the exact serve
//! data path: resolve URI → ipcam_gst::start → ingest_packet → Fmp4Muxer)
//! without the HTTP server. Counts muxer segments over N seconds.
//! Usage: cargo run -p web-display --example stream_only -- <rtsp_url> [secs]

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
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

    let mux = Arc::new(Mutex::new(web_display::mux::Fmp4Muxer::new()));
    let (state_tx, _rx) = tokio::sync::broadcast::channel(16);

    web_display::stream::spawn_streaming(
        uuid::Uuid::new_v4(),
        None,             // xaddr
        "p1".to_string(), // profile token (unused for manual URI)
        Some(url),        // profile_uri → manual path
        0,
        0,
        None, // credentials
        None, // audio_out
        mux.clone(),
        state_tx,
    );

    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let m = mux.lock();
        println!(
            "t={}s ready={} segments={}",
            t0.elapsed().as_secs(),
            m.is_ready(),
            m.segment_count()
        );
    }
    println!("DONE segments={}", mux.lock().segment_count());
}
