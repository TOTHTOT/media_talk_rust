//! Raw-tap bisect: pull N seconds from a camera with the appsink taps
//! enabled, count decoded audio/video callbacks, and dump one video
//! frame as a PPM image for visual verification.
//! Usage: cargo run -p ipcam-gst --example rawtap -- <rtsp_url> [secs] [out.ppm]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ipcam_gst::{RawAudioChunk, RawTaps, RawVideoFrame};
use parking_lot::Mutex;

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
        .unwrap_or(8);
    let ppm_path = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "target/tmp/rawtap.ppm".into());

    ipcam_gst::ensure_signalling_server().expect("signalling server");

    let video_frames = Arc::new(AtomicU64::new(0));
    let audio_chunks = Arc::new(AtomicU64::new(0));
    let audio_bytes = Arc::new(AtomicU64::new(0));

    // 视频 tap：数帧，第 30 帧落地成 PPM（RGBA → 去 alpha 存 RGB）
    let vpath = ppm_path.clone();
    let vcount = video_frames.clone();
    let video_sink = Arc::new(Mutex::new(move |f: RawVideoFrame<'_>| {
        let n = vcount.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 30 {
            let row = f.width as usize * 4;
            let mut rgb = Vec::with_capacity(f.width as usize * f.height as usize * 3);
            for y in 0..f.height as usize {
                let line = &f.data[y * f.stride..y * f.stride + row];
                for px in line.chunks_exact(4) {
                    rgb.extend_from_slice(&px[..3]);
                }
            }
            let header = format!("P6\n{} {}\n255\n", f.width, f.height);
            let mut out = header.into_bytes();
            out.extend_from_slice(&rgb);
            if let Err(e) = std::fs::write(&vpath, &out) {
                eprintln!("failed to write {vpath}: {e}");
            } else {
                println!("saved frame 30 to {vpath} ({}x{})", f.width, f.height);
            }
        }
    }));

    // 音频 tap：数 chunk / 字节，打印一次格式
    let acount = audio_chunks.clone();
    let abytes = audio_bytes.clone();
    let audio_sink = Arc::new(Mutex::new(move |c: RawAudioChunk<'_>| {
        abytes.fetch_add(c.data.len() as u64, Ordering::Relaxed);
        let n = acount.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 {
            println!(
                "first audio chunk: rate={} channels={} bytes={}",
                c.rate,
                c.channels,
                c.data.len()
            );
        }
    }));

    let taps = RawTaps {
        video: Some(video_sink),
        audio: Some(audio_sink),
    };
    let cfg = ipcam_gst::GstStreamConfig {
        uri: url,
        stream_name: "rawtap-example".into(),
        ..Default::default()
    };
    let handle = ipcam_gst::start_with_taps(cfg, taps).expect("start");

    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(1));
        let s = handle.stats();
        println!(
            "t={}s state={:?} encoded_frames={} decoded_frames={} audio_chunks={} audio_bytes={}",
            t0.elapsed().as_secs(),
            handle.state(),
            s.frames_video,
            video_frames.load(Ordering::Relaxed),
            audio_chunks.load(Ordering::Relaxed),
            audio_bytes.load(Ordering::Relaxed),
        );
    }
    handle.stop();
    println!(
        "DONE encoded={} decoded={} audio_chunks={}",
        handle.stats().frames_video,
        video_frames.load(Ordering::Relaxed),
        audio_chunks.load(Ordering::Relaxed),
    );
}
