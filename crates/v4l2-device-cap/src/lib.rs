use ipcam_core::{CoreError, CoreResult};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum V4l2Error {
    #[error("io error: {0}")]
    Io(String),
    #[error("not implemented")]
    NotImplemented,
}

impl From<V4l2Error> for CoreError {
    fn from(e: V4l2Error) -> Self {
        match e {
            V4l2Error::NotImplemented => CoreError::NotImplemented("V4L2"),
            V4l2Error::Io(s) => CoreError::Io(std::io::Error::other(s)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FrameSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct PixelFormatInfo {
    pub pixel_format: String,
    pub sizes: Vec<FrameSize>,
}

#[derive(Debug, Clone)]
pub struct V4l2Device {
    pub path: String,
    pub driver: String,
    pub bus_info: Option<String>,
    pub formats: Vec<PixelFormatInfo>,
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use nix::fcntl::{OFlag, open};
    use nix::sys::stat::Mode;
    use nix::unistd::close;
    use std::fs;

    const VIDIOC_QUERYCAP: u64 = 0x80685600;
    const VIDIOC_ENUM_FMT: u64 = 0xC0405602;
    const VIDIOC_ENUM_FRAMESIZES: u64 = 0xC02C564A;

    #[repr(C)]
    #[derive(Default, Clone)]
    struct V4l2Capability {
        driver: [u8; 16],
        card: [u8; 32],
        bus_info: [u8; 32],
        version: u32,
        capabilities: u32,
        device_caps: u32,
        reserved: [u32; 3],
    }

    #[repr(C)]
    #[derive(Default, Clone)]
    struct V4l2Fmtdesc {
        index: u32,
        r#type: u32,
        flags: u32,
        pixelformat: [u8; 4],
        reserved: [u32; 4],
        description: [u8; 32],
    }

    #[repr(C)]
    #[derive(Default, Clone)]
    struct V4l2Frmsizeenum {
        index: u32,
        pixel_format: u32,
        r#type: u32,
        union: [u32; 4],
        reserved: [u32; 2],
    }

    pub fn list_capture_devices() -> Result<Vec<V4l2Device>, V4l2Error> {
        let mut out = Vec::new();
        for entry in fs::read_dir("/dev").map_err(|e| V4l2Error::Io(e.to_string()))? {
            let entry = entry.map_err(|e| V4l2Error::Io(e.to_string()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("video") {
                continue;
            }
            let path = format!("/dev/{}", name);
            match probe(&path) {
                Ok(dev) => out.push(dev),
                Err(_) => continue,
            }
        }
        Ok(out)
    }

    fn probe(path: &str) -> Result<V4l2Device, V4l2Error> {
        let fd = open(path, OFlag::O_RDWR | OFlag::O_NONBLOCK, Mode::empty())
            .map_err(|e| V4l2Error::Io(format!("open {path}: {e}")))?;
        let cap = unsafe { ioctl_querycap(fd) };
        let driver = read_string(&cap.driver);
        let bus_info = Some(read_string(&cap.bus_info));
        let formats = unsafe { ioctl_enum_fmts(fd).unwrap_or_default() };
        let _ = close(fd);
        Ok(V4l2Device {
            path: path.to_string(),
            driver,
            bus_info,
            formats,
        })
    }

    unsafe fn ioctl_querycap(fd: i32) -> V4l2Capability {
        let mut cap = V4l2Capability::default();
        let r = unsafe { nix::libc::ioctl(fd, VIDIOC_QUERYCAP as _, &mut cap as *mut _) };
        if r != 0 {
            cap = V4l2Capability::default();
        }
        cap
    }

    unsafe fn ioctl_enum_fmts(fd: i32) -> Result<Vec<PixelFormatInfo>, V4l2Error> {
        let mut out = Vec::new();
        let mut idx = 0u32;
        loop {
            let mut fmtdesc = V4l2Fmtdesc {
                index: idx,
                r#type: 1,
                ..Default::default()
            };
            let r = unsafe { nix::libc::ioctl(fd, VIDIOC_ENUM_FMT as _, &mut fmtdesc as *mut _) };
            if r != 0 {
                break;
            }
            let pf_str = String::from_utf8_lossy(&fmtdesc.pixelformat).to_string();
            let sizes = unsafe { enum_frame_sizes(fd, fmtdesc.pixelformat) }.unwrap_or_default();
            out.push(PixelFormatInfo {
                pixel_format: pf_str,
                sizes,
            });
            idx += 1;
        }
        Ok(out)
    }

    unsafe fn enum_frame_sizes(fd: i32, pf: [u8; 4]) -> Result<Vec<FrameSize>, V4l2Error> {
        let mut out = Vec::new();
        let pf_u32: u32 = u32::from_le_bytes(pf);
        let mut idx = 0u32;
        loop {
            let mut e = V4l2Frmsizeenum {
                index: idx,
                pixel_format: pf_u32,
                r#type: 0,
                ..Default::default()
            };
            let r = unsafe { nix::libc::ioctl(fd, VIDIOC_ENUM_FRAMESIZES as _, &mut e as *mut _) };
            if r != 0 {
                break;
            }
            if e.r#type == 1 {
                let w = e.union[0];
                let h = e.union[1];
                out.push(FrameSize {
                    width: w,
                    height: h,
                });
            } else if e.r#type == 2 {
                let steps = e.union[2];
                let min_w = e.union[0];
                let max_w = e.union[1];
                let min_h = e.union[0];
                let max_h = e.union[1];
                let _ = (steps, min_w, max_w, min_h, max_h);
                out.push(FrameSize {
                    width: min_w,
                    height: min_h,
                });
            }
            idx += 1;
        }
        Ok(out)
    }

    fn read_string(buf: &[u8]) -> String {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).trim().to_string()
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    pub fn list_capture_devices() -> Result<Vec<V4l2Device>, V4l2Error> {
        Err(V4l2Error::NotImplemented)
    }
}

pub fn list_capture_devices() -> CoreResult<Vec<V4l2Device>> {
    imp::list_capture_devices().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_returns_result() {
        let r = list_capture_devices();
        if cfg!(target_os = "linux") {
            assert!(r.is_ok());
        } else {
            assert!(matches!(r, Err(CoreError::NotImplemented(_))));
        }
    }
}
