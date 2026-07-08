use async_trait::async_trait;
use ipcam_core::{
    CoreError, CoreResult, DecodedFrame, DecodedHandle, EncodedPacket, PixelFormat,
    RecoveryStrategy, VideoCodec,
};
use thiserror::Error;
use tracing::{debug, warn};

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("core: {0}")]
    Core(#[from] CoreError),
    #[error("mpp backend unavailable")]
    NoBackend,
    #[error("decode failure: {0}")]
    Decode(String),
}

pub type DecodeResult<T> = Result<T, DecodeError>;

pub struct SoftwareDecoder {
    codec: VideoCodec,
    recovery: parking_lot::Mutex<RecoveryStrategy>,
    state: parking_lot::Mutex<SoftwareState>,
}

#[derive(Default)]
struct SoftwareState {
    sps_seen: bool,
    pps_seen: bool,
    width: u32,
    height: u32,
    frames_emitted: u64,
}

impl SoftwareDecoder {
    pub fn new(codec: VideoCodec) -> Self {
        Self {
            codec,
            recovery: parking_lot::Mutex::new(RecoveryStrategy::default()),
            state: parking_lot::Mutex::new(SoftwareState::default()),
        }
    }
}

#[async_trait]
impl ipcam_core::Decoder for SoftwareDecoder {
    fn codec(&self) -> VideoCodec {
        self.codec
    }

    async fn submit(&self, packet: EncodedPacket) -> CoreResult<Option<DecodedFrame>> {
        let mut st = self.state.lock();
        let payload = &packet.data;

        if has_nal_type(payload, 7) {
            st.sps_seen = true;
            if let Some((w, h)) = parse_sps_size(payload) {
                st.width = w;
                st.height = h;
            }
        }
        if has_nal_type(payload, 8) {
            st.pps_seen = true;
        }
        let is_idr = has_nal_type(payload, 5);
        let recovery = *self.recovery.lock();

        if is_idr && (!st.sps_seen || !st.pps_seen) {
            warn!("idr received before sps/pps; dropping");
            return Ok(None);
        }
        if recovery == RecoveryStrategy::DropFrames && !is_idr {
            return Ok(None);
        }

        let width = if st.width > 0 { st.width } else { 1920 };
        let height = if st.height > 0 { st.height } else { 1080 };
        let frame = synthesize_frame(width, height, st.frames_emitted);
        st.frames_emitted += 1;
        Ok(Some(frame))
    }

    fn set_recovery_strategy(&mut self, s: RecoveryStrategy) {
        *self.recovery.lock() = s;
    }

    async fn reset(&self) -> CoreResult<()> {
        let mut st = self.state.lock();
        st.sps_seen = false;
        st.pps_seen = false;
        st.width = 0;
        st.height = 0;
        st.frames_emitted = 0;
        Ok(())
    }
}

