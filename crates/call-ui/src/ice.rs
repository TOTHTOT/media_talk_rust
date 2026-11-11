//! ICE 通话 (设置页验证功能): 控制器状态机 + WS 信令 + 手动 blob 通道.
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

use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use ipcam_gst::{
    AudioRecv, GstStreamError, IceMediaConfig, IceMediaHandle, RawTaps, TrackSource,
    VideoFrameSink, start_ice_media,
};
use ipcam_ice::blob::IceBlob;
use ipcam_ice::{IceAgent, IceConfig, IceState};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use slint::ComponentHandle as _;
use tokio::runtime::Runtime;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use crate::MainWindow;
use crate::app::{NoticeKind, NoticeLevel, UiEvent};
use crate::bridge::UiHandle;
use crate::store::Store;

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

/// WS 建连超时 (局域网验证工具, 不给长).
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 等 joined 等单帧的通用超时.
const WS_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
/// offerer 等 answer 的超时: 对端晚加入时 offer 已被丢弃, 只能靠超时解脱.
const ANSWER_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
/// ICE 连通性检查等待上限.
const ICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 通话期统计上报间隔.
const STATS_TICK: Duration = Duration::from_secs(1);
/// apply_remote_blob 等回执的 UI 侧上限 (正常路径 worker 停在导入等待, 立即回执).
const IMPORT_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

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

/// UI 侧持有的 ICE 控制器: 方法只发命令 / 读共享缓存, 业务全在 worker
/// 线程 (ice-ctl). Clone 供多个回调各自捕获.
#[derive(Clone)]
pub(crate) struct IceController {
    inner: Arc<Mutex<IceInner>>,
    /// worker gather 出的本地 blob 文本, 供手动导出; None = 当前没有.
    stash: Arc<Mutex<Option<String>>>,
    cmd_tx: Sender<IceCmd>,
}

impl IceController {
    /// 开始呼叫 (空闲时有效; 占线忽略并记日志 — worker 侧另有权威守卫).
    pub fn call(&self) {
        if self.state() != IceInner::Idle {
            warn!("ice call ignored: busy");
            return;
        }
        if let Err(e) = self.cmd_tx.send(IceCmd::Call) {
            warn!(err = %e, "ice call send failed");
        }
    }

    /// 挂断进行中的呼叫 (空闲时无操作).
    pub fn hangup(&self) {
        if let Err(e) = self.cmd_tx.send(IceCmd::Hangup) {
            warn!(err = %e, "ice hangup send failed");
        }
    }

    /// 当前状态机阶段.
    pub fn state(&self) -> IceInner {
        *self.inner.lock()
    }

    /// 导出本地 blob 文本 (手动通道的对端搬运载体).
    ///
    ///
    ///
    /// returns: Result<String, String> — Ok 为 blob 原文 (libnice SDP);
    /// Err 为不可导出的原因提示
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn local_blob_text(&self) -> Result<String, String> {
        self.stash
            .lock()
            .clone()
            .ok_or_else(|| "暂无本地 blob: 先开始呼叫, 状态变为收集中后即可导出".to_string())
    }

    /// 喂入对端 blob 文本 (手动通道).
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `text`: 对端导出的 blob 原文 (libnice SDP 文本)
    ///
    /// returns: Result<(), String> — Ok 仅表示已被呼叫流程接受; 解析与
    /// 连通结果经 IceStatus 状态行上报
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn apply_remote_blob(&self, text: &str) -> Result<(), String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("对端 blob 为空".into());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.cmd_tx
            .send(IceCmd::ImportBlob(text.to_string(), tx))
            .map_err(|_| "ICE worker 已退出".to_string())?;
        // 正常路径 worker 停在导入等待, 立即回执; 呼叫未开始/不在等待
        // 阶段时由 worker 回 Err. 超时兜底防 UI 线程长卡.
        rx.recv_timeout(IMPORT_REPLY_TIMEOUT)
            .map_err(|_| "等待导入超时: 请先点击呼叫, 状态显示收集中后再导入".to_string())?
    }
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

