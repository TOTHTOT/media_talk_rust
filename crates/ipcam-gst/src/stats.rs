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

/// Interruptible stop signal shared by the bus/session threads.
/// `stop()` wakes every [`wait_or_stop`] waiter immediately so reconnect
/// backoff never delays shutdown (crate-internal; used by the pipeline).
#[cfg_attr(not(feature = "gst"), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct StopSignal {
    flag: AtomicBool,
    lock: Mutex<()>,
    cv: Condvar,
}

#[cfg_attr(not(feature = "gst"), allow(dead_code))]
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
#[cfg_attr(not(feature = "gst"), allow(dead_code))]
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
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("state", &self.state)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            state: StreamState::Connecting,
            stats: StreamStats::default(),
            stop_hook: None,
        }
    }
}

/// Opaque session handle: query state/stats and stop the stream.
/// Cheap to clone, `Send + Sync`.
#[derive(Debug, Clone)]
pub struct GstStreamHandle {
    inner: Arc<Mutex<Inner>>,
}

impl GstStreamHandle {
    // Used by the (feature-gated) pipeline; keep the warning off in
    // `gst`-less builds where only the tests reach these methods.
    #[cfg_attr(not(feature = "gst"), allow(dead_code))]
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    pub fn state(&self) -> StreamState {
        self.inner.lock().state
    }

    pub fn stats(&self) -> StreamStats {
        self.inner.lock().stats.clone()
    }

    /// Register the teardown hook fired by the first `stop()` (the
    /// pipeline sets state to Null there). Called once by `start()`.
    #[cfg_attr(not(feature = "gst"), allow(dead_code))]
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
    pub(crate) fn transition(&self, new_state: StreamState) {
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
        info!(?from, to = ?new_state, "stream state transition");
    }

    /// Record one delivered frame (crate-internal, called by the pipeline).
    #[cfg_attr(not(feature = "gst"), allow(dead_code))]
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
    #[allow(dead_code)]
    pub(crate) fn note_error(&self, msg: impl Into<String>) {
        self.inner.lock().stats.last_error = Some(msg.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_transitions_succeed() {
        let h = GstStreamHandle::new();
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
        let h = GstStreamHandle::new();
        // Connecting → Reconnecting is not in the table
        h.transition(StreamState::Reconnecting);
        assert_eq!(h.state(), StreamState::Connecting);
        assert_eq!(h.stats().reconnects, 0);

        // Ended is terminal: Ended → Playing must not move
        h.transition(StreamState::Ended);
        h.transition(StreamState::Playing);
        assert_eq!(h.state(), StreamState::Ended);

        // Failed is terminal too
        let h2 = GstStreamHandle::new();
        h2.transition(StreamState::Failed);
        h2.transition(StreamState::Connecting);
        assert_eq!(h2.state(), StreamState::Failed);
    }

    #[test]
    fn stop_is_idempotent() {
        let h = GstStreamHandle::new();
        h.transition(StreamState::Playing);
        h.stop();
        assert_eq!(h.state(), StreamState::Ended);
        // second stop: no panic, no state change
        h.stop();
        assert_eq!(h.state(), StreamState::Ended);

        // stop after Failed keeps Failed (terminal)
        let h2 = GstStreamHandle::new();
        h2.transition(StreamState::Failed);
        h2.stop();
        assert_eq!(h2.state(), StreamState::Failed);
    }

    #[test]
    fn stop_fires_hook_exactly_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        let calls = Arc::new(AtomicU32::new(0));
        let h = GstStreamHandle::new();
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
        let h = GstStreamHandle::new();
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
        let h = GstStreamHandle::new();
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
}
