//! SIP 通话设置卡: 启动时把 "CLI 覆盖 > kv > 内置默认" 合成 GuiSipConfig,
//! 并装配设置页 SIP 卡片的写 kv 回调 (下次呼叫生效).
//!
//! kv 缺失时用内置默认播种, 设置页总要有值可显示; 播种值是内置默认而非
//! CLI 覆盖 — 覆盖只影响本次启动, 不落库.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::rc::Rc;

use anyhow::bail;
use slint::ComponentHandle as _;
use tracing::{error, warn};

use crate::MainWindow;
use crate::app::{NoticeKind, NoticeLevel};
use crate::session::{
    DEFAULT_AUDIO_RTP_PORT, DEFAULT_VIDEO_RTP_PORT, GuiSipConfig, GuiSipOverrides, RtpPorts,
    SipIceConfig,
};
use crate::store::Store;

/// kv 键: SIP 服务器地址 (ip:port).
pub(crate) const KV_SIP_SERVER: &str = "sip_server";
/// kv 键: SIP 账号 (同时作为注册用户名).
pub(crate) const KV_SIP_USERNAME: &str = "sip_username";
/// kv 键: 账号密码 (digest 鉴权).
pub(crate) const KV_SIP_PASSWORD: &str = "sip_password";
/// kv 键: 注册有效期 (秒).
pub(crate) const KV_SIP_EXPIRES: &str = "sip_expires";
/// kv 键: SIP ICE 开关 ("1"/"0").
pub(crate) const KV_SIP_ICE: &str = "sip_ice";
/// kv 键: SIP STUN URI; 显式置空 = 仅 host 候选.
pub(crate) const KV_SIP_STUN: &str = "sip_stun";
/// kv 键: 强制绑定的本机 IPv4 网卡; 留空 = 自动选择网卡.
pub(crate) const KV_SIP_LOCAL_IP: &str = "sip_local_ip";
/// kv 键: 视频源原始串 ("camera[:设备]" / "file:路径" / "rtsp://...").
pub(crate) const KV_SIP_VIDEO_SRC: &str = "sip_video_src";
/// kv 键: 音频源原始串 ("mic" / "file:路径").
pub(crate) const KV_SIP_AUDIO_SRC: &str = "sip_audio_src";
/// kv 键: 视频限宽 (对端解码能力); 0 = 不限.
pub(crate) const KV_SIP_MAX_WIDTH: &str = "sip_max_width";
/// kv 键: 被叫侧音频 RTP 首选端口.
pub(crate) const KV_SIP_RTP_AUDIO: &str = "sip_rtp_audio";
/// kv 键: 被叫侧视频 RTP 首选端口.
pub(crate) const KV_SIP_RTP_VIDEO: &str = "sip_rtp_video";

/// 内置默认 SIP 服务器地址 (与原 CLI default_value 一致).
const DEFAULT_SERVER: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 11, 17), 5062));
/// 内置默认账号.
const DEFAULT_USERNAME: &str = "9999";
/// 内置默认密码.
const DEFAULT_PASSWORD: &str = "changeme";
/// 内置默认注册有效期 (秒).
const DEFAULT_EXPIRES: u32 = 60;
/// 内置默认视频源.
const DEFAULT_VIDEO_SRC: &str = "camera";
/// 内置默认音频源.
const DEFAULT_AUDIO_SRC: &str = "mic";
/// 内置默认视频限宽.
const DEFAULT_MAX_WIDTH: u32 = 1920;
/// 内置默认 STUN URI.
const DEFAULT_STUN: &str = "stun://stun.miwifi.com:3478";