/// 起 ICE worker 线程 (ice-ctl) 并返回控制器. 线程模型照 weather::spawn:
/// std mpsc 命令通道驱动, worker 自建 current-thread tokio runtime.
///
///
///
/// # Arguments
///
/// * `handle`: UiEvent 回投句柄 (worker → 设置页状态行)
/// * `sources`: 本机媒体源配置 (每通电话复用, 来自 GuiSipConfig)
/// * `tap`: 收端视频帧回调 (与 SIP 通话同一个 GUI 纹理通路)
///
/// returns: Result<(IceController, JoinHandle<()>), std::io::Error> —
/// 控制器给 UI 回调, join 句柄给 run() 保活
///
/// # Examples
///
/// ```
///
/// ```
pub(crate) fn spawn(
    handle: UiHandle,
    sources: IceSources,
    tap: VideoFrameSink,
) -> std::io::Result<(IceController, JoinHandle<()>)> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<IceCmd>();
    let inner = Arc::new(Mutex::new(IceInner::Idle));
    let stash = Arc::new(Mutex::new(None));
    let ctl = IceController {
        inner: inner.clone(),
        stash: stash.clone(),
        cmd_tx,
    };
    let join = std::thread::Builder::new()
        .name("ice-ctl".into())
        .spawn(move || run_loop(cmd_rx, handle, inner, stash, sources, tap))?;
    Ok((ctl, join))
}

/// worker 主循环: 阻塞在命令通道上, Call 分支跑完整呼叫状态机
/// (run_call), 任何结果 (挂断/出错) 都回 Idle 并上报终态文案.
///
///
///
/// # Arguments
///
/// * `cmd_rx`: UI → worker 命令通道接收端
/// * `handle`: UiEvent 回投句柄
/// * `inner`: 对外可见的状态机阶段 (控制器共享)
/// * `stash`: 本地 blob 缓存 (控制器共享, 供手动导出)
/// * `sources`: 本机媒体源配置
/// * `tap`: 收端视频帧回调
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
fn run_loop(
    cmd_rx: Receiver<IceCmd>,
    handle: UiHandle,
    inner: Arc<Mutex<IceInner>>,
    stash: Arc<Mutex<Option<String>>>,
    sources: IceSources,
    tap: VideoFrameSink,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(err = %e, "ice worker: build tokio runtime failed");
            post_status(&handle, &format!("错误: 启动 ICE 线程失败: {e}"), 0, 0);
            return;
        }
    };
    post_status(&handle, "空闲", 0, 0);
    loop {
        match cmd_rx.recv() {
            Ok(IceCmd::Call) => {
                if *inner.lock() != IceInner::Idle {
                    warn!("ice call ignored: busy");
                    continue;
                }
                match run_call(&rt, &cmd_rx, &handle, &inner, &stash, &sources, &tap) {
                    Ok(()) => post_status(&handle, "空闲", 0, 0),
                    Err(e) => {
                        // 错误文案留在状态行 (不被随后的空闲覆盖), 下通电话刷新
                        error!(err = %e, "ice call failed");
                        post_status(&handle, &format!("错误: {e}"), 0, 0);
                    }
                }
                *inner.lock() = IceInner::Idle;
                stash.lock().take();
            }
            Ok(IceCmd::Hangup) => {} // 空闲时挂断无操作
            Ok(IceCmd::ImportBlob(_, reply)) => {
                let _ = reply.send(Err("当前没有进行中的呼叫".into()));
            }
            Err(_) => return, // 控制器全灭 (进程收尾), 线程退出
        }
    }
}

