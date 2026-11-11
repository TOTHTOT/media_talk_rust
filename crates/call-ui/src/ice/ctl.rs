//! ICE 呼叫 worker: 控制器句柄 + 状态机主循环. 线程模型与段间挂断吸收见 crate::ice 模块文档.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use ipcam_gst::{
    AudioRecv, IceMediaConfig, IceMediaHandle, RawTaps, VideoFrameSink, start_ice_media,
};
use ipcam_ice::blob::IceBlob;
use ipcam_ice::{IceAgent, IceConfig, IceState};
use parking_lot::Mutex;
use tokio::runtime::Runtime;
use tracing::{error, info, warn};

use crate::app::UiEvent;
use crate::bridge::UiHandle;
use crate::ice::sig::{WsClient, is_offerer, ws_connect_join, ws_exchange};
use crate::ice::{
    DEFAULT_ROOM, DEFAULT_SIGSERVER, DEFAULT_STUN, IceCmd, IceCtlError, IceInner, IceSettings,
    IceSources, KV_MANUAL, KV_ROOM, KV_SIGSERVER, KV_STUN,
};
use crate::store::Store;

/// ICE 连通性检查等待上限.
const ICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 通话期统计上报间隔.
const STATS_TICK: Duration = Duration::from_secs(1);
/// apply_remote_blob 等回执的 UI 侧上限 (正常路径 worker 停在导入等待, 立即回执).
const IMPORT_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

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
/// call_flow 主体 → 收尾 (先停媒体, 再关 agent — 顺序纪律见 crate::ice
/// 模块文档).
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

#[cfg(test)]
mod tests {
    use super::*;

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
