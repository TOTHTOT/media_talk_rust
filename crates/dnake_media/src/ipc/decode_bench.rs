use anyhow::{Context, anyhow};
use hardware_decode::SoftwareDecoder;
use ipcam_core::{Decoder, EncodedPacket, VideoCodec};
use std::path::Path;
use std::time::Instant;

pub async fn run(path: &str, max_frames: u64) -> anyhow::Result<()> {
    let p = Path::new(path);
    if !p.exists() {
        return Err(anyhow!("file not found: {}", path));
    }
    let bytes = std::fs::read(p).with_context(|| format!("read {}", path))?;
    let units = annex_b::split_access_units(&bytes);
    if units.is_empty() {
        return Err(anyhow!("no access units found in {}", path));
    }
    let decoder = SoftwareDecoder::new(VideoCodec::H264);
    let mut frames = 0u64;
    let start = Instant::now();
    for (i, au) in units.iter().enumerate() {
        let packet = EncodedPacket {
            codec: VideoCodec::H264,
            data: au.clone().into(),
            rtp_ts: i as u32,
            arrival_us: ipcam_core::now_micros(),
            is_keyframe: true,
            marker: true,
        };
        if decoder.submit(packet).await.context("submit")?.is_some() {
            frames += 1;
            if max_frames > 0 && frames >= max_frames {
                break;
            }
        }
    }
    let elapsed = start.elapsed();
    let fps = frames as f64 / elapsed.as_secs_f64().max(0.001);
    println!(
        "decoded {} frames in {:?} ({:.1} fps avg)",
        frames, elapsed, fps
    );
    Ok(())
}

mod annex_b {
    use bytes::Bytes;

    pub fn split_access_units(data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        let mut i = 0;
        let n = data.len();
        while i + 2 < n {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                cur.extend_from_slice(&[0, 0, 0, 1]);
                i += 3;
            } else if data[i] == 0
                && data[i + 1] == 0
                && data[i + 2] == 0
                && i + 3 < n
                && data[i + 3] == 1
            {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                cur.extend_from_slice(&[0, 0, 0, 1]);
                i += 4;
            } else {
                cur.push(data[i]);
                i += 1;
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out.into_iter().filter(|v| !v.is_empty()).collect()
    }

    #[allow(dead_code)]
    pub fn _bytes_marker() -> Bytes {
        Bytes::from_static(&[0, 0, 0, 1])
    }
}