/// 一次呼叫的完整状态机: 读设置 → 建 agent (自动模式先 join 拿槽位) →
/// call_flow 主体 → 收尾 (先停媒体, 再关 agent — 顺序纪律见模块注释).
///
///
///
/// # Arguments
///
/// * `rt`: worker 线程的 current-thread runtime (分段 block_on 用)
/// * `cmd_rx`: UI 命令通道 (段间吸收挂断 / 手动模式等待导入)
/// * `handle`: UiEvent 回投句柄
/// * `inner`: 对外可见的状态机阶段
/// * `stash`: 本地 blob 缓存
/// * `sources`: 本机媒体源配置
/// * `tap`: 收端视频帧回调
///
/// returns: Result<(), IceCtlError> — Ok = 正常挂断; Err = 某步失败
/// (已上报错误文案, 收尾仍由本函数完成)
///
/// # Examples
///
/// ```
///
/// ```
fn run_call(
    rt: &Runtime,
    cmd_rx: &Receiver<IceCmd>,
    handle: &UiHandle,
    inner: &Arc<Mutex<IceInner>>,
    stash: &Arc<Mutex<Option<String>>>,
    sources: &IceSources,
    tap: &VideoFrameSink,
) -> Result<(), IceCtlError> {
    let settings = read_settings()?;
    *inner.lock() = IceInner::Gathering;
    post_status(handle, "收集中", 0, 0);
    let (agent, sig) = if settings.manual {
        info!("ice call: manual blob channel mode");
        let agent = IceAgent::new(IceConfig {
            stun: settings.stun,
            controlling: true,
        })?;
        (agent, None)
    } else {
        // join 先于 agent: controlling 由槽位决定 (slot 0 = offerer).
        let (ws, slot) = rt.block_on(ws_connect_join(&settings.sigserver, &settings.room))?;
        info!(slot, room = %settings.room, "ice call: joined sigserver room");
        let agent = IceAgent::new(IceConfig {
            stun: settings.stun,
            controlling: is_offerer(slot),
        })?;
        (agent, Some((ws, slot)))
    };
    let mut media = None;
    let outcome = call_flow(
        rt, cmd_rx, handle, inner, stash, sources, tap, &agent, sig, &mut media,
    );
    // 收尾顺序纪律: 先停媒体 (pipeline Null, nicesink/nicesrc 停用),
    // 再关 agent (glib 线程退出 + NiceAgent finalize).
    if let Some(m) = media.as_ref() {
        m.stop();
    }
    drop(media);
    agent.close();
    outcome
}

