//! 屏幕开关: 仅 RK3566 上真实关闭显示.
//!
//! 优先级: 背光 (backlight/leds) > DRM DPMS > fb blank. 熄屏后 Slint
//! 事件循环与触摸输入仍正常, 主窗遮罩接收任意点击后再调 [`ScreenController::set_off`]
//! 恢复显示. 其他平台不探测设备, 按钮在 slint 侧只弹 toast.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

const BACKLIGHT_DIR: &str = "/sys/class/backlight";
const LEDS_DIR: &str = "/sys/class/leds";
const DRM_DIR: &str = "/sys/class/drm";
const FB_BLANK: &str = "/sys/class/graphics/fb0/blank";

/// 用户设置亮度在 kv 表中的键 (百分比 5..100)
pub(crate) const KV_BRIGHTNESS: &str = "screen_brightness";
/// 亮度下限: 0 会让屏幕全黑, 与熄屏键语义冲突, 设置页保底 5%
pub(crate) const BRIGHTNESS_MIN_PERCENT: u32 = 5;

/// RK3566 的设备树/model 可能出现在这些节点; 任一节点命中即视为目标板.
const PLATFORM_MARKERS: [&str; 5] = [
    "/proc/device-tree/compatible",
    "/sys/firmware/devicetree/base/compatible",
    "/proc/device-tree/model",
    "/sys/firmware/devicetree/base/model",
    "/sys/devices/soc0/machine",
];

pub(crate) struct ScreenController {
    backend: Option<Backend>,
    /// DSI connector 的 status 文件: 写 off 强制断开 (内核会 DPMS off),
    /// 写 detect 恢复. 这是本板唯一可写的 DSI 电源控制入口.
    connector_status: Vec<PathBuf>,
    off: bool,
}

enum Backend {
    Backlight(Backlight),
    Drm(Vec<PathBuf>),
    FbBlank(PathBuf),
}

struct Backlight {
    brightness: PathBuf,
    brightness_writable: bool,
    /// 有 bl_power 时一并写入; 某些 RK3566 内核只认 brightness 变化
    bl_power: Option<PathBuf>,
    /// 熄屏前亮度; 无 brightness 节点时为 0
    saved_brightness: u32,
    max_brightness: u32,
}

impl ScreenController {
    pub(crate) fn new() -> Self {
        let on_target = cfg!(target_os = "linux") && running_on_rk3566();
        let backend = if on_target { detect_backend() } else { None };
        let connector_status = if on_target {
            detect_connector_status()
        } else {
            Vec::new()
        };
        match &backend {
            Some(Backend::Backlight(b)) => {
                info!(brightness = %b.brightness.display(), "screen control: backlight")
            }
            Some(Backend::Drm(paths)) => {
                info!(connectors = paths.len(), "screen control: drm dpms")
            }
            Some(Backend::FbBlank(path)) => {
                info!(path = %path.display(), "screen control: fb blank")
            }
            None => info!("screen control unavailable"),
        }
        if !connector_status.is_empty() {
            info!(
                connectors = connector_status.len(),
                "screen control: dsi status"
            );
        }
        Self {
            backend,
            connector_status,
            off: false,
        }
    }

    /// 当前平台/设备是否支持真实熄屏 (UI 据此决定走硬件还是 toast).
    pub(crate) fn supported(&self) -> bool {
        self.backend.is_some() || !self.connector_status.is_empty()
    }

    pub(crate) fn is_off(&self) -> bool {
        self.off
    }

    /// 是否支持亮度调节 (需要可写 brightness 的背光设备).
    pub(crate) fn brightness_available(&self) -> bool {
        matches!(&self.backend, Some(Backend::Backlight(b)) if b.brightness_writable)
    }

    /// 当前背光亮度百分比; 无背光/不可读时返回 None.
    pub(crate) fn brightness_percent(&self) -> Option<u32> {
        match &self.backend {
            Some(Backend::Backlight(b)) => b.percent(),
            _ => None,
        }
    }