/// 读 kv; 缺失时用默认值播种并返回默认 (设置页总要有值可显示).
/// 读取出错记日志回退默认 (库刚打开, 出错属异常路径).
fn kv_or_seed(store: &Store, key: &str, default: &str) -> String {
    match store.kv_get(key) {
        Ok(Some(v)) => v,
        Ok(None) => {
            if let Err(e) = store.kv_set(key, default) {
                error!(err = %e, key, "seed sip setting failed");
            }
            default.to_string()
        }
        Err(e) => {
            error!(err = %e, key, "read sip setting failed");
            default.to_string()
        }
    }
}

/// 解析 kv 字符串; 失败回退默认并记警告 — 坏 kv 不该让 GUI 起不来,
/// 卡片显示生效值 (回退结果), 用户重输即修复.
fn parse_kv<T: std::str::FromStr>(key: &str, raw: &str, default: T) -> T {
    match raw.trim().parse::<T>() {
        Ok(v) => v,
        Err(_) => {
            warn!(key, raw, "invalid sip setting in kv, fall back to default");
            default
        }
    }
}

/// 合成最终 SIP 配置: CLI 覆盖 > kv > 内置默认; kv 缺失时播种内置默认.
/// 媒体源解析与 local_ip 仅 IPv4 的报错语义与原 main.rs 装配完全一致.
///
/// # Arguments
///
/// * `store`: kv 持久化
/// * `over`: CLI 覆盖项, 全部 None 时完全由 kv/默认决定
///
/// returns: anyhow::Result<GuiSipConfig>
///
/// # Examples
///
/// ```
///
/// ```
pub fn resolve(store: &Store, over: &GuiSipOverrides) -> anyhow::Result<GuiSipConfig> {
    let server = over.server.unwrap_or_else(|| {
        parse_kv(
            KV_SIP_SERVER,
            &kv_or_seed(store, KV_SIP_SERVER, &DEFAULT_SERVER.to_string()),
            DEFAULT_SERVER,
        )
    });
    let username = over
        .username
        .clone()
        .unwrap_or_else(|| kv_or_seed(store, KV_SIP_USERNAME, DEFAULT_USERNAME));
    let password = over
        .password
        .clone()
        .unwrap_or_else(|| kv_or_seed(store, KV_SIP_PASSWORD, DEFAULT_PASSWORD));
    let expires = over.expires.unwrap_or_else(|| {
        parse_kv(
            KV_SIP_EXPIRES,
            &kv_or_seed(store, KV_SIP_EXPIRES, &DEFAULT_EXPIRES.to_string()),
            DEFAULT_EXPIRES,
        )
    });
    // CLI 只收 IPv4 (与原 main.rs 一致, IPv6 直接报错); kv 存 IP 字符串,
    // 留空/缺失 = 自动选网卡, 非法值忽略并记警告
    let local_ip = match over.local_ip {
        Some(IpAddr::V4(v4)) => Some(v4),
        Some(IpAddr::V6(_)) => bail!("only support IPv4 for now!"),
        None => {
            let raw = kv_or_seed(store, KV_SIP_LOCAL_IP, "");
            if raw.trim().is_empty() {
                None
            } else {
                match raw.trim().parse::<IpAddr>() {
                    Ok(IpAddr::V4(v4)) => Some(v4),
                    Ok(IpAddr::V6(_)) => {
                        warn!(raw, "sip local_ip in kv is IPv6, ignored (IPv4 only)");
                        None
                    }
                    Err(_) => {
                        warn!(raw, "sip local_ip in kv unparsable, ignored");
                        None
                    }
                }
            }
        }
    };
    // 源存 CLI 原始串, 合成时才解析 (与原 main.rs 相同, 解析错直接报错退出)
    let video_src_raw = over
        .video_src
        .clone()
        .unwrap_or_else(|| kv_or_seed(store, KV_SIP_VIDEO_SRC, DEFAULT_VIDEO_SRC));
    let video_src = ipcam_gst::parse_track_source(&video_src_raw).map_err(anyhow::Error::msg)?;
    let audio_src_raw = over
        .audio_src
        .clone()
        .unwrap_or_else(|| kv_or_seed(store, KV_SIP_AUDIO_SRC, DEFAULT_AUDIO_SRC));
    let audio_src = ipcam_gst::parse_track_source(&audio_src_raw).map_err(anyhow::Error::msg)?;
    let max_width = over.max_width.unwrap_or_else(|| {
        parse_kv(
            KV_SIP_MAX_WIDTH,
            &kv_or_seed(store, KV_SIP_MAX_WIDTH, &DEFAULT_MAX_WIDTH.to_string()),
            DEFAULT_MAX_WIDTH,
        )
    });
    let ice = over
        .ice
        .unwrap_or_else(|| kv_or_seed(store, KV_SIP_ICE, "0") == "1");
    // STUN 与原 main.rs 一致: 空串 = None (仅 host 候选), 不 trim
    let stun_raw = over
        .stun
        .clone()
        .unwrap_or_else(|| kv_or_seed(store, KV_SIP_STUN, DEFAULT_STUN));
    let stun = (!stun_raw.is_empty()).then_some(stun_raw);
    let video_rtp_port = over.video_rtp_port.unwrap_or_else(|| {
        parse_kv(
            KV_SIP_RTP_VIDEO,
            &kv_or_seed(store, KV_SIP_RTP_VIDEO, &DEFAULT_VIDEO_RTP_PORT.to_string()),
            DEFAULT_VIDEO_RTP_PORT,
        )
    });
    let audio_rtp_port = over.audio_rtp_port.unwrap_or_else(|| {
        parse_kv(
            KV_SIP_RTP_AUDIO,
            &kv_or_seed(store, KV_SIP_RTP_AUDIO, &DEFAULT_AUDIO_RTP_PORT.to_string()),
            DEFAULT_AUDIO_RTP_PORT,
        )
    });
    Ok(GuiSipConfig {
        server,
        username,
        password,
        expires,
        local_ip,
        audio_src,
        video_src,
        max_width,
        rtp_ports: RtpPorts {
            audio: audio_rtp_port,
            video: video_rtp_port,
        },
        ice: SipIceConfig { enabled: ice, stun },
    })
}