/// 呼叫主体 (agent 存活期内): 本地候选 → 对端 blob (WS 交换或手动导入)
/// → 连通性检查 → 起媒体 → 通话期统计上报. 出错返回 Err, 媒体句柄经
/// `media` 传出给 run_call 收尾 (本函数自身不停媒体/不关 agent).
///
///
///
/// # Arguments
///
/// * `rt`: worker 线程的 runtime
/// * `cmd_rx`: UI 命令通道
/// * `handle`: UiEvent 回投句柄
/// * `inner`: 对外可见的状态机阶段
/// * `stash`: 本地 blob 缓存
/// * `sources`: 本机媒体源配置
/// * `tap`: 收端视频帧回调
/// * `agent`: 已建好的 ICE agent (controlling 已按角色定好)
/// * `sig`: 自动模式的 (WS 连接, 槽位); 手动模式 None
/// * `media`: 媒体句柄出参 (起媒体后由 run_call 负责停)
///
/// returns: Result<(), IceCtlError> — Ok = 用户挂断; Err = 失败/掉线
///
/// # Examples
///
/// ```
///
/// ```
// reason: 呼叫主体需要 worker 全套上下文 (runtime/命令通道/回投/共享状态/
// 媒体配置/agent/信令连接/媒体出参), 拆分只会把状态机撕成碎片; 状态机
// 本来就是一个内聚流程.
#[allow(clippy::too_many_arguments)]
fn call_flow(
    rt: &Runtime,
    cmd_rx: &Receiver<IceCmd>,
    handle: &UiHandle,
    inner: &Arc<Mutex<IceInner>>,
    stash: &Arc<Mutex<Option<String>>>,
    sources: &IceSources,
    tap: &VideoFrameSink,
    agent: &IceAgent,
    sig: Option<(WsClient, u8)>,
    media: &mut Option<IceMediaHandle>,
) -> Result<(), IceCtlError> {
    // 1. 本地候选: gather 完缓存 blob 文本供手动导出
    let local = rt.block_on(agent.local_blob())?;
    stash.lock().clone_from(&Some(local.as_str().to_string()));
    info!(bytes = local.as_str().len(), "ice local blob gathered");
    if drain_hangup(cmd_rx) {
        return Ok(());
    }

    // 2. 对端 blob: 自动 = WS 角色交换 (连接在本函数结束时关闭);
    //    手动 = 停在导入等待 (同一命令通道)
    *inner.lock() = IceInner::Exchanging;
    let remote = match sig {
        Some((ws, slot)) => rt.block_on(ws_exchange(ws, slot, &local))?,
        None => match wait_import(cmd_rx)? {
            Some(blob) => blob,
            None => return Ok(()), // 手动模式用户挂断
        },
    };
    info!(bytes = remote.as_str().len(), "ice remote blob received");
    if drain_hangup(cmd_rx) {
        return Ok(());
    }

    // 3. 喂入对端 blob, 等连通性检查 (两侧都喂入后才真正推进)
    rt.block_on(agent.add_remote(&remote))?;
    *inner.lock() = IceInner::Connecting;
    post_status(handle, "连接中", 0, 0);
    rt.block_on(wait_connected(agent))?;
    if drain_hangup(cmd_rx) {
        return Ok(());
    }

    // 4. 起媒体: 收端视频走 GUI tap; 音频接收 linux 走 ALSA default
    //    放音 (与 SIP 收端 autoaudiosink 落地等价), 无 ALSA 平台仅
    //    TapOnly (无 audio tap 时即不收音频).
    #[cfg(target_os = "linux")]
    let audio_recv = AudioRecv::Alsa("default".into());
    #[cfg(not(target_os = "linux"))]
    let audio_recv = AudioRecv::TapOnly;
    let m = start_ice_media(&IceMediaConfig {
        agent,
        video_src: sources.video_src.clone(),
        audio_send: sources.audio_send,
        audio_recv,
        max_width: sources.max_width,
        taps: RawTaps {
            video: Some(tap.clone()),
            audio: None,
        },
    })?;
    *media = Some(m);
    *inner.lock() = IceInner::Active;
    post_status(handle, "已连接", 0, 0);
    info!("ice media started");

    // 5. 通话期: 1s 上报收发字节; 挂断 / 掉线 / 通道断开退出
    let state_rx = agent.state_rx();
    loop {
        match cmd_rx.recv_timeout(STATS_TICK) {
            Ok(IceCmd::Hangup) => return Ok(()),
            Ok(IceCmd::Call) => continue, // 占线中, 重复呼叫忽略
            Ok(IceCmd::ImportBlob(_, reply)) => {
                let _ = reply.send(Err("通话中: 手动导入仅在呼叫建立前可用".into()));
            }
            Err(RecvTimeoutError::Timeout) => match *state_rx.borrow() {
                IceState::Connected => {
                    let (tx_b, rx_b) = media.as_ref().map_or((0, 0), IceMediaHandle::bytes);
                    post_status(handle, "已连接", tx_b / 1024, rx_b / 1024);
                }
                IceState::Failed => return Err(IceCtlError::IceFailed),
                other => return Err(IceCtlError::Sig(format!("ICE 连接中断: {other:?}"))),
            },
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// 读取本次呼叫的 ICE 设置 (worker 线程自建 Store, 同 weather.rs 惯例;
/// 设置页写 kv 后下次呼叫生效).
///
///
///
/// returns: Result<IceSettings, IceCtlError> — 库打不开 / 读 kv 失败时 Err
///
/// # Examples
///
/// ```
///
/// ```
fn read_settings() -> Result<IceSettings, IceCtlError> {
    let store = Store::open().map_err(|e| IceCtlError::Kv(e.to_string()))?;
    let kv_or = |key: &str, default: &str| {
        store
            .kv_get(key)
            .ok()
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_string())
    };
    let sigserver = kv_or(KV_SIGSERVER, DEFAULT_SIGSERVER);
    let room = kv_or(KV_ROOM, DEFAULT_ROOM);
    // STUN: kv 缺失用默认; 显式清空 (空串) = 仅 host 候选 (本机环回调试).
    let stun = match store.kv_get(KV_STUN).ok().flatten() {
        None => Some(DEFAULT_STUN.into()),
        Some(v) => {
            let v = v.trim().to_string();
            (!v.is_empty()).then_some(v)
        }
    };
    let manual = store.kv_get(KV_MANUAL).ok().flatten().as_deref() == Some("1");
    Ok(IceSettings {
        sigserver,
        room,
        stun,
        manual,
    })
}

/// 吸收一个异步段之间积压的命令; true = 用户已点挂断 (调用方立即收尾).
/// 段内到达的导入请求以 Err 回执 (未到导入阶段).
fn drain_hangup(cmd_rx: &Receiver<IceCmd>) -> bool {
    let mut hangup = false;
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            IceCmd::Hangup => hangup = true,
            IceCmd::Call => {}
            IceCmd::ImportBlob(_, reply) => {
                let _ = reply.send(Err("呼叫尚在建立中, 未到导入阶段".into()));
            }
        }
    }
    hangup
}