    /// 设置背光亮度百分比 (5..100). 熄屏期间只记录目标值, 亮屏时恢复.
    pub(crate) fn set_brightness_percent(&mut self, percent: u32) -> io::Result<()> {
        let Some(Backend::Backlight(b)) = self.backend.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "brightness control unsupported",
            ));
        };
        if self.off {
            b.saved_brightness = raw_from_percent(percent, b.max_brightness);
            return Ok(());
        }
        b.set_percent(percent)
    }

    /// 开/关显示. 重复调用同一状态直接成功; 失败时不改 `off`, 便于 UI 回滚.
    pub(crate) fn set_off(&mut self, off: bool) -> io::Result<()> {
        if self.off == off {
            return Ok(());
        }
        if off {
            // 先关背光/主显示, 再强制 DSI 断开 (内核随后会把 connector DPMS 置 Off)
            let mut ok = false;
            let mut last_err = None;
            if let Some(backend) = self.backend.as_mut() {
                match backend.set_off(true) {
                    Ok(()) => ok = true,
                    Err(e) => last_err = Some(e),
                }
            }
            if !self.connector_status.is_empty() {
                match write_all(&self.connector_status, "off") {
                    Ok(()) => ok = true,
                    Err(e) => last_err = Some(e),
                }
            }
            if !ok {
                return Err(last_err.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::Unsupported, "screen control unsupported")
                }));
            }
        } else {
            // DSI 先恢复, 再恢复背光; 否则会出现黑屏亮背光
            if !self.connector_status.is_empty() {
                write_all(&self.connector_status, "detect")?;
            }
            if let Some(backend) = self.backend.as_mut() {
                backend.set_off(false)?;
            }
        }
        self.off = off;
        info!(off, "screen display toggled");
        Ok(())
    }
}

impl Backend {
    fn set_off(&mut self, off: bool) -> io::Result<()> {
        match self {
            Backend::Backlight(b) => {
                if off {
                    b.power_off()
                } else {
                    b.power_on()
                }
            }
            Backend::Drm(paths) => write_all(paths, if off { "off" } else { "on" }),
            Backend::FbBlank(path) => fs::write(path, if off { "4" } else { "0" }),
        }
    }
}

impl Backlight {
    fn power_off(&mut self) -> io::Result<()> {
        // brightness 归零是板端最可靠的熄屏手段 (实测 bl_power 单独写 1
        // 不一定改变实际亮度); 有 bl_power 时再补一刀.
        if self.brightness_writable {
            if let Some(v) = read_u32(&self.brightness) {
                self.saved_brightness = v;
            }
            fs::write(&self.brightness, "0")?;
        }
        if let Some(bl_power) = &self.bl_power {
            if let Err(e) = fs::write(bl_power, "1") {
                if !self.brightness_writable {
                    return Err(e);
                }
                warn!(path = %bl_power.display(), err = %e, "write bl_power failed");
            }
        }
        Ok(())
    }

    fn power_on(&mut self) -> io::Result<()> {
        let mut bl_err = None;
        if let Some(bl_power) = &self.bl_power {
            if let Err(e) = fs::write(bl_power, "0") {
                bl_err = Some(e);
            }
        }
        if self.brightness_writable {
            fs::write(&self.brightness, self.restore_brightness().to_string())?;
            return Ok(());
        }
        match bl_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn restore_brightness(&self) -> u32 {
        if self.saved_brightness == 0 {
            self.max_brightness.max(1)
        } else {
            self.saved_brightness
        }
    }

    fn percent(&self) -> Option<u32> {
        let raw = read_u32(&self.brightness)?;
        Some(percent_from_raw(raw, self.max_brightness))
    }

    fn set_percent(&self, percent: u32) -> io::Result<()> {
        if !self.brightness_writable {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "backlight brightness not writable",
            ));
        }
        let raw = raw_from_percent(percent, self.max_brightness);
        fs::write(&self.brightness, raw.to_string())
    }
}

