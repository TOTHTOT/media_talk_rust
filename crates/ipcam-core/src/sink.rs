use crate::{AudioCodec, CoreResult, DecodedFrame, EncodedPacket, RtpTimestamp};

pub trait VideoSink: Send {
    fn on_access_unit(&mut self, packet: EncodedPacket) -> CoreResult<()>;

    fn codec(&self) -> crate::VideoCodec;

    fn rtp_ts(&self) -> RtpTimestamp {
        0
    }
}

pub trait AudioSink: Send {
    fn on_adts_frame(
        &mut self,
        codec: AudioCodec,
        data: bytes::Bytes,
        rtp_ts: RtpTimestamp,
    ) -> CoreResult<()>;
}

impl<T: FnMut(EncodedPacket) -> CoreResult<()> + Send> VideoSink for T {
    fn on_access_unit(&mut self, packet: EncodedPacket) -> CoreResult<()> {
        (self)(packet)
    }

    fn codec(&self) -> crate::VideoCodec {
        crate::VideoCodec::Unknown
    }
}

pub struct BufferedVideoSink {
    codec: crate::VideoCodec,
    inner: Vec<EncodedPacket>,
}

impl BufferedVideoSink {
    pub fn new(codec: crate::VideoCodec) -> Self {
        Self {
            codec,
            inner: Vec::new(),
        }
    }

    pub fn drain(&mut self) -> Vec<EncodedPacket> {
        std::mem::take(&mut self.inner)
    }
}

impl VideoSink for BufferedVideoSink {
    fn on_access_unit(&mut self, packet: EncodedPacket) -> CoreResult<()> {
        self.inner.push(packet);
        Ok(())
    }

    fn codec(&self) -> crate::VideoCodec {
        self.codec
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DecodedSinkStats {
    pub frames: u64,
    pub bytes: u64,
}

pub struct DecodedCounter {
    pub stats: parking_lot::Mutex<DecodedSinkStats>,
}

impl DecodedCounter {
    pub fn new() -> Self {
        Self {
            stats: parking_lot::Mutex::new(Default::default()),
        }
    }

    pub fn snapshot(&self) -> DecodedSinkStats {
        *self.stats.lock()
    }
}

impl Default for DecodedCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoSink for DecodedCounter {
    fn on_access_unit(&mut self, packet: EncodedPacket) -> CoreResult<()> {
        let mut s = self.stats.lock();
        s.frames += 1;
        s.bytes += packet.data.len() as u64;
        Ok(())
    }

    fn codec(&self) -> crate::VideoCodec {
        crate::VideoCodec::Unknown
    }
}

pub fn _frame_size_bytes(frame: &DecodedFrame) -> usize {
    (frame.stride as usize) * (frame.height as usize) * 3 / 2
}
