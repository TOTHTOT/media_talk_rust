//! Session state machine (`StreamState`), counters (`StreamStats`) and
//! the public `GstStreamHandle` returned by [`crate::start`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use tracing::{info, warn};

/// Lifecycle of one streaming session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Connecting,
    Playing,
    Reconnecting,
    /// Terminal: retries exhausted or unrecoverable error.
    Failed,
    /// Terminal: EOS or explicit `stop()`.
    Ended,
}

/// 会话终态结论: 不可恢复失败 (带错误原文) 或 正常结束.
///
/// 终态与 [`StreamState`] 分开: 状态机只关心"现在处于哪个状态",
/// 终态结论额外冻结了"为什么结束"——错误原文在失败瞬间取快照,
/// 之后句柄被 `stop()` 改写也不影响这次结论的内容.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamTerminal {
    /// 不可恢复失败: 认证错 / 重试耗尽. `error` 是失败瞬间的错误原文
    Failed { error: Option<String> },
    /// 用户主动停止 / 正常收尾
    Ended,
}

/// 终态回调钩子: 包装一个 `Fn(StreamTerminal)` 回调.
///
/// 单独一层是为了让 `GstStreamConfig` 能继续派生 `Debug` / `Clone` ——
/// 闭包本身没有 `Debug` 实现.
#[derive(Clone)]
pub struct TerminalHook(Arc<dyn Fn(StreamTerminal) + Send + Sync>);

impl TerminalHook {
    /// 包装终态回调.
    ///
    /// # Arguments
    ///
    /// * `f`: 终态回调 (在 ingest 会话线程上触发, 应尽快返回)
    ///
    /// returns: TerminalHook
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn new(f: impl Fn(StreamTerminal) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// 触发回调 (crate 内部使用).
    pub(crate) fn call(&self, terminal: StreamTerminal) {
        (self.0)(terminal);
    }
}

impl std::fmt::Debug for TerminalHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TerminalHook")
    }
}

/// Cumulative session counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamStats {
    pub frames_video: u64,
    pub frames_audio: u64,
    pub bytes: u64,
    pub reconnects: u32,
    pub last_error: Option<String>,
}

/// Pure transition table for the session state machine.
///
/// Legal moves: Connecting→Playing/Failed/Ended,
/// Playing→Reconnecting/Ended/Failed,
/// Reconnecting→Connecting/Failed/Ended. Failed and Ended are terminal.
pub(crate) fn can_transition(from: StreamState, to: StreamState) -> bool {
    use StreamState::*;
    matches!(
        (from, to),
        (Connecting, Playing)
            | (Connecting, Failed)
            | (Connecting, Ended)
            | (Playing, Reconnecting)
            | (Playing, Ended)
            | (Playing, Failed)
            | (Reconnecting, Connecting)
            | (Reconnecting, Failed)
            | (Reconnecting, Ended)
    )
}

/// 终态判定: `Failed` / `Ended` 是终态, 其余都是中间态.
pub(crate) fn is_terminal(state: StreamState) -> bool {
    matches!(state, StreamState::Failed | StreamState::Ended)
}

/// Interruptible stop signal shared by the bus/session threads.
/// `stop()` wakes every [`wait_or_stop`] waiter immediately so reconnect
/// backoff never delays shutdown (crate-internal; used by the pipeline).
#[derive(Debug)]
pub(crate) struct StopSignal {
    flag: AtomicBool,
    lock: Mutex<()>,
    cv: Condvar,
}