/// 熄屏设备探测: 背光最稳, 其次 DRM connector DPMS, 最后 fb blank.
fn detect_backend() -> Option<Backend> {
    detect_backlight()
        .or_else(detect_led_backlight)
        .or_else(detect_drm)
        .or_else(detect_fb_blank)
}

fn detect_backlight() -> Option<Backend> {
    let entries = fs::read_dir(BACKLIGHT_DIR).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        let bl_power = dir.join("bl_power");
        let brightness = dir.join("brightness");
        let bl_power = is_writable(&bl_power).then_some(bl_power);
        let brightness_writable = is_writable(&brightness);
        if bl_power.is_none() && !brightness_writable {
            continue;
        }
        let saved = read_u32(&brightness).unwrap_or(0);
        let max = read_u32(&dir.join("max_brightness"))
            .unwrap_or(saved.max(1))
            .max(1);
        return Some(Backend::Backlight(Backlight {
            brightness,
            brightness_writable,
            bl_power,
            saved_brightness: saved,
            max_brightness: max,
        }));
    }
    None
}

/// 部分 ARM 板把 LCD 背光注册在 leds 子系统而不是 backlight.
fn detect_led_backlight() -> Option<Backend> {
    let entries = fs::read_dir(LEDS_DIR).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !name.contains("backlight") && !name.contains("lcd") {
            continue;
        }
        let dir = entry.path();
        let brightness = dir.join("brightness");
        if !is_writable(&brightness) {
            continue;
        }
        let saved = read_u32(&brightness).unwrap_or(0);
        let max = read_u32(&dir.join("max_brightness"))
            .unwrap_or(saved.max(1))
            .max(1);
        return Some(Backend::Backlight(Backlight {
            brightness,
            brightness_writable: true,
            bl_power: None,
            saved_brightness: saved,
            max_brightness: max,
        }));
    }
    None
}

fn detect_drm() -> Option<Backend> {
    let mut connected = Vec::new();
    let mut all = Vec::new();
    for entry in fs::read_dir(DRM_DIR).ok()?.flatten() {
        let dir = entry.path();
        let dpms = dir.join("dpms");
        if !is_writable(&dpms) {
            continue;
        }
        all.push(dpms.clone());
        let is_connected = fs::read_to_string(dir.join("status"))
            .map(|s| s.trim() == "connected")
            .unwrap_or(false);
        if is_connected {
            connected.push(dpms);
        }
    }
    if !connected.is_empty() {
        Some(Backend::Drm(connected))
    } else if !all.is_empty() {
        Some(Backend::Drm(all))
    } else {
        None
    }
}

/// 本板 DSI connector 的 `dpms` 是只读的, 但 `status` 可写:
/// 写 "off" 强制 disconnected (内核会联动把 DPMS 置 Off),
/// 写 "detect" 恢复热插拔检测 (DPMS 回到 On).
fn detect_connector_status() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Ok(entries) = fs::read_dir(DRM_DIR) else {
        return paths;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !is_dsi_connector(&name.to_string_lossy()) {
            continue;
        }
        let status = entry.path().join("status");
        if is_writable(&status) {
            paths.push(status);
        }
    }
    paths
}

fn is_dsi_connector(name: &str) -> bool {
    name.contains("-DSI-")
}

fn detect_fb_blank() -> Option<Backend> {
    let path = PathBuf::from(FB_BLANK);
    is_writable(&path).then_some(Backend::FbBlank(path))
}

fn is_writable(path: &Path) -> bool {
    fs::OpenOptions::new().write(true).open(path).is_ok()
}

