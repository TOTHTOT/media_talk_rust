//! libnice NiceAgent 的安全包装.
//!
//! 线程纪律 (spec 风险#1 的编码规则): NiceAgent 绑定自建 GMainContext,
//! 独立线程跑 MainLoop; 一切 `nice_*` 调用经 [`IceAgent::invoke`] marshal
//! 回该线程执行; GObject 信号回调同样在该线程触发, trampoline 内只做
//! 无锁 channel 发送, 不做业务.
//!
//! 关停: [`IceAgent::close`] — 退出 MainLoop, join glib 线程, 归还两份
//! GObject 引用 (引用账见该方法的文档). 接收回调 (attach_recv) 与
//! candidate-gathering-done 的装箱数据不拆连, 存活至进程退出 (记录不修).

use std::ffi::{CString, c_uint, c_void};
use std::sync::{Mutex, mpsc as std_mpsc};
use std::time::Duration;

use glib::prelude::ObjectExt as _;
use glib::translate::ToGlibPtr as _;
use tokio::sync::mpsc;
use tokio::sync::{oneshot, watch};

use super::blob::IceBlob;
use super::sys;

/// Windows 一次性补 Winsock 初始化: 本机 libnice (0.1.23, gst 捆绑) 枚举
/// 本机地址走 getnameinfo, 但库自身不调 WSAStartup (gst 进程里通常已被
/// GSocket 顺带初始化, 单测进程没有这一步), 漏了会导致 gather 无候选
/// 立即失败. gio 的 g_networking_init 幂等完成 WSAStartup.
#[cfg(windows)]
fn ensure_winsock() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: gio 导出函数, 幂等, 无参无返回, 全程仅做 WSAStartup 一次.
    ONCE.call_once(|| unsafe { gio::ffi::g_networking_init() });
}

#[cfg(not(windows))]
fn ensure_winsock() {}

/// 解析 STUN URI: `stun://host:port` / `stun:host:port` / `host:port` /
/// `host` (默认 3478). libnice 的 `stun-server` 属性只收主机名/IP,
/// 端口必须单独写 `stun-port`, 不能把整个 URI 塞进去.
fn parse_stun_uri(uri: &str) -> Result<(String, u16), IceError> {
    let raw = uri.trim().trim_end_matches('/');
    if raw.is_empty() {
        return Err(IceError::InvalidStun(uri.to_string()));
    }
    if raw.contains("://") && !raw.starts_with("stun://") {
        return Err(IceError::InvalidStun(uri.to_string()));
    }
    let s = raw
        .strip_prefix("stun://")
        .or_else(|| raw.strip_prefix("stun:"))
        .unwrap_or(raw);
    if s.is_empty() {
        return Err(IceError::InvalidStun(uri.to_string()));
    }

    // IPv6 字面量必须带方括号: [::1]:3478 / [::1]
    if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| IceError::InvalidStun(uri.to_string()))?;
        if host.is_empty() {
            return Err(IceError::InvalidStun(uri.to_string()));
        }
        let port = match after.strip_prefix(':') {
            Some(port) if !port.is_empty() => parse_port(port, uri)?,
            Some(_) | None => 3478,
        };
        return Ok((host.to_string(), port));
    }

    // 裸 IPv6 (含多个冒号) 没有端口信息, 整串当 host, 用默认端口.
    if s.matches(':').count() > 1 {
        return Ok((s.to_string(), 3478));
    }
    match s.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !port.is_empty() => {
            Ok((host.to_string(), parse_port(port, uri)?))
        }
        Some(_) => Err(IceError::InvalidStun(uri.to_string())),
        None => Ok((s.to_string(), 3478)),
    }
}

fn parse_port(port: &str, uri: &str) -> Result<u16, IceError> {
    match port.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(IceError::InvalidStun(uri.to_string())),
    }
}