/// SIP 设置页回显值: resolve 合成结果的字符串形式, 启动时刷进卡片输入框
/// (媒体源从 TrackSource 反向序列化, 与 parse_track_source 语法一一对应).
pub(crate) struct SipUiStrings {
    pub server: String,
    pub username: String,
    pub password: String,
    pub expires: String,
    pub stun: String,
    pub local_ip: String,
    pub video_src: String,
    pub audio_src: String,
    pub max_width: String,
    pub rtp_audio: String,
    pub rtp_video: String,
}

/// TrackSource 反向序列化为 CLI 原始串 (file:路径 / rtsp://uri / camera[:设备] / mic).
fn src_raw(src: &ipcam_gst::TrackSource) -> String {
    match src {
        ipcam_gst::TrackSource::File(p) => format!("file:{}", p.display()),
        ipcam_gst::TrackSource::Rtsp { uri } => uri.clone(),
        ipcam_gst::TrackSource::LocalCamera { device: None } => "camera".to_string(),
        ipcam_gst::TrackSource::LocalCamera { device: Some(d) } => format!("camera:{d}"),
        ipcam_gst::TrackSource::Mic => "mic".to_string(),
    }
}

/// GuiSipConfig → 卡片回显字符串.
pub(crate) fn ui_strings(cfg: &GuiSipConfig) -> SipUiStrings {
    SipUiStrings {
        server: cfg.server.to_string(),
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        expires: cfg.expires.to_string(),
        stun: cfg.ice.stun.clone().unwrap_or_default(),
        local_ip: cfg.local_ip.map(|ip| ip.to_string()).unwrap_or_default(),
        video_src: src_raw(&cfg.video_src),
        audio_src: src_raw(&cfg.audio_src),
        max_width: cfg.max_width.to_string(),
        rtp_audio: cfg.rtp_ports.audio.to_string(),
        rtp_video: cfg.rtp_ports.video.to_string(),
    }
}