fn read_u32(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn percent_from_raw(raw: u32, max: u32) -> u32 {
    let max = max.max(1) as u64;
    let raw = (raw as u64).min(max);
    ((raw * 100 + max / 2) / max) as u32
}

fn raw_from_percent(percent: u32, max: u32) -> u32 {
    let max = max.max(1) as u64;
    let percent = (percent as u64).min(100);
    ((max * percent + 50) / 100).min(max) as u32
}

fn write_all(paths: &[PathBuf], value: &str) -> io::Result<()> {
    let mut ok = 0usize;
    let mut last_err = None;
    for path in paths {
        match fs::write(path, value) {
            Ok(()) => ok += 1,
            Err(e) => {
                warn!(path = %path.display(), value, err = %e, "write display control failed");
                last_err = Some(e);
            }
        }
    }
    if ok > 0 {
        return Ok(());
    }
    Err(last_err.unwrap_or_else(|| io::Error::other("no drm connector")))
}

fn running_on_rk3566() -> bool {
    PLATFORM_MARKERS
        .iter()
        .any(|path| contains_marker(path, "rk3566"))
}

fn contains_marker(path: &str, marker: &str) -> bool {
    fs::read(path)
        .map(|bytes| contains_marker_bytes(&bytes, marker))
        .unwrap_or(false)
}

fn contains_marker_bytes(bytes: &[u8], marker: &str) -> bool {
    String::from_utf8_lossy(bytes)
        .to_ascii_lowercase()
        .contains(marker)
}

#[cfg(all(test, target_os = "linux", target_arch = "aarch64"))]
mod tests {
    use super::*;

    #[test]
    fn detects_rk3566_compatible_string() {
        assert!(contains_marker_bytes(
            b"rockchip,rk3566\0rockchip,rk356x\0",
            "rk3566"
        ));
        assert!(contains_marker_bytes(b"Taishan Pi RK3566", "rk3566"));
        assert!(!contains_marker_bytes(b"rockchip,rk3568", "rk3566"));
    }

    #[test]
    fn restore_brightness_never_zero() {
        let backlight = Backlight {
            brightness: PathBuf::from("/tmp/brightness"),
            brightness_writable: true,
            bl_power: None,
            saved_brightness: 0,
            max_brightness: 255,
        };
        assert_eq!(backlight.restore_brightness(), 255);

        let backlight = Backlight {
            saved_brightness: 80,
            ..backlight
        };
        assert_eq!(backlight.restore_brightness(), 80);
    }

    #[test]
    fn detects_dsi_connector_names() {
        assert!(is_dsi_connector("card0-DSI-1"));
        assert!(!is_dsi_connector("card0-HDMI-A-1"));
        assert!(!is_dsi_connector("card0-Writeback-1"));
    }

    #[test]
    fn brightness_percent_roundtrip() {
        assert_eq!(raw_from_percent(5, 255), 13);
        assert_eq!(percent_from_raw(13, 255), 5);
        assert_eq!(raw_from_percent(50, 255), 128);
        assert_eq!(percent_from_raw(128, 255), 50);
        assert_eq!(raw_from_percent(100, 255), 255);
        assert_eq!(percent_from_raw(255, 255), 100);
    }

    #[test]
    fn backlight_toggles_brightness_and_bl_power() {
        let dir =
            std::env::temp_dir().join(format!("call-ui-screen-backlight-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let brightness = dir.join("brightness");
        let bl_power = dir.join("bl_power");
        fs::write(&brightness, "200").unwrap();
        fs::write(&bl_power, "0").unwrap();

        let mut backlight = Backlight {
            brightness: brightness.clone(),
            brightness_writable: true,
            bl_power: Some(bl_power.clone()),
            saved_brightness: 200,
            max_brightness: 255,
        };
        backlight.power_off().unwrap();
        assert_eq!(fs::read_to_string(&brightness).unwrap(), "0");
        assert_eq!(fs::read_to_string(&bl_power).unwrap(), "1");

        backlight.power_on().unwrap();
        assert_eq!(fs::read_to_string(&brightness).unwrap(), "200");
        assert_eq!(fs::read_to_string(&bl_power).unwrap(), "0");

        backlight.set_percent(50).unwrap();
        assert_eq!(fs::read_to_string(&brightness).unwrap(), "128");
        assert_eq!(backlight.percent(), Some(50));

        fs::remove_dir_all(&dir).unwrap();
    }
}