/// 手动模式的导入等待: 阻塞在命令通道上直至 ImportBlob (回执已接受)
/// 或 Hangup; 1s tick 只为 recv 可超时, 无定时任务.
///
///
///
/// # Arguments
///
/// * `cmd_rx`: UI 命令通道
///
/// returns: Result<Option<IceBlob>, IceCtlError> — Some = 对端 blob;
/// None = 用户挂断; Err = 通道断开
///
/// # Examples
///
/// ```
///
/// ```
fn wait_import(cmd_rx: &Receiver<IceCmd>) -> Result<Option<IceBlob>, IceCtlError> {
    loop {
        match cmd_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(IceCmd::ImportBlob(text, reply)) => {
                // Ok 只表示"已被呼叫流程接受"; 解析/连通结果走状态上报
                // (add_remote 在 call_flow 里统一喂入).
                let _ = reply.send(Ok(()));
                return Ok(Some(IceBlob::from_sdp(text)));
            }
            Ok(IceCmd::Hangup) => return Ok(None),
            Ok(IceCmd::Call) => continue, // 占线中, 重复呼叫忽略
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(IceCtlError::Sig("命令通道断开".into()));
            }
        }
    }
}

/// ICE 状态上报快捷方式 (UiEvent::IceStatus 漏斗).
///
///
///
/// # Arguments
///
/// * `handle`: UiEvent 回投句柄
/// * `state`: 状态行文案 ("空闲"/"收集中"/"连接中"/"已连接"/"错误: …")
/// * `tx_kb`: 已发送 KiB (非通话期 0)
/// * `rx_kb`: 已接收 KiB (非通话期 0)
///
/// returns: ()
///
/// # Examples
///
/// ```
///
/// ```
fn post_status(handle: &UiHandle, state: &str, tx_kb: u64, rx_kb: u64) {
    handle.post(UiEvent::IceStatus {
        state: state.into(),
        tx_kb,
        rx_kb,
    });
}

/// sigserver 槽位角色判定: slot 0 = offerer (controlling).
fn is_offerer(slot: u8) -> bool {
    slot == 0
}

/// 本端作为 WS 客户端的连接类型 (sigserver 明文 ws, MaybeTls 兼容 wss).
type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 连接 sigserver 并 join 房间, 阻塞到收到 joined 帧 (拿槽位).
///
///
///
/// # Arguments
///
/// * `sigserver`: sigserver WS 地址 (ws://…)
/// * `room`: 房间名
///
/// returns: Result<(WsClient, u8), IceCtlError> — WS 连接与槽位
/// (0=offerer, 1=answerer)
///
/// # Examples
///
/// ```
///
/// ```
async fn ws_connect_join(sigserver: &str, room: &str) -> Result<(WsClient, u8), IceCtlError> {
    let (mut ws, _) = tokio::time::timeout(
        WS_CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async(sigserver),
    )
    .await
    .map_err(|_| IceCtlError::Ws(format!("连接 {sigserver} 超时")))?
    .map_err(|e| IceCtlError::Ws(e.to_string()))?;
    send_frame(
        &mut ws,
        Frame {
            kind: "join".into(),
            room: room.to_string(),
            ..Default::default()
        },
    )
    .await?;
    let frame = recv_frame(&mut ws, "等待加入房间确认", WS_FRAME_TIMEOUT).await?;
    match frame.kind.as_str() {
        "joined" => {
            let slot = frame
                .slot
                .filter(|s| *s == 0 || *s == 1)
                .ok_or_else(|| IceCtlError::Sig("joined 帧缺槽位".into()))?;
            Ok((ws, slot as u8))
        }
        "error" => Err(IceCtlError::Sig(format!(
            "信令服务器拒绝: {}",
            frame.detail
        ))),
        other => Err(IceCtlError::Sig(format!(
            "等待 joined 时收到意外帧: {other}"
        ))),
    }
}

