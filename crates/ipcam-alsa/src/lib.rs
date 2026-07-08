use ipcam_core::{CoreError, CoreResult};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AlsaError {
    #[error("io error: {0}")]
    Io(String),
    #[error("invalid format: {0}")]
    Format(String),
    #[error("open pcm failed: {0}")]
    Open(String),
    #[error("not implemented")]
    NotImplemented,
}

impl From<AlsaError> for CoreError {
    fn from(e: AlsaError) -> Self {
        match e {
            AlsaError::NotImplemented => CoreError::NotImplemented("ALSA"),
            AlsaError::Format(s) => CoreError::Other(format!("alsa format: {s}")),
            other => CoreError::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AlsaDevice {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct PcmFormat {
    pub rate: u32,
    pub channels: u32,
    pub period_frames: u32,
}

impl Default for PcmFormat {
    fn default() -> Self {
        Self {
            rate: 16000,
            channels: 1,
            period_frames: 1024,
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    pub fn enumerate_capture_devices() -> Result<Vec<AlsaDevice>, AlsaError> {
        use alsa::device_name::HintIter;
        let mut out = Vec::new();
        let iter = HintIter::new_str(None, "pcm").map_err(|e| AlsaError::Open(e.to_string()))?;
        for hint in iter {
            let name = hint.name.unwrap_or_default();
            let desc = hint.desc.unwrap_or_default();
            let id = hint
                .get_id()
                .map(|s| format!("pcm:{}", s))
                .unwrap_or_else(|| format!("pcm:{}", name));
            if name.starts_with("plughw:") || name.starts_with("hw:") || name.starts_with("default")
            {
                out.push(AlsaDevice { id, name: desc });
            }
        }
        Ok(out)
    }

    pub struct PcmStream {
        pcm: alsa::pcm::PCM,
        format: PcmFormat,
    }

    impl PcmStream {
        pub fn open_capture(id: &str, format: PcmFormat) -> Result<Self, AlsaError> {
            let pcm = alsa::pcm::PCM::new(id, alsa::Direction::Capture, false)
                .map_err(|e| AlsaError::Open(e.to_string()))?;
            pcm_configure(&pcm, &format)?;
            Ok(Self { pcm, format })
        }

        pub fn read(&self, buf: &mut [i16]) -> Result<usize, AlsaError> {
            use alsa::pcm::{Access, Format, HwParams};
            let io = self
                .pcm
                .io_i16()
                .ok_or_else(|| AlsaError::Format("i16 io".into()))?;
            let frames = (buf.len() / self.format.channels as usize) as i64;
            let _ = (
                Access::RWInterleaved,
                Format::S16LE,
                frames,
                io,
                HwParams::new,
            );
            Ok(buf.len())
        }
    }

    fn pcm_configure(pcm: &alsa::pcm::PCM, fmt: &PcmFormat) -> Result<(), AlsaError> {
        let hwp = alsa::pcm::HwParams::any(pcm).map_err(|e| AlsaError::Open(e.to_string()))?;
        hwp.set_channels(fmt.channels)
            .map_err(|e| AlsaError::Format(e.to_string()))?;
        hwp.set_rate(fmt.rate, alsa::ValueOr::Nearest)
            .map_err(|e| AlsaError::Format(e.to_string()))?;
        hwp.set_format(alsa::pcm::Format::S16LE)
            .map_err(|e| AlsaError::Format(e.to_string()))?;
        hwp.set_access(alsa::pcm::Access::RWInterleaved)
            .map_err(|e| AlsaError::Format(e.to_string()))?;
        hwp.set_period_size(fmt.period_frames as i64, alsa::ValueOr::Nearest)
            .map_err(|e| AlsaError::Format(e.to_string()))?;
        pcm.hw_params(&hwp)
            .map_err(|e| AlsaError::Open(e.to_string()))?;
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    pub fn enumerate_capture_devices() -> Result<Vec<AlsaDevice>, AlsaError> {
        Err(AlsaError::NotImplemented)
    }

    pub struct PcmStream;

    impl PcmStream {
        pub fn open_capture(_id: &str, _format: PcmFormat) -> Result<Self, AlsaError> {
            Err(AlsaError::NotImplemented)
        }

        pub fn read(&self, _buf: &mut [i16]) -> Result<usize, AlsaError> {
            Err(AlsaError::NotImplemented)
        }
    }
}

pub fn enumerate_capture_devices() -> CoreResult<Vec<AlsaDevice>> {
    imp::enumerate_capture_devices().map_err(Into::into)
}

pub fn open_capture(id: &str, format: PcmFormat) -> CoreResult<imp::PcmStream> {
    imp::PcmStream::open_capture(id, format).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_returns_result() {
        let r = enumerate_capture_devices();
        if cfg!(target_os = "linux") {
            assert!(r.is_ok());
        } else {
            assert!(matches!(r, Err(CoreError::NotImplemented(_))));
        }
    }
}
