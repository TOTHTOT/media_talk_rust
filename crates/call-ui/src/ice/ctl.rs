//! ICE 呼叫 worker: 控制器句柄 + 状态机主循环. 线程模型与段间挂断吸收见 crate::ice 模块文档.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use ipcam_gst::{
    AudioRecv, IceMediaConfig, IceMediaHandle, RawTaps, VideoFrameSink, start_ice_media,
};
use ipcam_ice::signal::{self, WsClient};
use ipcam_ice::{IceAgent, IceConfig, IceState};
use parking_lot::Mutex;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tracing::{error, info, warn};

use crate::app::UiEvent;
use crate::bridge::UiHandle;
use crate::ice::{
    DEFAULT_ROOM, DEFAULT_SIGSERVER, DEFAULT_STUN, IceCmd, IceCtlError, IceInner, IceSettings,
    IceSources, KV_ROOM, KV_SIGSERVER, KV_STUN,
};
use crate::store::Store;

/// ICE 连通性检查等待上限.
const ICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 通话期统计上报间隔.
const STATS_TICK: Duration = Duration::from_secs(1);

/// UI 侧持有的 ICE 控制器: 方法只发命令 / 读共享状态, 业务全在 worker
/// 线程 (ice-ctl). Clone 供多个回调各自捕获.
#[derive(Clone)]
pub(crate) struct IceController {
    inner: Arc<Mutex<IceInner>>,
    cmd_tx: UnboundedSender<IceCmd>,
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
}

/// 起 ICE worker 线程 (ice-ctl) 并返回控制器. 线程模型照 weather::spawn:
/// tokio unbounded 命令通道驱动, worker 自建 current-thread tokio runtime.
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
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<IceCmd>();
    let inner = Arc::new(Mutex::new(IceInner::Idle));
    let ctl = IceController {
        inner: inner.clone(),
        cmd_tx,
    };
    let join = std::thread::Builder::new()
        .name("ice-ctl".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    error!(err = %e, "ice worker: build tokio runtime failed");
                    post_status(
                        &handle,
                        &format!("error: failed to start ICE worker: {e}"),
                        0,
                        0,
                    );
                    return;
                }
            };
            // 全链路 async; 这里只保留线程入口的一次 block_on.
            rt.block_on(run_loop(cmd_rx, handle, inner, sources, tap));
        })?;
    Ok((ctl, join))
}

/// worker 主循环: 等待命令, Call 分支跑完整异步呼叫状态机
/// (`run_call`), 任何结果 (挂断/出错) 都回 Idle 并上报终态文案.
///
/// # Arguments
///
/// * `cmd_rx`: UI → worker 命令通道接收端 (tokio unbounded)
/// * `handle`: UiEvent 回投句柄
/// * `inner`: 对外可见的状态机阶段 (控制器共享)
/// * `sources`: 本机媒体源配置
/// * `tap`: 收端视频帧回调
async fn run_loop(
    mut cmd_rx: UnboundedReceiver<IceCmd>,
    handle: UiHandle,
    inner: Arc<Mutex<IceInner>>,
    sources: IceSources,
    tap: VideoFrameSink,
) {
    post_status(&handle, "空闲", 0, 0);
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            IceCmd::Call => {
                if *inner.lock() != IceInner::Idle {
                    warn!("ice call ignored: busy");
                    continue;
                }
                match run_call(&mut cmd_rx, &handle, &inner, &sources, &tap).await {
                    Ok(()) => post_status(&handle, "空闲", 0, 0),
                    Err(e) => {
                        // 错误文案留在状态行 (不被随后的空闲覆盖), 下通电话刷新
                        error!(err = %e, "ice call failed");
                        post_status(&handle, &format!("error: {e}"), 0, 0);
                    }
                }
                *inner.lock() = IceInner::Idle;
            }
            IceCmd::Hangup => {} // 空闲时挂断无操作
        }
    }
}