/// 角色化交换对端 blob: offerer (slot 0) 发本地 blob 再等 answer;
/// answerer 等 offer 后回本地 blob. 返回对端 blob; WS 连接随本函数
/// 结束关闭 (通话期对端 hangup 转发不读, 见模块注释).
///
///
///
/// # Arguments
///
/// * `ws`: 已 join 的 WS 连接 (消费, 函数结束即关)
/// * `slot`: 本端槽位 (0=offerer)
/// * `local`: 本端 gather 完成的 blob
///
/// returns: Result<IceBlob, IceCtlError> — 对端 blob
///
/// # Examples
///
/// ```
///
/// ```
async fn ws_exchange(ws: WsClient, slot: u8, local: &IceBlob) -> Result<IceBlob, IceCtlError> {
    let mut ws = ws;
    if is_offerer(slot) {
        send_frame(
            &mut ws,
            Frame {
                kind: "offer".into(),
                payload: local.as_str().to_string(),
                ..Default::default()
            },
        )
        .await?;
        loop {
            // 对端晚加入时 offer 已被服务器丢弃, 只能靠超时解脱.
            let frame = recv_frame(
                &mut ws,
                "等待对端应答 (对端可能尚未加入房间)",
                ANSWER_WAIT_TIMEOUT,
            )
            .await?;
            match frame.kind.as_str() {
                "answer" => return Ok(IceBlob::from_sdp(frame.payload)),
                "hangup" => return Err(IceCtlError::Sig("对端已挂断".into())),
                "error" => return Err(IceCtlError::Sig(format!("信令错误: {}", frame.detail))),
                _ => continue,
            }
        }
    }
    let remote = loop {
        let frame = recv_frame(&mut ws, "等待对端呼叫", WS_FRAME_TIMEOUT).await?;
        match frame.kind.as_str() {
            "offer" => break frame.payload,
            "hangup" => return Err(IceCtlError::Sig("对端已挂断".into())),
            "error" => return Err(IceCtlError::Sig(format!("信令错误: {}", frame.detail))),
            _ => continue,
        }
    };
    send_frame(
        &mut ws,
        Frame {
            kind: "answer".into(),
            payload: local.as_str().to_string(),
            ..Default::default()
        },
    )
    .await?;
    Ok(IceBlob::from_sdp(remote))
}

/// 发送一帧 JSON 文本.
///
///
///
/// # Arguments
///
/// * `ws`: 目标连接
/// * `frame`: 信令帧
///
/// returns: Result<(), IceCtlError> — 序列化/发送失败时 Err
///
/// # Examples
///
/// ```
///
/// ```
async fn send_frame(ws: &mut WsClient, frame: Frame) -> Result<(), IceCtlError> {
    let text = serde_json::to_string(&frame).map_err(|e| IceCtlError::Sig(e.to_string()))?;
    ws.send(Message::text(text))
        .await
        .map_err(|e| IceCtlError::Ws(e.to_string()))
}

/// 收一帧文本 JSON (跳过 ping/pong/binary 等非文本帧), 带超时.
///
///
///
/// # Arguments
///
/// * `ws`: 源连接
/// * `what`: 超时信息里的等待目的 (进错误文案)
/// * `timeout`: 等待上限
///
/// returns: Result<Frame, IceCtlError> — 超时/断连/坏 JSON 时 Err
///
/// # Examples
///
/// ```
///
/// ```
async fn recv_frame(
    ws: &mut WsClient,
    what: &str,
    timeout: Duration,
) -> Result<Frame, IceCtlError> {
    let msg = loop {
        let next = tokio::time::timeout(timeout, ws.next())
            .await
            .map_err(|_| IceCtlError::Sig(format!("{what} 超时")))?;
        match next {
            Some(Ok(m)) if m.is_text() => break m,
            Some(Ok(_)) => continue, // 协议层非文本帧, 跳过
            Some(Err(e)) => return Err(IceCtlError::Ws(e.to_string())),
            None => return Err(IceCtlError::Sig("信令连接已关闭".into())),
        }
    };
    serde_json::from_str::<Frame>(msg.to_text().map_err(|e| IceCtlError::Sig(e.to_string()))?)
        .map_err(|e| IceCtlError::Sig(format!("信令帧解析失败: {e}")))
}

/// 等 agent 到 Connected (上限 ICE_CONNECT_TIMEOUT); Failed 立即报错.
///
///
///
/// # Arguments
///
/// * `agent`: 已喂入对端 blob 的 agent
///
/// returns: Result<(), IceCtlError> — 超时/Failed 时 Err
///
/// # Examples
///
/// ```
///
/// ```
async fn wait_connected(agent: &IceAgent) -> Result<(), IceCtlError> {
    let mut rx = agent.state_rx();
    let deadline = tokio::time::Instant::now() + ICE_CONNECT_TIMEOUT;
    loop {
        match *rx.borrow() {
            IceState::Connected => return Ok(()),
            IceState::Failed => return Err(IceCtlError::IceFailed),
            _ => {}
        }
        tokio::select! {
            _ = rx.changed() => {}
            _ = tokio::time::sleep_until(deadline) => {
                return Err(IceCtlError::Sig("等待 ICE 连通超时".into()));
            }
        }
    }
}

