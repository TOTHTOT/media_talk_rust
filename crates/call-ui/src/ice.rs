//! ICE 通话 (设置页验证功能): 控制器状态机 + WS 信令.
//!
//! WS 信令已下沉到 `ipcam_ice::signal`; 本模块按职能拆分为两个子模块:
//! 控制器/状态机 (`ice/ctl.rs`) 与 UI 装配 (`ice/ui.rs`); 文件内只留
//! 跨子模块共享的常量与类型.
//!
//! 状态机: Idle → Gathering (读 kv + 建 agent + 收本地候选) → Exchanging
//! (WS 角色交换) → Connecting (add_remote + 等 Connected) →
//! Active (媒体收发 + 1s 字节上报) → Idle (挂断/出错). 全程经
//! UiEvent::IceStatus 上报设置页状态行, 状态文案后端直出 (与 Weather
//! 不同, 这里本来就是工程验证文案, 不进 @tr).
//!
//! 线程模型照 weather::spawn: 独立 worker 线程 (ice-ctl) 持自建
//! current-thread tokio runtime; UI → worker 走 tokio unbounded 命令通道;
//! 状态/统计经 UiHandle::post 回投. 所有异步段直接 await, 仅线程入口
//! 一次 block_on.
//!
//! WS 信令 (sigserver 协议): connect → join → 等 joined 拿槽位
//! (slot 0 = offerer/controlling) → 按槽位建 agent → offerer 发本地
//! blob 等 answer, answerer 等 offer 回本地 blob. sigserver 会缓存
//! offerer 的最后一条 offer/blob, answerer 晚加入时补发; 旧版服务器
//! 不支持缓存时仍由超时兜底. 通话建立后 WS 即关闭, 对端 hangup 转发
//! 不读 (记录在案).
//!
//! 挂断顺序纪律 (Task 3/5 review 裁决): 先 IceMediaHandle::stop()
//! (pipeline 置 Null, nicesink/nicesrc 停用), 再 IceAgent::close()
//! (glib 线程退出 + GObject 引用归还) — 元件还活在已 finalize 的
//! NiceAgent 上就是悬垂.

mod ctl;
mod ui;

use ipcam_gst::{GstStreamError, TrackSource};

pub(crate) use ctl::{IceController, spawn};
pub(crate) use ui::wire_ice;

/// kv 键: sigserver WS 地址.
pub(crate) const KV_SIGSERVER: &str = "ice_sigserver";
/// kv 键: 信令房间名.
pub(crate) const KV_ROOM: &str = "ice_room";
/// kv 键: STUN URI; 显式置空 = 仅 host 候选 (本机环回调试).
pub(crate) const KV_STUN: &str = "ice_stun";

/// 默认 sigserver 地址 (本机起 sigserver 即插即用).
pub(crate) const DEFAULT_SIGSERVER: &str = "ws://127.0.0.1:9000";
/// 默认房间名.
pub(crate) const DEFAULT_ROOM: &str = "test";
/// 默认 STUN 服务器.
pub(crate) const DEFAULT_STUN: &str = "stun://stun.miwifi.com:3478";

/// ICE 控制器错误: 状态机各步骤的失败汇总, 全部经 IceStatus 上报展示.
#[derive(Debug, thiserror::Error)]
enum IceCtlError {
    #[error("failed to read ICE settings: {0}")]
    Kv(String),
    #[error(transparent)]
    Signal(#[from] ipcam_ice::signal::SignalError),
    #[error("signaling exchange failed: {0}")]
    Sig(String),
    #[error("ICE connectivity check failed (peer unreachable?)")]
    IceFailed,
    #[error(transparent)]
    Agent(#[from] ipcam_ice::IceError),
    #[error(transparent)]
    Media(#[from] GstStreamError),
}

/// 控制器对外可见的状态机阶段 (Task 7 卡片据此置 calling 标志).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IceInner {
    #[default]
    Idle,
    Gathering,
    Exchanging,
    Connecting,
    Active,
}

/// UI → ICE worker 命令 (tokio unbounded, 同 weather wake 模式).
enum IceCmd {
    Call,
    Hangup,
}

/// worker 每通电话复用的本机媒体配置 (run() 时从 GuiSipConfig 摘录).
pub(crate) struct IceSources {
    /// 发送视频源; Mic 不是合法视频源 → None (只收不放).
    pub video_src: Option<TrackSource>,
    /// 是否发送音频: 仅当 audio_src=Mic 时 true (ICE 音频发送链固定采麦克风).
    pub audio_send: bool,
    /// 发送限宽 (0 = 不限).
    pub max_width: u32,
}

/// 呼叫设置快照: 每次呼叫从 kv 现读 (设置页改动下次呼叫生效).
struct IceSettings {
    sigserver: String,
    room: String,
    stun: Option<String>,
}