/// agent 配置.
pub struct IceConfig {
    /// STUN server URI (如 `stun://stun.miwifi.com:3478`), None = 仅 host 候选.
    pub stun: Option<String>,
    /// true = controlling (通常 offer 方), false = controlled.
    pub controlling: bool,
    /// component 数: 1 = 纯数据场景; 2 = media 约定 (1=video, 2=audio).
    /// libnice 要求 >= 1, 传 0 在建 agent 时报 [`IceError::CreateStream`].
    pub components: u32,
}

/// 组件连通状态 (映射 NiceComponentState, UI 展示用).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceState {
    Gathering,
    Connecting,
    Connected,
    Failed,
}

/// 选中路径上的候选类型 (ICE 测试展示用; 映射 candidate.h 的
/// NiceCandidateType, SDP 展示用的 blob::CandKind 不含 prflx 故单列).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectedKind {
    Host,
    Srflx,
    Prflx,
    Relay,
}

impl SelectedKind {
    fn from_raw(raw: c_uint) -> Option<Self> {
        match raw {
            sys::CAND_HOST => Some(Self::Host),
            sys::CAND_SRFLX => Some(Self::Srflx),
            sys::CAND_PRFLX => Some(Self::Prflx),
            sys::CAND_RELAY => Some(Self::Relay),
            _ => None,
        }
    }
}

/// agent 操作错误 (控制路径, 见各变体注释).
#[derive(Debug, thiserror::Error)]
pub enum IceError {
    #[error("failed to create libnice agent (library missing?)")]
    NullAgent,
    #[error("add_stream returned 0")]
    CreateStream,
    #[error("gather_candidates failed")]
    GatherFailed,
    #[error("remote SDP rejected")]
    RemoteRejected,
    #[error("invalid STUN server URI: {0}")]
    InvalidStun(String),
    #[error("ICE connectivity check failed (peer unreachable)")]
    ConnectFailed,
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("agent closed")]
    Closed,
}

impl From<oneshot::error::RecvError> for IceError {
    fn from(_: oneshot::error::RecvError) -> Self {
        IceError::Closed
    }
}

/// 裸指针的 Send 搬运容器: 只负责把指针带过人线程边界, 不附带解引用语义.
struct SendPtr<T>(*mut T);

impl<T> SendPtr<T> {
    /// 取出指针. async 块内直接投影 `.0` 会把裸指针写进 future 的
    /// Send 见证, 经方法边界取出可保持 future 的 Send 推导.
    fn into_ptr(self) -> *mut T {
        self.0
    }
}

// SAFETY: 容器本身不碰指向的数据; 解引用只发生在目标线程 (glib 线程,
// 由 invoke / ready 通道纪律保证), 不存在并发触达.
unsafe impl<T> Send for SendPtr<T> {}

/// 公共 ICE agent 句柄.
///
/// 所有方法把 `nice_*` 调用 marshal 回内部 glib 线程 (见模块注释),
/// 句柄自身可跨线程自由传递. 关停走 [`IceAgent::close`]: 退出 mainloop,
/// join glib 线程, 归还全部 GObject 引用; 调用方 MUST 先停掉所有
/// nicesink/nicesrc 类使用者再 close (顺序纪律见 ice_media 模块注释).
pub struct IceAgent {
    ctx: glib::MainContext,
    /// 关停把手: glib 线程里那个 MainLoop 的克隆 (glib::MainLoop 是
    /// GMainLoop 的引用计数包装, clone 不复制循环本体).
    mainloop: glib::MainLoop,
    agent: *mut sys::NiceAgent,
    stream: u32,
    state_tx: watch::Sender<IceState>,
    _loop_thread: std::thread::JoinHandle<()>,
}

// SAFETY: agent 裸指针只在 glib 线程解引用 (invoke 纪律), 其余线程仅整体传递.
unsafe impl Send for IceAgent {}
// SAFETY: 共享访问只触及只读元数据与内部 channel/watch 端点; agent 指针仍
// 只经 invoke 在 glib 线程触达, 不存在跨线程数据竞争.
unsafe impl Sync for IceAgent {}