/// 装配 SIP 设置页回调: 文本项 trim 后写 kv (下次呼叫生效), ICE 开关写
/// "1"/"0"; 数字项 (注册有效期/限宽/RTP 端口) 在 Rust 侧解析, 非法输入
/// 静默保留旧值 — 输入即提交, 中间态必然非法, 弹 toast 只会刷屏.
///
/// # Arguments
///
/// * `ui`: 主窗口
/// * `store`: kv 持久化
pub(crate) fn wire_sip_settings(ui: &MainWindow, store: &Rc<Store>) {
    /// 设置保存成功的 toast.
    fn toast_saved(ui: &MainWindow) {
        ui.invoke_notify(
            NoticeLevel::Info as i32,
            NoticeKind::Raw as i32,
            "已保存, 下次呼叫生效".into(),
        );
    }

    // 服务器地址
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_sip_server(move |v| {
            if let Err(e) = store.kv_set(KV_SIP_SERVER, v.trim()) {
                error!(err = %e, "save sip server failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 账号
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_sip_username(move |v| {
            if let Err(e) = store.kv_set(KV_SIP_USERNAME, v.trim()) {
                error!(err = %e, "save sip username failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 密码
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_sip_password(move |v| {
            if let Err(e) = store.kv_set(KV_SIP_PASSWORD, v.trim()) {
                error!(err = %e, "save sip password failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 注册有效期 (秒): 非法输入静默保留旧值
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_sip_expires(move |v| {
            let Ok(n) = v.trim().parse::<u32>() else {
                return;
            };
            if let Err(e) = store.kv_set(KV_SIP_EXPIRES, &n.to_string()) {
                error!(err = %e, "save sip expires failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // STUN URI (留空 = 仅 host 候选)
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_sip_stun(move |v| {
            if let Err(e) = store.kv_set(KV_SIP_STUN, v.trim()) {
                error!(err = %e, "save sip stun failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 本机 IP (留空 = 自动选择网卡)
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_sip_local_ip(move |v| {
            if let Err(e) = store.kv_set(KV_SIP_LOCAL_IP, v.trim()) {
                error!(err = %e, "save sip local ip failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 视频源
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_sip_video_src(move |v| {
            if let Err(e) = store.kv_set(KV_SIP_VIDEO_SRC, v.trim()) {
                error!(err = %e, "save sip video src failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 音频源
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_sip_audio_src(move |v| {
            if let Err(e) = store.kv_set(KV_SIP_AUDIO_SRC, v.trim()) {
                error!(err = %e, "save sip audio src failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 视频限宽: 非法输入静默保留旧值
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_sip_max_width(move |v| {
            let Ok(n) = v.trim().parse::<u32>() else {
                return;
            };
            if let Err(e) = store.kv_set(KV_SIP_MAX_WIDTH, &n.to_string()) {
                error!(err = %e, "save sip max width failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // ICE 开关
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_sip_ice(move |v| {
            if let Err(e) = store.kv_set(KV_SIP_ICE, if v { "1" } else { "0" }) {
                error!(err = %e, "save sip ice failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 音频 RTP 端口: 非法输入静默保留旧值
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_sip_rtp_audio(move |v| {
            let Ok(n) = v.trim().parse::<u16>() else {
                return;
            };
            if let Err(e) = store.kv_set(KV_SIP_RTP_AUDIO, &n.to_string()) {
                error!(err = %e, "save sip rtp audio port failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 视频 RTP 端口: 非法输入静默保留旧值
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_sip_rtp_video(move |v| {
            let Ok(n) = v.trim().parse::<u16>() else {
                return;
            };
            if let Err(e) = store.kv_set(KV_SIP_RTP_VIDEO, &n.to_string()) {
                error!(err = %e, "save sip rtp video port failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
}