/// 装配 ICE 设置回调: 三个文本项 + 手动开关写 kv (下次呼叫生效),
/// 呼叫/挂断/导出/导入直通控制器. 导出文本写进 ice-blob-dialog-text
/// 属性, 弹框本体在 settings/ice_dialogs.slint.
///
/// # Arguments
///
/// * `ui`: 主窗口
/// * `store`: kv 持久化
/// * `ctl`: ICE 控制器
pub(crate) fn wire_ice(ui: &MainWindow, store: &Rc<Store>, ctl: &IceController) {
    /// 设置保存成功的 toast.
    fn toast_saved(ui: &MainWindow) {
        ui.invoke_notify(
            NoticeLevel::Info as i32,
            NoticeKind::Raw as i32,
            "已保存, 下次呼叫生效".into(),
        );
    }

    // sigserver 地址
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_ice_sigserver(move |v| {
            if let Err(e) = store.kv_set(KV_SIGSERVER, v.trim()) {
                error!(err = %e, "save ice sigserver failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 房间名
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_ice_room(move |v| {
            if let Err(e) = store.kv_set(KV_ROOM, v.trim()) {
                error!(err = %e, "save ice room failed");
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
        ui.on_set_ice_stun(move |v| {
            if let Err(e) = store.kv_set(KV_STUN, v.trim()) {
                error!(err = %e, "save ice stun failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 手动 blob 通道开关
    {
        let store = store.clone();
        let weak = ui.as_weak();
        ui.on_set_ice_manual(move |v| {
            if let Err(e) = store.kv_set(KV_MANUAL, if v { "1" } else { "0" }) {
                error!(err = %e, "save ice manual flag failed");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                toast_saved(&ui);
            }
        });
    }
    // 呼叫 / 挂断
    {
        let ctl = ctl.clone();
        ui.on_ice_call(move || ctl.call());
    }
    {
        let ctl = ctl.clone();
        ui.on_ice_hangup(move || ctl.hangup());
    }
    // 导出本地 blob: 文本进弹框属性 (弹框本体 Task 7)
    {
        let ctl = ctl.clone();
        let weak = ui.as_weak();
        ui.on_ice_export_blob(move || {
            if let Some(ui) = weak.upgrade() {
                match ctl.local_blob_text() {
                    Ok(text) => ui.set_ice_blob_dialog_text(text.as_str().into()),
                    Err(e) => {
                        ui.invoke_notify(NoticeLevel::Warn as i32, NoticeKind::Raw as i32, e.into())
                    }
                }
            }
        });
    }
    // 导入对端 blob (手动通道)
    {
        let ctl = ctl.clone();
        let weak = ui.as_weak();
        ui.on_ice_import_apply(move |text| {
            if let Err(e) = ctl.apply_remote_blob(text.as_str()) {
                error!(err = %e, "ice import blob rejected");
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_notify(NoticeLevel::Warn as i32, NoticeKind::Raw as i32, e.into());
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sigserver_frame() {
        let f: Frame = serde_json::from_str(r#"{"type":"offer","payload":"v=0\r\n"}"#).unwrap();
        assert_eq!(f.kind, "offer");
        assert_eq!(f.payload, "v=0\r\n");
        assert_eq!(f.slot, None);
    }

    #[test]
    fn parses_joined_frame_slot() {
        let f: Frame = serde_json::from_str(r#"{"type":"joined","slot":1}"#).unwrap();
        assert_eq!(f.slot, Some(1));
        assert!(!is_offerer(f.slot.unwrap() as u8));
    }

    #[test]
    fn decides_offerer_by_slot() {
        assert!(is_offerer(0) && !is_offerer(1));
    }

    #[test]
    fn manual_export_rejects_empty_stash() {
        // 无呼叫时导出给可读错误, 不 panic
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<IceCmd>();
        let ctl = IceController {
            inner: Arc::new(Mutex::new(IceInner::Idle)),
            stash: Arc::new(Mutex::new(None)),
            cmd_tx,
        };
        assert!(ctl.local_blob_text().is_err());
        // 空文本导入直接拒, 不碰通道
        assert!(ctl.apply_remote_blob("   ").is_err());
    }
}