impl StopSignal {
    pub(crate) fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            lock: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    pub(crate) fn stop(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// Sleep for `d`, returning `true` when the full duration elapsed and
/// `false` when `signal.stop()` interrupted the wait.
pub(crate) fn wait_or_stop(signal: &StopSignal, d: Duration) -> bool {
    let deadline = Instant::now() + d;
    let mut guard = signal.lock.lock();
    loop {
        if signal.is_stopped() {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        let _ = signal.cv.wait_for(&mut guard, deadline - now);
    }
}

struct Inner {
    state: StreamState,
    stats: StreamStats,
    /// Hook fired once on the first `stop()` (tears down the pipeline).
    stop_hook: Option<Box<dyn Fn() + Send>>,
    /// 终态结论快照: 进入终态时冻结, 含失败瞬间的错误原文.
    terminal: Option<StreamTerminal>,
    /// 终态回调只允许触发一次 (防止 stop() 与失败竞态时重复通知).
    terminal_fired: bool,
    /// 终态回调钩子 (来自 GstStreamConfig::on_terminal).
    on_terminal: Option<TerminalHook>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("state", &self.state)
            .field("stats", &self.stats)
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            state: StreamState::Connecting,
            stats: StreamStats::default(),
            stop_hook: None,
            terminal: None,
            terminal_fired: false,
            on_terminal: None,
        }
    }
}

/// Opaque session handle: query state/stats and stop the stream.
/// Cheap to clone, `Send + Sync`.
#[derive(Debug, Clone)]
pub struct GstStreamHandle {
    inner: Arc<Mutex<Inner>>,
    /// Signalled whenever the state changes; lets `wait_for_terminal`
    /// block instead of polling.
    cv: Arc<Condvar>,
}

impl GstStreamHandle {
    /// 新建句柄, 带终态回调钩子.
    ///
    /// # Arguments
    ///
    /// * `on_terminal`: 终态回调钩子 (来自 `GstStreamConfig::on_terminal`)
    ///
    /// returns: GstStreamHandle
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub(crate) fn with_terminal(on_terminal: Option<TerminalHook>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                on_terminal,
                ..Inner::default()
            })),
            cv: Arc::new(Condvar::new()),
        }
    }

    /// 阻塞等待会话到达终态 (失败 / 结束), 最多等 `timeout`.
    ///
    /// 返回终态结论, 超时返回 `None`. 与 `GstStreamConfig::on_terminal`
    /// 回调语义等价, 二选一即可: 回调式适合 GUI (回调里切回 UI 线程),
    /// 阻塞式适合已经在异步任务里等待的调用方.
    ///
    /// 终态结论里的错误原文是失败瞬间的快照, 之后句柄被 `stop()`
    /// 改写也不影响已返回的内容.
    ///
    /// # Arguments
    ///
    /// * `timeout`: 最多等待多久 (超时后返回 `None`, 不改变会话状态)
    ///
    /// returns: Option<StreamTerminal> — 终态结论, 或超时时的 `None`
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn wait_for_terminal(&self, timeout: Duration) -> Option<StreamTerminal> {
        // timeout 接近 Duration::MAX 时加法会溢出 panic, 退化为"无限等"
        let deadline = Instant::now().checked_add(timeout);
        let mut guard = self.inner.lock();
        loop {
            if let Some(terminal) = &guard.terminal {
                return Some(terminal.clone());
            }
            match deadline {
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return None;
                    }
                    self.cv.wait_for(&mut guard, d - now);
                }
                // 溢出: 永不超时, 等一个固定片再复查终态
                None => {
                    self.cv.wait_for(&mut guard, Duration::from_secs(60));
                }
            }
        }
    }

    pub fn state(&self) -> StreamState {
        self.inner.lock().state
    }

    pub fn stats(&self) -> StreamStats {
        self.inner.lock().stats.clone()
    }

    /// 当前会话的终态结论 (若已到达终态).
    ///
    /// returns: Option<StreamTerminal>
    pub fn terminal(&self) -> Option<StreamTerminal> {
        self.inner.lock().terminal.clone()
    }

    /// Register the teardown hook fired by the first `stop()` (the
    /// pipeline sets state to Null there). Called once by `start()`.
    pub(crate) fn set_stop_hook(&self, hook: impl Fn() + Send + 'static) {
        self.inner.lock().stop_hook = Some(Box::new(hook));
    }

    /// Stop the session: fire the teardown hook (once), transition to
    /// `Ended`, never reconnect. Idempotent — calling it again (or
    /// after a terminal `Failed`) does not panic and does not change
    /// the state.
    pub fn stop(&self) {
        let hook = {
            let mut inner = self.inner.lock();
            if matches!(inner.state, StreamState::Ended | StreamState::Failed) {
                return;
            }
            inner.stop_hook.take()
        };
        // Run the hook outside the lock: pipeline teardown may wake the
        // bus thread, which touches this same handle.
        if let Some(hook) = hook {
            hook();
        }
        self.transition(StreamState::Ended);
    }

    /// Attempt a state transition. Illegal moves are logged and ignored
    /// so a misbehaving pipeline can never corrupt the state machine.
    ///
    /// 进入终态时在锁内冻结终态结论快照 (含失败瞬间的错误原文);
    /// 回调的触发不在这里 —— 终态回调统一由会话线程在退出前触发,
    /// 保证它只跑一次且不阻塞状态转移.
    pub(crate) fn transition(&self, new_state: StreamState) {
        {
            let mut inner = self.inner.lock();
            let from = inner.state;
            if from == new_state {
                return;
            }
            if !can_transition(from, new_state) {
                warn!(?from, to = ?new_state, "illegal stream state transition ignored");
                return;
            }
            if new_state == StreamState::Reconnecting {
                inner.stats.reconnects += 1;
            }
            inner.state = new_state;
            if inner.terminal.is_none() && is_terminal(new_state) {
                inner.terminal = Some(match new_state {
                    StreamState::Failed => StreamTerminal::Failed {
                        error: inner.stats.last_error.clone(),
                    },
                    _ => StreamTerminal::Ended,
                });
            }
        };
        self.cv.notify_all();
        info!(to = ?new_state, "stream state transition");
    }

    /// 触发终态回调 (只触发一次). 由会话线程在退出前调用.
    ///
    /// # Arguments
    ///
    /// * `terminal`: 终态结论 (通常取自 [`GstStreamHandle::terminal`])
    ///
    /// returns: ()
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub(crate) fn fire_terminal(&self, terminal: StreamTerminal) {
        let hook = {
            let mut inner = self.inner.lock();
            if inner.terminal_fired {
                return;
            }
            inner.terminal_fired = true;
            inner.on_terminal.take()
        };
        if let Some(hook) = hook {
            hook.call(terminal);
        }
    }

    /// Record one delivered frame (crate-internal, called by the pipeline).
    pub(crate) fn note_frame(&self, bytes: u64, is_audio: bool) {
        let mut inner = self.inner.lock();
        if is_audio {
            inner.stats.frames_audio += 1;
        } else {
            inner.stats.frames_video += 1;
        }
        inner.stats.bytes += bytes;
    }

    /// Record the latest error message (crate-internal).
    pub(crate) fn note_error(&self, msg: impl Into<String>) {
        self.inner.lock().stats.last_error = Some(msg.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_transitions_succeed() {
        let h = GstStreamHandle::with_terminal(None);
        assert_eq!(h.state(), StreamState::Connecting);

        h.transition(StreamState::Playing);
        assert_eq!(h.state(), StreamState::Playing);

        h.transition(StreamState::Reconnecting);
        assert_eq!(h.state(), StreamState::Reconnecting);
        assert_eq!(h.stats().reconnects, 1);

        h.transition(StreamState::Connecting);
        assert_eq!(h.state(), StreamState::Connecting);

        h.transition(StreamState::Playing);
        h.transition(StreamState::Ended);
        assert_eq!(h.state(), StreamState::Ended);
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        let h = GstStreamHandle::with_terminal(None);
        // Connecting → Reconnecting is not in the table
        h.transition(StreamState::Reconnecting);
        assert_eq!(h.state(), StreamState::Connecting);
        assert_eq!(h.stats().reconnects, 0);

        // Ended is terminal: Ended → Playing must not move
        h.transition(StreamState::Ended);
        h.transition(StreamState::Playing);
        assert_eq!(h.state(), StreamState::Ended);

        // Failed is terminal too
        let h2 = GstStreamHandle::with_terminal(None);
        h2.transition(StreamState::Failed);
        h2.transition(StreamState::Connecting);
        assert_eq!(h2.state(), StreamState::Failed);
    }

    #[test]
    fn stop_is_idempotent() {
        let h = GstStreamHandle::with_terminal(None);
        h.transition(StreamState::Playing);
        h.stop();
        assert_eq!(h.state(), StreamState::Ended);
        // second stop: no panic, no state change
        h.stop();
        assert_eq!(h.state(), StreamState::Ended);

        // stop after Failed keeps Failed (terminal)
        let h2 = GstStreamHandle::with_terminal(None);
        h2.transition(StreamState::Failed);
        h2.stop();
        assert_eq!(h2.state(), StreamState::Failed);
    }

    #[test]
    fn stop_fires_hook_exactly_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        let calls = Arc::new(AtomicU32::new(0));
        let h = GstStreamHandle::with_terminal(None);
        let calls2 = calls.clone();
        h.set_stop_hook(move || {
            calls2.fetch_add(1, Ordering::SeqCst);
        });
        h.stop();
        h.stop();
        assert_eq!(h.state(), StreamState::Ended);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stats_accumulate() {
        let h = GstStreamHandle::with_terminal(None);
        h.note_frame(100, false);
        h.note_frame(50, true);
        h.note_frame(200, false);
        let s = h.stats();
        assert_eq!(s.frames_video, 2);
        assert_eq!(s.frames_audio, 1);
        assert_eq!(s.bytes, 350);

        h.note_error("boom");
        assert_eq!(h.stats().last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn reconnect_cycle_transitions() {
        let h = GstStreamHandle::with_terminal(None);
        h.transition(StreamState::Playing);
        h.transition(StreamState::Reconnecting);
        h.transition(StreamState::Connecting);
        h.transition(StreamState::Playing);
        h.transition(StreamState::Reconnecting);
        assert_eq!(h.state(), StreamState::Reconnecting);
        assert_eq!(h.stats().reconnects, 2);

        // Reconnecting → Playing directly is NOT in the table (must go
        // through Connecting); retries exhausted goes Reconnecting → Failed.
        h.transition(StreamState::Playing);
        assert_eq!(h.state(), StreamState::Reconnecting);
        h.transition(StreamState::Failed);
        assert_eq!(h.state(), StreamState::Failed);
    }

    #[test]
    fn wait_or_stop_returns_true_after_full_wait() {
        let signal = StopSignal::new();
        let started = Instant::now();
        assert!(wait_or_stop(&signal, Duration::from_millis(20)));
        assert!(started.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn wait_or_stop_is_interrupted_by_stop() {
        let signal = Arc::new(StopSignal::new());
        let signal2 = signal.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            signal2.stop();
        });
        let started = Instant::now();
        // 60s backoff must be interrupted almost immediately.
        assert!(!wait_or_stop(&signal, Duration::from_secs(60)));
        assert!(started.elapsed() < Duration::from_secs(5));
        t.join().unwrap();
    }

    #[test]
    fn wait_for_terminal_times_out_without_terminal_state() {
        let h = GstStreamHandle::with_terminal(None);
        let started = Instant::now();
        assert_eq!(h.wait_for_terminal(Duration::from_millis(50)), None);
        assert!(started.elapsed() >= Duration::from_millis(50));
        // 超时不改变会话状态
        assert_eq!(h.state(), StreamState::Connecting);
    }

    #[test]
    fn wait_for_terminal_observes_failure_with_error_snapshot() {
        let h = GstStreamHandle::with_terminal(None);
        h.note_error("401 Unauthorized");
        h.transition(StreamState::Failed);
        let t = h.wait_for_terminal(Duration::from_millis(50));
        assert_eq!(
            t,
            Some(StreamTerminal::Failed {
                error: Some("401 Unauthorized".to_string())
            })
        );
    }

    #[test]
    fn wait_for_terminal_observes_ended_without_error() {
        let h = GstStreamHandle::with_terminal(None);
        h.transition(StreamState::Ended);
        assert_eq!(
            h.wait_for_terminal(Duration::from_millis(50)),
            Some(StreamTerminal::Ended)
        );
    }

    #[test]
    fn wait_for_terminal_unblocks_when_terminal_arrives_later() {
        let h = GstStreamHandle::with_terminal(None);
        let h2 = h.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            h2.transition(StreamState::Failed);
        });
        let got = h.wait_for_terminal(Duration::from_secs(5));
        assert_eq!(got, Some(StreamTerminal::Failed { error: None }));
        t.join().unwrap();
    }

    #[test]
    fn terminal_callback_fires_exactly_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let hook = TerminalHook::new(move |_t: StreamTerminal| {
            calls2.fetch_add(1, Ordering::SeqCst);
        });
        let h = GstStreamHandle::with_terminal(Some(hook));
        h.transition(StreamState::Failed);
        h.fire_terminal(StreamTerminal::Failed { error: None });
        h.fire_terminal(StreamTerminal::Failed { error: None });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn terminal_callback_receives_error_snapshot() {
        use std::sync::Arc;
        use std::sync::Mutex;

        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let hook = TerminalHook::new(move |t: StreamTerminal| {
            seen2.lock().unwrap().push(t);
        });
        let h = GstStreamHandle::with_terminal(Some(hook));
        h.note_error("connect failed");
        h.transition(StreamState::Failed);
        let terminal = h.terminal().expect("terminal frozen");
        h.fire_terminal(terminal);
        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![StreamTerminal::Failed {
                error: Some("connect failed".to_string())
            }]
        );
    }

    #[test]
    fn terminal_snapshot_survives_later_error_overwrite() {
        let h = GstStreamHandle::with_terminal(None);
        h.note_error("first");
        h.transition(StreamState::Failed);
        let snapshot = h.terminal().expect("terminal frozen");
        // 失败后又有新错误写进来, 已冻结的快照不受影响
        h.note_error("second");
        assert_eq!(
            snapshot,
            StreamTerminal::Failed {
                error: Some("first".to_string())
            }
        );
    }

    #[test]
    fn stop_produces_ended_terminal() {
        let h = GstStreamHandle::with_terminal(None);
        h.stop();
        assert_eq!(h.terminal(), Some(StreamTerminal::Ended));
    }
}