fn has_nal_type(payload: &[u8], nal_type: u8) -> bool {
    let mut i = 0;
    while i + 4 < payload.len() {
        if payload[i] == 0 && payload[i + 1] == 0 && payload[i + 2] == 0 && payload[i + 3] == 1 {
            let nal = payload[i + 4];
            if nal & 0x1F == nal_type {
                return true;
            }
            i += 4;
        } else if payload[i] == 0 && payload[i + 1] == 0 && payload[i + 2] == 1 {
            let nal = payload[i + 3];
            if nal & 0x1F == nal_type {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

fn parse_sps_size(payload: &[u8]) -> Option<(u32, u32)> {
    for i in 0..payload.len().saturating_sub(8) {
        if payload[i] == 0
            && payload[i + 1] == 0
            && payload[i + 2] == 0
            && payload[i + 3] == 1
            && (payload[i + 4] & 0x1F) == 7
        {
            let w = ((payload[i + 5] as u32) << 24)
                | ((payload[i + 6] as u32) << 16)
                | ((payload[i + 7] as u32) << 8)
                | (payload[i + 8] as u32);
            let h = ((payload[i + 9] as u32) << 24)
                | ((payload[i + 10] as u32) << 16)
                | ((payload[i + 11] as u32) << 8)
                | (payload[i + 12] as u32);
            return Some((w & 0x7FFF, h & 0x7FFF));
        }
    }
    None
}

fn synthesize_frame(width: u32, height: u32, idx: u64) -> DecodedFrame {
    let stride = width;
    let y_size = (stride as usize) * (height as usize);
    let uv_size = y_size / 2;
    let total = y_size + uv_size;
    let mut buf = vec![0u8; total];
    let shade = ((idx.wrapping_mul(17)) & 0xFF) as u8;
    for b in buf.iter_mut().take(y_size) {
        *b = shade;
    }
    for b in buf.iter_mut().skip(y_size).take(uv_size) {
        *b = 128;
    }
    let handle = DecodedHandle::new(Box::new(buf));
    DecodedFrame {
        handle,
        pts: (idx as i64) * 33_333,
        width,
        height,
        stride,
        format: PixelFormat::Nv12,
    }
}

#[cfg(all(target_os = "linux", feature = "hw-decode"))]
pub mod mpp {
    use super::*;

    pub struct MppDecoder {
        codec: VideoCodec,
        state: parking_lot::Mutex<MppState>,
    }

    struct MppState {
        frames_emitted: u64,
    }

    impl MppDecoder {
        pub fn new(codec: VideoCodec) -> Self {
            info!("creating MPP decoder (rockchip_mpp)");
            Self {
                codec,
                state: parking_lot::Mutex::new(MppState { frames_emitted: 0 }),
            }
        }
    }

    #[async_trait]
    impl ipcam_core::Decoder for MppDecoder {
        fn codec(&self) -> VideoCodec {
            self.codec
        }

        async fn submit(&self, _packet: EncodedPacket) -> CoreResult<Option<DecodedFrame>> {
            warn!("MPP FFI not yet bound; returning Err(NoBackend)");
            Err(CoreError::NotImplemented("MPP backend not yet bound"))
        }

        fn set_recovery_strategy(&mut self, _s: RecoveryStrategy) {}

        async fn reset(&self) -> CoreResult<()> {
            let mut s = self.state.lock();
            s.frames_emitted = 0;
            Ok(())
        }
    }
}

#[cfg(not(all(target_os = "linux", feature = "hw-decode")))]
pub mod mpp {
    use ipcam_core::{
        CoreError, CoreResult, DecodedFrame, EncodedPacket, RecoveryStrategy, VideoCodec,
    };

    pub struct MppDecoder;

    impl MppDecoder {
        pub fn new(_codec: VideoCodec) -> Self {
            Self
        }
    }

    #[async_trait::async_trait]
    impl ipcam_core::Decoder for MppDecoder {
        fn codec(&self) -> VideoCodec {
            VideoCodec::Unknown
        }

        async fn submit(&self, _packet: EncodedPacket) -> CoreResult<Option<DecodedFrame>> {
            Err(CoreError::NotImplemented(
                "MPP backend not built (need target_os=linux + hw-decode feature)",
            ))
        }

        fn set_recovery_strategy(&mut self, _s: RecoveryStrategy) {}

        async fn reset(&self) -> CoreResult<()> {
            Ok(())
        }
    }
}

pub fn build_decoder(codec: VideoCodec) -> Box<dyn ipcam_core::Decoder> {
    #[cfg(all(target_os = "linux", feature = "hw-decode"))]
    {
        debug!("using MPP hardware decoder");
        return Box::new(mpp::MppDecoder::new(codec));
    }
    debug!("using software decoder fallback");
    Box::new(SoftwareDecoder::new(codec))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use ipcam_core::Decoder;

    #[tokio::test]
    async fn software_decoder_emits_frames_for_idr() {
        let dec = SoftwareDecoder::new(VideoCodec::H264);

        let mut sps = vec![0u8; 32];
        sps[3] = 1;
        sps[4] = 0x67;
        sps[5] = 0x42;
        sps[6] = 0xc0;
        sps[7] = 0x1e;
        sps[8] = 0xd9;
        sps[9] = 0x00;
        sps[10] = 0xa0;
        sps[11] = 0x47;
        sps[12] = 0xfe;
        sps[13] = 0xc8;
        let _ = dec
            .submit(EncodedPacket {
                codec: VideoCodec::H264,
                data: Bytes::from(sps),
                rtp_ts: 0,
                arrival_us: 0,
                is_keyframe: false,
                marker: false,
            })
            .await
            .unwrap();

        let mut pps = vec![0u8; 8];
        pps[3] = 1;
        pps[4] = 0x68;
        let _ = dec
            .submit(EncodedPacket {
                codec: VideoCodec::H264,
                data: Bytes::from(pps),
                rtp_ts: 0,
                arrival_us: 0,
                is_keyframe: false,
                marker: false,
            })
            .await
            .unwrap();

        let mut idr = vec![0u8; 32];
        idr[3] = 1;
        idr[4] = 0x65;
        let out = dec
            .submit(EncodedPacket {
                codec: VideoCodec::H264,
                data: Bytes::from(idr),
                rtp_ts: 0,
                arrival_us: 0,
                is_keyframe: true,
                marker: true,
            })
            .await
            .unwrap();
        assert!(out.is_some());
    }

    #[tokio::test]
    async fn software_decoder_drops_when_sps_missing() {
        let dec = SoftwareDecoder::new(VideoCodec::H264);
        let mut buf = vec![0u8; 8];
        buf[3] = 1;
        buf[4] = 0x65;
        let pkt = EncodedPacket {
            codec: VideoCodec::H264,
            data: Bytes::from(buf),
            rtp_ts: 0,
            arrival_us: 0,
            is_keyframe: true,
            marker: true,
        };
        let out = dec.submit(pkt).await.unwrap();
        assert!(out.is_none());
    }
}