/// 一次呼叫的完整状态机: 读设置 → 建 agent (自动模式先 join 拿槽位) →
/// call_flow 主体 → 收尾 (先停媒体, 再关 agent — 顺序纪律见 crate::ice
/// 模块文档).
///
///
///
/// # Arguments
///
/// * `cmd_rx`: UI 命令通道
/// * `handle`: UiEvent 回投句柄
/// * `inner`: 对外可见的状态机阶段
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
async fn run_call(
    cmd_rx: &mut UnboundedReceiver<IceCmd>,
    handle: &UiHandle,
    inner: &Arc<Mutex<IceInner>>,
    sources: &IceSources,
    tap: &VideoFrameSink,
) -> Result<(), IceCtlError> {
    let settings = read_settings()?;
    *inner.lock() = IceInner::Gathering;
    post_status(handle, "收集中", 0, 0);
    // join 先于 agent: controlling 由槽位决定 (slot 0 = offerer).
    let (ws, slot) = signal::ws_connect_join(&settings.sigserver, &settings.room).await?;
    info!(slot, room = %settings.room, "ice call: joined sigserver room");
    let agent = IceAgent::new(IceConfig {
        stun: settings.stun,
        controlling: signal::is_offerer(slot),
        components: 2,
    })?;
    let mut media = None;
    let outcome = call_flow(
        cmd_rx,
        handle,
        inner,
        sources,
        tap,
        &agent,
        (ws, slot),
        &mut media,
    )
    .await;
    // 收尾顺序纪律: 先停媒体 (pipeline Null, nicesink/nicesrc 停用),
    // 再关 agent (glib 线程退出 + NiceAgent finalize).
    if let Some(m) = media.as_ref() {
        m.stop();
    }
    drop(media);
    agent.close();
    outcome
}

/// 呼叫主体 (agent 存活期内): 本地候选 → 对端 blob (WS 交换)
/// → 连通性检查 → 起媒体 → 通话期统计上报. 出错返回 Err, 媒体句柄经
/// `media` 传出给 run_call 收尾 (本函数自身不停媒体/不关 agent).
///
///
///
/// # Arguments
///
/// * `cmd_rx`: UI 命令通道
/// * `handle`: UiEvent 回投句柄
/// * `inner`: 对外可见的状态机阶段
/// * `sources`: 本机媒体源配置
/// * `tap`: 收端视频帧回调
/// * `agent`: 已建好的 ICE agent (controlling 已按角色定好)
/// * `sig`: (WS 连接, 槽位)
/// * `media`: 媒体句柄出参 (起媒体后由 run_call 负责停)
///
/// returns: Result<(), IceCtlError> — Ok = 用户挂断; Err = 失败/掉线
///
/// # Examples
///
/// ```
///
/// ```
// reason: 呼叫主体需要 worker 全套上下文 (命令通道/回投/共享状态/
// 媒体配置/agent/信令连接/媒体出参), 拆分只会把状态机撕成碎片; 状态机
// 本来就是一个内聚流程.
#[allow(clippy::too_many_arguments)]
async fn call_flow(
    cmd_rx: &mut UnboundedReceiver<IceCmd>,
    handle: &UiHandle,
    inner: &Arc<Mutex<IceInner>>,
    sources: &IceSources,
    tap: &VideoFrameSink,
    agent: &IceAgent,
    sig: (WsClient, u8),
    media: &mut Option<IceMediaHandle>,
) -> Result<(), IceCtlError> {
    // 1. 本地候选: gather 完得到 blob
    let local = agent.local_blob().await?;
    info!("ice local blob gathered, local = {local:#?}");
    if drain_hangup(cmd_rx) {
        return Ok(());
    }

    // 2. 对端 blob: WS 角色交换 (连接在本函数结束时关闭)
    *inner.lock() = IceInner::Exchanging;
    let (ws, slot) = sig;
    let remote = signal::ws_exchange(ws, slot, &local, signal::OFFER, signal::ANSWER).await?;
    info!(remote_sdp = ?remote, "ice remote blob received");
    if drain_hangup(cmd_rx) {
        return Ok(());
    }

    // 3. 喂入对端 blob, 等连通性检查 (两侧都喂入后才真正推进)
    agent.add_remote(&remote).await?;
    *inner.lock() = IceInner::Connecting;
    post_status(handle, "连接中", 0, 0);
    agent.wait_connected(ICE_CONNECT_TIMEOUT).await?;
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
        match tokio::time::timeout(STATS_TICK, cmd_rx.recv()).await {
            Ok(Some(IceCmd::Hangup)) => return Ok(()),
            Ok(Some(IceCmd::Call)) => continue, // 占线中, 重复呼叫忽略
            Ok(None) => return Ok(()),
            Err(_) => match *state_rx.borrow() {
                IceState::Connected => {
                    let (tx_b, rx_b) = media.as_ref().map_or((0, 0), IceMediaHandle::bytes);
                    post_status(handle, "已连接", tx_b / 1024, rx_b / 1024);
                }
                IceState::Failed => return Err(IceCtlError::IceFailed),
                other => {
                    return Err(IceCtlError::Sig(format!(
                        "ICE connection interrupted: {other:?}"
                    )));
                }
            },
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
    Ok(IceSettings {
        sigserver,
        room,
        stun,
    })
}

/// 吸收一个异步段之间积压的命令; true = 用户已点挂断 (调用方立即收尾).
fn drain_hangup(cmd_rx: &mut UnboundedReceiver<IceCmd>) -> bool {
    let mut hangup = false;
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            IceCmd::Hangup => hangup = true,
            IceCmd::Call => {}
        }
    }
    hangup
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