impl IceAgent {
    /// 创建 agent: 起独立 glib 线程, 按 `cfg.components` 建流 (component
    /// 编号 1..N, media 场景 1=video/2=audio, 纯数据场景传 1),
    /// 连接状态信号, 阻塞至建流完成 (上限 5s).
    ///
    /// # Arguments
    ///
    /// * `cfg`: STUN URI 与 controlling 角色, 见 [`IceConfig`].
    ///
    /// returns: Result<IceAgent, IceError> — 库缺失 / 线程启动失败 / 建流失败时 Err
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn new(cfg: IceConfig) -> Result<Self, IceError> {
        ensure_winsock();
        // libnice 要求 component 数 >= 1; 提前拒, 不起线程.
        if cfg.components == 0 {
            return Err(IceError::CreateStream);
        }
        // MainContext::new 不绑定线程: 这里 (调用方线程) 创建, glib 线程使用.
        let ctx = glib::MainContext::new();
        let (ready_tx, ready_rx) =
            std_mpsc::channel::<Result<(u32, SendPtr<sys::NiceAgent>, glib::MainLoop), IceError>>();
        let (state_tx, _state_rx) = watch::channel(IceState::Gathering);

        let thread_ctx = ctx.clone();
        let loop_ctx = ctx.clone();
        let thread_state_tx = state_tx.clone();
        let thread = std::thread::Builder::new()
            .name("ice-glib".into())
            .spawn(move || {
                let ready_inner = ready_tx.clone();
                let run = move || {
                    let mainloop = glib::MainLoop::new(Some(&thread_ctx), false);
                    // SAFETY: agent 在本 ctx 线程创建并随线程存活; 后续一切
                    // nice_* 调用经 invoke 回到本线程, 指针不跨线程解引用.
                    let agent = unsafe {
                        sys::nice_agent_new(thread_ctx.to_glib_none().0, sys::COMPAT_RFC5245)
                    };
                    if agent.is_null() {
                        let _ = ready_inner.send(Err(IceError::NullAgent));
                        return;
                    }
                    // SAFETY: NiceAgent 是 GObject (G_TYPE_OBJECT) 子类实例,
                    // 非浮态; glib-rs 的 from_glib_none 对非浮态对象等价
                    // g_object_ref_sink, 即取得一份真实强引用 — 引用计数 =
                    // nice_agent_new 初始 1 + 本份 1. 两份都待 IceAgent::close
                    // 归还: 一份由 close 的 ctx.invoke 闭包在 glib 线程归还
                    // (quit 同闭包、紧随 unref 之后, 见 close 的顺序论证),
                    // 另一份由本闭包收尾时 gobj Drop 归还 — mainloop.run()
                    // 不返回 gobj 就不会析构, 不 close 则两份引用都挂到进程
                    // 退出 (历史泄漏行为, 引用账见 close 的文档).
                    let gobj: glib::Object =
                        unsafe { glib::translate::from_glib_none(agent.cast()) };
                    gobj.set_property("controlling-mode", cfg.controlling);
                    if let Some(stun) = cfg.stun {
                        // libnice 的 stun-server 属性只接受主机名/IP, 端口单独
                        // 走 stun-port; 这里兼容 stun://host:port 与 host:port.
                        let (server, port) = match parse_stun_uri(&stun) {
                            Ok(parsed) => parsed,
                            Err(e) => {
                                let _ = ready_inner.send(Err(e));
                                return;
                            }
                        };
                        gobj.set_property("stun-server", server);
                        gobj.set_property("stun-server-port", port as u32);
                    }
                    let state_cb = thread_state_tx.clone();
                    // SAFETY: agent 存活; 信号回调只发 watch (无锁单写),
                    // 符合线程纪律 (业务不进了 glib 线程).
                    unsafe {
                        sys::connect_state_changed(agent, move |_stream, _component, raw| {
                            let mapped = match raw {
                                sys::STATE_GATHERING => Some(IceState::Gathering),
                                sys::STATE_CONNECTING => Some(IceState::Connecting),
                                sys::STATE_CONNECTED | sys::STATE_READY => {
                                    Some(IceState::Connected)
                                }
                                sys::STATE_FAILED => Some(IceState::Failed),
                                _ => None,
                            };
                            if let Some(s) = mapped {
                                let _ = state_cb.send_replace(s);
                            }
                        });
                    }
                    // SAFETY: add_stream 在 ctx 线程内调用, agent 存活.
                    let stream = unsafe { sys::nice_agent_add_stream(agent, cfg.components) };
                    if stream == 0 {
                        let _ = ready_inner.send(Err(IceError::CreateStream));
                        return;
                    }
                    // 关停把手随就绪信号传出; 线程自持本体跑到 close() quit.
                    let _ = ready_inner.send(Ok((stream, SendPtr(agent), mainloop.clone())));
                    // 驱动 ctx 上排队的 invoke / 信号回调, 至 close() quit.
                    mainloop.run();
                };
                // 新 ctx 不可能已被其他线程占用; 失败仅理论路径, 契约枚举无更
                // 贴切变体, 借用 NullAgent 上报 (GLIB 线程根本没起来).
                if loop_ctx.with_thread_default(run).is_err() {
                    let _ = ready_tx.send(Err(IceError::NullAgent));
                }
            })
            .map_err(|_| IceError::NullAgent)?;

        let (stream, agent, mainloop) =
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|e| match e {
                    std_mpsc::RecvTimeoutError::Timeout => IceError::Timeout("agent init"),
                    // glib 线程在就绪前死亡 (panic / 理论上的 acquire 失败).
                    std_mpsc::RecvTimeoutError::Disconnected => IceError::NullAgent,
                })??;
        Ok(Self {
            ctx,
            mainloop,
            agent: agent.0,
            stream,
            state_tx,
            _loop_thread: thread,
        })
    }

    /// 关停 agent: 退出 glib 线程的 MainLoop, join 线程, 归还两份 GObject
    /// 引用, NiceAgent 随之 finalize. 全程在 glib 线程完成 (GObject 线程
    /// 亲和约定).
    ///
    /// 引用账 (与 new() 里的 from_glib_none SAFETY 注释一致):
    /// 1. 第一次 unref 归还 `nice_agent_new` 的初始引用 — 经 `ctx.invoke`
    ///    排入 glib 线程, 在闭包内 unref 后立即 `quit` (见下方顺序论证);
    /// 2. glib 线程收尾时 `run` 闭包内的 gobj (from_glib_none 取得的那份)
    ///    Drop 再 unref 一次 — join 保证它先于本函数返回完成.
    ///
    /// 顺序论证 (review fix): `g_main_context_invoke` 只是往 ctx 挂 idle
    /// 源, 而 `g_main_loop_quit` 仅翻转 is_running, 由 run() 在迭代边界
    /// 检查 — 若 quit 在闭包外调用, 闭包可能被边界检查跳过 (unref 丢失,
    /// 正是 close 要修的泄漏). quit 移进闭包后: unref 先于 quit 执行,
    /// is_running 的翻转在闭包返回后的下一个边界才生效, unref#1 不可能
    /// 被跳过.
    ///
    /// 调用方 MUST 保证: close 时没有未完成的 [`IceAgent::invoke`] 类
    /// 调用 (close 与业务调用互斥), 且所有 nicesink/nicesrc 类使用者已先
    /// 停用 (ice_media 模块的顺序纪律: 先停媒体, 再关 agent). close 不可
    /// 在 glib 线程自身调用 (invoke 对 owner 线程是同步直调, join 当前
    /// 线程会 panic). candidate-gathering-done 的信号 handler 装箱数据随
    /// 连接存活, 本版不拆连, 进程退出才回收 (与 attach_recv 的 Box 同纪律,
    /// 记录不修).
    pub fn close(self) {
        let Self {
            ctx,
            mainloop,
            agent,
            stream: _,
            state_tx: _,
            _loop_thread,
        } = self;
        let ptr = SendPtr(agent);
        // 同底层 GMainLoop 的第二把手 (Shared 引用计数包装, clone 不复制
        // 循环本体), 随闭包进 glib 线程做 quit.
        let quit_loop = mainloop.clone();
        // SAFETY: agent 指针在 IceAgent 整个存活期有效 (从未 unref); 本方法
        // 消费 self, 之后无任何使用者; 闭包由 ctx 调度在 glib 线程执行,
        // 内部先 unref#1 再 quit — unref 先于 loop 边界检查, 不可能被跳过;
        // 本次 unref 后 gobj 仍持有一份引用, 对象不会在本闭包内 finalize.
        ctx.invoke(move || {
            unsafe { glib::gobject_ffi::g_object_unref(ptr.into_ptr().cast()) };
            quit_loop.quit();
        });
        // 第二份引用 (gobj Drop) 的归还发生在线程收尾; join 确保它先于
        // 本函数返回完成. 线程 panic 不属于契约路径, 只记日志.
        if _loop_thread.join().is_err() {
            tracing::warn!("ice glib thread panicked during close");
        }
    }

    /// 收集本地候选并生成可交换的 ICE blob (至 gathering-done, 上限 10s).
    ///
    /// 先连 `candidate-gathering-done` 信号再触发 gather, 顺序由 invoke 的
    /// ctx 单线程执行保证; gather_candidates 立即失败 (返回 0) 时直接 Err.
    ///
    /// # Arguments
    ///
    /// * `&self`: 目标 agent.
    ///
    /// returns: Result<IceBlob, IceError> — GatherFailed / Timeout("gather") / Closed
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub async fn local_blob(&self) -> Result<IceBlob, IceError> {
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let done_slot = Mutex::new(Some(done_tx));
        // 信号连接与 gather 放进同一个 invoke, 顺序不再依赖跨调用的调度时序.
        let gather_rc = self
            .invoke(move |agent, stream| {
                // SAFETY: 本闭包在 agent 的 ctx 线程执行, agent 存活 (IceAgent 持有).
                unsafe {
                    sys::connect_gathering_done(agent, move || {
                        // SAFETY: 回调在 ctx 线程触发, Mutex 只护住 take-once.
                        if let Ok(mut slot) = done_slot.lock() {
                            if let Some(tx) = slot.take() {
                                let _ = tx.send(());
                            }
                        }
                    });
                    sys::nice_agent_gather_candidates(agent, stream)
                }
            })
            .await?;
        if gather_rc == 0 {
            return Err(IceError::GatherFailed);
        }
        tokio::time::timeout(Duration::from_secs(10), done_rx)
            .await
            .map_err(|_| IceError::Timeout("gather"))??;
        let sdp = self
            .invoke(|agent, _| {
                // SAFETY: ctx 线程; 返回值所有权移出, 由调用方 g_free.
                SendPtr(unsafe { sys::nice_agent_generate_local_sdp(agent) })
            })
            .await?;
        if sdp.0.is_null() {
            return Err(IceError::GatherFailed);
        }
        // SAFETY: 所有权从 C 转移, from_glib_full 会 g_free.
        Ok(IceBlob::new_unchecked(unsafe {
            glib::translate::from_glib_full(sdp.0)
        }))
    }

    /// 喂入对端 blob (credentials + candidates), 触发连通性检查.
    ///
    /// 两侧 agent 各自调用本方法交换 blob 后, ICE 打洞即开始,
    /// 进度经 [`IceAgent::state_rx`] 订阅.
    ///
    /// # Arguments
    ///
    /// * `blob`: 对端 `local_blob()` 产出的信令单元.
    ///
    /// returns: Result<(), IceError> — 解析被拒时 RemoteRejected
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub async fn add_remote(&self, blob: &IceBlob) -> Result<(), IceError> {
        // blob 含内嵌 NUL 视为损坏的信令文本.
        let sdp = CString::new(blob.as_str()).map_err(|_| IceError::RemoteRejected)?;
        let parsed = self
            .invoke(move |agent, _| {
                // SAFETY: ctx 线程; sdp 为 NUL 结尾堆串, 生命周期覆盖本次调用.
                unsafe { sys::nice_agent_parse_remote_sdp(agent, sdp.as_ptr()) }
            })
            .await?;
        if parsed < 0 {
            return Err(IceError::RemoteRejected);
        }
        Ok(())
    }

    /// 状态订阅 (watch 保留最新值, 晚订阅也能读到当前状态).
    pub fn state_rx(&self) -> watch::Receiver<IceState> {
        self.state_tx.subscribe()
    }

    /// 等待组件进入 Connected; Failed 立即返回 [`IceError::ConnectFailed`],
    /// 超时返回 [`IceError::Timeout`].
    ///
    /// # Arguments
    ///
    /// * `timeout`: 最长等待时间
    pub async fn wait_connected(&self, timeout: Duration) -> Result<(), IceError> {
        let mut rx = self.state_rx();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match *rx.borrow() {
                IceState::Connected => return Ok(()),
                IceState::Failed => return Err(IceError::ConnectFailed),
                _ => {}
            }
            tokio::select! {
                _ = rx.changed() => {}
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(IceError::Timeout("connected"));
                }
            }
        }
    }

    /// 查询 component 当前选中路径的候选类型 (本地, 远端).
    ///
    /// component 连通 ([`IceState::Connected`]) 后可用; 未连通 / 库未选中 /
    /// 类型值越界时返回 None。走 invoke 纪律, 可在任意线程调用。
    pub async fn selected_pair(&self, component: u32) -> Option<(SelectedKind, SelectedKind)> {
        let raw = self
            .invoke(move |agent, stream| {
                let mut local: *mut sys::NiceCandidate = std::ptr::null_mut();
                let mut remote: *mut sys::NiceCandidate = std::ptr::null_mut();
                // SAFETY: ctx 线程; agent 存活; 出参由库写入, 候选归 agent 所有
                // (本闭包不释放), 调用前后均判空.
                let ok = unsafe {
                    sys::nice_agent_get_selected_pair(
                        agent,
                        stream,
                        component,
                        &mut local,
                        &mut remote,
                    )
                };
                if ok == 1 && !local.is_null() && !remote.is_null() {
                    // SAFETY: 两指针均已判空且为库返回的合法候选.
                    Some((unsafe { sys::candidate_type(local) }, unsafe {
                        sys::candidate_type(remote)
                    }))
                } else {
                    None
                }
            })
            .await
            .ok()??;
        Some((
            SelectedKind::from_raw(raw.0)?,
            SelectedKind::from_raw(raw.1)?,
        ))
    }

    /// 原始 NiceAgent 指针, 仅供 ice_media 设置 nicesink/nicesrc `agent` 属性.
    ///
    /// # Safety
    /// 调用方 MUST 保证 agent 存活于所有使用者之后 (IceMediaHandle 先于 IceAgent drop).
    pub fn raw_agent(&self) -> *mut c_void {
        self.agent.cast()
    }

    /// 把闭包 marshal 到 agent 的 glib 线程执行并取回结果 (invoke 纪律的唯一入口).
    ///
    /// 用 `MainContext::spawn` (future 排入 ctx 队列, 不阻塞当前 tokio worker)
    /// 而非 `invoke` (会占用调用线程); 结果经 oneshot 回传, ctx 线程死亡时
    /// 报 Closed 而不是挂起.
    async fn invoke<T, F>(&self, f: F) -> Result<T, IceError>
    where
        T: Send + 'static,
        F: FnOnce(*mut sys::NiceAgent, u32) -> T + Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<T>();
        let agent = SendPtr(self.agent);
        let stream = self.stream;
        self.ctx.spawn(async move {
            let out = f(agent.into_ptr(), stream);
            let _ = tx.send(out);
        });
        rx.await.map_err(IceError::from)
    }

    /// 经 component 1 发原始字节 (connected 之后).
    ///
    /// 无 ICE 数据面专用错误变体, 发送失败借用 [`IceError::RemoteRejected`]
    /// 表示 (契约枚举的历史遗留, 与 cfg(test) 时期一致)。
    pub async fn send(&self, data: &[u8]) -> Result<(), IceError> {
        // invoke 要求 'static, 复制一份带进 glib 线程.
        let buf: Vec<u8> = data.to_vec();
        let len = u32::try_from(buf.len()).map_err(|_| IceError::RemoteRejected)?;
        let sent = self
            .invoke(move |agent, stream| {
                // SAFETY: ctx 线程; buf 存活至闭包返回, 长度已截到 u32.
                unsafe { sys::nice_agent_send(agent, stream, 1, len, buf.as_ptr().cast()) }
            })
            .await?;
        if sent < 0 {
            // 契约枚举无数据面变体, 借用 RemoteRejected 表示发送失败.
            return Err(IceError::RemoteRejected);
        }
        Ok(())
    }

    /// component 1 挂数据接收回调.
    ///
    /// glib trampoline 把数据 marshal 进 mpsc, 由独立线程调 `f`, 用户回调
    /// 永远不占用 glib 线程。注意 (既有纪律, 升格为 pub 后更显式): 每次调用
    /// 会把回调通道 Box 泄漏到进程退出才回收, 其 receiver std::thread 同样
    /// 泄漏 (blocking_recv 循环只随进程退出才结束) — 一次性测试工具/单测场景
    /// 无感知, 长驻进程请勿反复调用。ice_probe 与环回测试各调一次。
    pub async fn attach_recv<F>(&self, f: F) -> Result<(), IceError>
    where
        F: Fn(&[u8]) + Send + 'static,
    {
        let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);
        std::thread::spawn(move || {
            while let Some(d) = data_rx.blocking_recv() {
                f(&d);
            }
        });
        let ctx_ptr = SendPtr(self.ctx.to_glib_none().0);
        let attached = self
            .invoke(move |agent, stream| {
                // SAFETY: ctx 线程; data_tx 装箱后泄漏, 存活至进程退出
                // (pub API 同泄漏纪律, 见方法文档). ctx 传 agent 自己的 glib ctx —
                // 本版 libnice 的 attach_recv 会把该 component 的全部 socket
                // I/O (含连通性检查的 STUN 回包) 路由到传入 ctx, 传 NULL 则
                // 落到进程默认 ctx (无人迭代, 永远收不到包).
                unsafe {
                    sys::nice_agent_attach_recv(
                        agent,
                        stream,
                        1,
                        ctx_ptr.into_ptr(),
                        Some(recv_trampoline),
                        Box::into_raw(Box::new(data_tx)) as *mut c_void,
                    )
                }
            })
            .await?;
        if attached == 0 {
            // 契约枚举无数据面变体, 借用 RemoteRejected 表示挂载失败.
            return Err(IceError::RemoteRejected);
        }
        Ok(())
    }
}

