//! ICE 通话 (设置页验证功能): 控制器状态机 + WS 信令 + 手动 blob 通道.
//!
//! 本模块按职能拆分为三个子模块: 控制器/状态机 (`ice/ctl.rs`),
//! WS 信令 (`ice/sig.rs`), UI 装配 (`ice/ui.rs`); 文件内只留跨子模块
//! 共享的常量与类型.
//!
//! 状态机: Idle → Gathering (读 kv + 建 agent + 收本地候选) → Exchanging
//! (WS 角色交换 / 手动等导入) → Connecting (add_remote + 等 Connected) →
//! Active (媒体收发 + 1s 字节上报) → Idle (挂断/出错). 全程经
//! UiEvent::IceStatus 上报设置页状态行, 状态文案后端直出 (与 Weather
//! 不同, 这里本来就是工程验证文案, 不进 @tr).
//!
//! 线程模型照 weather::spawn: 独立 worker 线程 (ice-ctl) 持自建
//! current-thread tokio runtime; UI → worker 走 std mpsc 命令通道;
//! 状态/统计经 UiHandle::post 回投. 异步段 (WS / ICE) 用 rt.block_on
//! 分段驱动, 段间 drain_hangup 吸收用户在段内点的挂断 (段内不响应,
//! 段界生效 — 验证工具的合理取舍).
//!
//! WS 信令 (sigserver 协议, Task 4): connect → join → 等 joined 拿槽位
//! (slot 0 = offerer/controlling) → 按槽位建 agent → offerer 发本地
//! blob 等 answer, answerer 等 offer 回本地 blob. 对端晚于本端加入时
//! offer 被 sigserver 静默丢弃 — offerer 只能发完等 answer, 超时报错
//! (重连/补发不在本版范围). 通话建立后 WS 即关闭, 对端 hangup 转发
//! 不读 (记录在案).
//!
//! 手动通道 (ice_manual=1): 不起 WS, controlling=true; gather 完缓存
//! 本地 blob 供导出 (ice-export-blob), 停在导入等待 (同一命令通道收
//! ice-import-apply), 喂入后走同一连通/媒体路径.
//!
//! 挂断顺序纪律 (Task 3/5 review 裁决): 先 IceMediaHandle::stop()
//! (pipeline 置 Null, nicesink/nicesrc 停用), 再 IceAgent::close()
//! (glib 线程退出 + GObject 引用归还) — 元件还活在已 finalize 的
//! NiceAgent 上就是悬垂.

mod ctl;
mod sig;
mod ui;

use std::sync::mpsc::Sender;

use ipcam_gst::{GstStreamError, TrackSource};
use serde::{Deserialize, Serialize};

pub(crate) use ctl::{IceController, spawn};
pub(crate) use ui::wire_ice;

/// kv 键: sigserver WS 地址.
pub(crate) const KV_SIGSERVER: &str = "ice_sigserver";
/// kv 键: 信令房间名.
pub(crate) const KV_ROOM: &str = "ice_room";
/// kv 键: STUN URI; 显式置空 = 仅 host 候选 (本机环回调试).
pub(crate) const KV_STUN: &str = "ice_stun";
/// kv 键: 手动 blob 通道开关 ("1"/"0").
pub(crate) const KV_MANUAL: &str = "ice_manual";

/// 默认 sigserver 地址 (本机起 sigserver 即插即用).
pub(crate) const DEFAULT_SIGSERVER: &str = "ws://127.0.0.1:9000";
/// 默认房间名.
pub(crate) const DEFAULT_ROOM: &str = "test";
/// 默认 STUN 服务器.
pub(crate) const DEFAULT_STUN: &str = "stun://stun.l.google.com:19302";

/// WS 信令帧, 与 sigserver (media_talk_rust/src/bin/sigserver.rs) 同构.
/// 本结构是协议的消费端镜像, 字段增减必须与 sigserver 同步.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    room: String,
    #[serde(default)]
    payload: String,
    #[serde(default)]
    detail: String,
    /// joined 帧携带的槽位 (0=offerer, 1=answerer); 其余帧为 None.
    #[serde(default)]
    slot: Option<i32>,
}

/// ICE 控制器错误: 状态机各步骤的失败汇总, 全部经 IceStatus 上报展示.
#[derive(Debug, thiserror::Error)]
enum IceCtlError {
    #[error("读取 ICE 设置失败: {0}")]
    Kv(String),
    #[error("连接信令服务器失败: {0}")]
    Ws(String),
    #[error("信令交换失败: {0}")]
    Sig(String),
    #[error("ICE 连通性检查失败 (对端不可达?)")]
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

/// UI → ICE worker 命令 (std mpsc, 同 weather wake 模式).
enum IceCmd {
    Call,
    Hangup,
    /// 手动通道: 喂入对端 blob; Sender 是接受回执 (解析/连通结果走状态上报).
    ImportBlob(String, Sender<Result<(), String>>),
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
    manual: bool,
}