unsafe extern "C" fn recv_trampoline(
    _agent: *mut sys::NiceAgent,
    _stream: std::ffi::c_uint,
    _component: std::ffi::c_uint,
    len: std::ffi::c_uint,
    buf: *mut std::ffi::c_char,
    data: *mut c_void,
) {
    // SAFETY: data 是 attach_recv 里 Box::into_raw 的 mpsc::Sender,
    // 泄漏式存活覆盖所有回调; 回调在 agent ctx 线程触发, try_send 无锁.
    let tx = unsafe { &*(data as *const mpsc::Sender<Vec<u8>>) };
    // SAFETY: libnice 保证 buf 有效且长度 len 个字节可读.
    let bytes = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len as usize) };
    let _ = tx.try_send(bytes.to_vec());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_stun_uri_forms() {
        assert_eq!(
            parse_stun_uri("stun://stun.miwifi.com:3478").unwrap(),
            ("stun.miwifi.com".into(), 3478)
        );
        assert_eq!(
            parse_stun_uri("stun.miwifi.com:3478").unwrap(),
            ("stun.miwifi.com".into(), 3478)
        );
        assert_eq!(
            parse_stun_uri("stun:stun.miwifi.com:3478/").unwrap(),
            ("stun.miwifi.com".into(), 3478)
        );
        assert_eq!(
            parse_stun_uri("stun.miwifi.com").unwrap(),
            ("stun.miwifi.com".into(), 3478)
        );
        assert_eq!(parse_stun_uri("[::1]:3478").unwrap(), ("::1".into(), 3478));
        assert!(matches!(
            parse_stun_uri("turn://example.com:3478"),
            Err(IceError::InvalidStun(_))
        ));
        assert!(matches!(
            parse_stun_uri("stun://:3478"),
            Err(IceError::InvalidStun(_))
        ));
    }

    /// 双 agent 本机环回: 无 STUN, host 候选打洞, 数据面仅断言 A→B (B 侧 attach 仅为打通 ctx 路由).
    /// libnice 缺失 (nice_agent_new 返回 null) 时静默跳过, 与 gst 缺件 guard 同惯例.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_agents_connect_loopback() {
        loopback_with_components(2).await;
    }

    /// 单 component 环回: IceConfig.components 可配的最小形态 (纯数据场景,
    /// ice_probe 同款配置).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_component_loopback() {
        loopback_with_components(1).await;
    }

    /// 环回主体: component 数走参数 (2 = media 约定 1=video/2=audio).
    async fn loopback_with_components(components: u32) {
        let Ok(a) = IceAgent::new(IceConfig {
            stun: None,
            controlling: true,
            components,
        }) else {
            return; // libnice 不可用
        };
        let Ok(b) = IceAgent::new(IceConfig {
            stun: None,
            controlling: false,
            components,
        }) else {
            return;
        };

        let (a_blob, b_blob) = tokio::join!(a.local_blob(), b.local_blob());
        let (a_blob, b_blob) = (a_blob.unwrap(), b_blob.unwrap());
        // 双侧都要喂对端 blob, join! 覆盖对称补调.
        let (ra, rb) = tokio::join!(a.add_remote(&b_blob), b.add_remote(&a_blob));
        ra.unwrap();
        rb.unwrap();

        // 这版 libnice 的 attach_recv 才把 component 的 socket I/O (含连通性
        // 检查回包) 接到被迭代的 ctx 上, 必须在等 Connected 之前挂好两侧.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let attach_b = b.attach_recv(move |data| {
            let _ = tx.try_send(data.to_vec());
        });
        let attach_a = a.attach_recv(|_| {});
        let (ra, rb) = tokio::join!(attach_a, attach_b);
        ra.unwrap();
        rb.unwrap();

        wait_connected(&a).await;
        wait_connected(&b).await;
        println!("loopback: both agents reached Connected state");

        // 数据面: A→B 经 nice_agent_send + attach_recv 验证
        a.send(b"ping").await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("recv timeout")
            .expect("channel closed");
        assert_eq!(got, b"ping");
        println!(
            "loopback: payload received: {:?}",
            String::from_utf8_lossy(&got)
        );

        // 选中路径查询: 本机环回两侧都是 host 候选 (Task 5 新增 API 的 TDD 断言)
        let pair_a = a.selected_pair(1).await;
        let pair_b = b.selected_pair(1).await;
        assert_eq!(
            pair_a,
            Some((SelectedKind::Host, SelectedKind::Host)),
            "A 侧选中路径应为 host/host"
        );
        assert_eq!(
            pair_b,
            Some((SelectedKind::Host, SelectedKind::Host)),
            "B 侧选中路径应为 host/host"
        );
        println!("loopback: selected pair a={pair_a:?} b={pair_b:?}");

        // 收尾回归: close (quit + join + 双 unref) 不得挂起/崩溃
        a.close();
        b.close();
        println!("loopback: both agents closed");
    }

    async fn wait_connected(agent: &IceAgent) {
        let mut rx = agent.state_rx();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if *rx.borrow() == IceState::Connected {
                return;
            }
            tokio::select! {
                _ = rx.changed() => {}
                _ = tokio::time::sleep_until(deadline) => panic!("ICE not connected"),
            }
        }
    }
}
