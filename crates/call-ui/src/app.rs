//! UI 状态机: 三态 (Idle/Outgoing/Incoming/InCall) 的全部切换决策,
//! 不依赖 slint, 纯逻辑可单测. slint 层只把字段映射到属性.

use chrono::{DateTime, Local};

/// 界面阶段, 与 ui/main.slint 的 phase 属性一一对应
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum Phase {
    #[default]
    Idle = 0,
    Outgoing = 1,
    Incoming = 2,
    InCall = 3,
}

/// 通话方向 (记最近通话用)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallDir {
    Outgoing,
    Incoming,
}

/// 一条最近通话记录 (主页"最近通话"条)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    /// "呼出" / "呼入" / "未接"
    pub kind: &'static str,
    pub peer: String,
    /// "10:12 · 42 秒" / "10:12 · 未接通" / "09:47"
    pub detail: String,
    pub missed: bool,
}

/// 通话时长可读格式: 90 → "1 分 30 秒"
fn fmt_duration(secs: i64) -> String {
    if secs >= 60 {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    } else {
        format!("{} 秒", secs.max(0))
    }
}

/// SIP 后台 → UI 的事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    Registered,
    RegisterFailed(String),
    Incoming {
        caller: String,
    },
    Confirmed {
        peer: String,
    },
    /// 通话结束 (对端 BYE / 本地挂断 / 对方取消), reason 进日志
    Terminated {
        reason: String,
    },
    /// 呼叫/接听失败 (4xx/5xx/超时/媒体起不来)
    Failed {
        reason: String,
    },
}

/// UI 决策出的动作, 直接就是 SIP worker 命令, 无需翻译
pub use crate::bridge::WorkerCmd;

#[derive(Debug, Default)]
pub struct AppState {
    pub phase: Phase,
    pub peer: String,
    pub error: String,
    /// SIP 注册状态 (状态栏绿灯/主页"通话服务"行)
    pub registered: bool,
    /// 今日访客数 (来电次数, 进程内计数, 不持久化)
    pub today_visitors: u32,
    /// 最近通话记录, 新的在前, 上限 20 条 (主页只显示前 2)
    pub call_log: Vec<CallRecord>,
    /// 当前/进行通话的方向, 记 call_log 用
    call_dir: Option<CallDir>,
    /// Confirmed 时刻, 通话时长从它算
    confirmed_at: Option<DateTime<Local>>,
}

impl AppState {
    /// 通话结束记账: 按结束时的阶段记 呼出(时长/未接通) / 呼入(时长) /
    /// 未接. 本地挂断/拒接在 UI 动作里已记过 (阶段已回 Idle), 这里跳过
    fn push_call_record(&mut self) {
        let now = Local::now();
        let hm = now.format("%H:%M");
        let (kind, detail, missed) = match (self.phase, self.call_dir, self.confirmed_at) {
            (Phase::InCall, Some(CallDir::Outgoing), Some(t0)) => (
                "呼出",
                format!("{hm} · {}", fmt_duration((now - t0).num_seconds())),
                false,
            ),
            (Phase::InCall, Some(CallDir::Incoming), Some(t0)) => (
                "呼入",
                format!("{hm} · {}", fmt_duration((now - t0).num_seconds())),
                false,
            ),
            (Phase::InCall, _, _) => ("呼出", format!("{hm}"), false),
            (Phase::Outgoing, _, _) => ("呼出", format!("{hm} · 未接通"), false),
            (Phase::Incoming, _, _) => ("未接", format!("{hm}"), true),
            (Phase::Idle, _, _) => return,
        };
        self.call_log.insert(
            0,
            CallRecord {
                kind,
                peer: self.peer.clone(),
                detail,
                missed,
            },
        );
        self.call_log.truncate(20);
    }

    /// 通话收尾公共段: 记账 + 回 Idle + 清通话上下文
    fn end_call(&mut self) {
        self.push_call_record();
        self.phase = Phase::Idle;
        self.peer.clear();
        self.call_dir = None;
        self.confirmed_at = None;
    }

    /// SIP 事件驱动状态迁移. 事件可能迟到 (比如挂断后才来 Confirmed):
    /// Incoming/Confirmed 与当前阶段不匹配时忽略不迁移, 避免界面乱跳;
    /// 但 Terminated/Failed 是终态事件, 无条件回 Idle 并清 peer,
    /// 其中 Failed 仅在非 Idle 时才写 error (Idle 下属于迟到事件, 不弹错误条)
    pub fn on_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Registered => {
                self.registered = true;
                self.error.clear();
            }
            UiEvent::RegisterFailed(reason) => {
                self.registered = false;
                self.error = format!("注册失败: {reason}");
            }
            UiEvent::Incoming { caller } => {
                self.today_visitors += 1;
                // 占线时 worker 已直接拒掉, 到这里一定是 Idle
                if self.phase == Phase::Idle {
                    self.phase = Phase::Incoming;
                    self.peer = caller;
                    self.call_dir = Some(CallDir::Incoming);
                    self.confirmed_at = None;
                }
            }
            UiEvent::Confirmed { peer } => {
                if matches!(self.phase, Phase::Outgoing | Phase::Incoming) {
                    self.phase = Phase::InCall;
                    self.peer = peer;
                    self.error.clear();
                    self.confirmed_at = Some(Local::now());
                }
            }
            UiEvent::Terminated { .. } | UiEvent::Failed { .. } => {
                if let UiEvent::Failed { reason } = &ev {
                    if self.phase != Phase::Idle {
                        self.error = reason.clone();
                    }
                }
                self.end_call();
            }
        }
    }

    /// 拨打: 仅 Idle 且号码非空; 返回 WorkerCmd 给 worker
    pub fn dial(&mut self, number: String) -> Option<WorkerCmd> {
        let number = number.trim().to_string();
        if self.phase != Phase::Idle || number.is_empty() {
            return None;
        }
        self.phase = Phase::Outgoing;
        self.peer = number.clone();
        self.error.clear();
        self.call_dir = Some(CallDir::Outgoing);
        self.confirmed_at = None;
        Some(WorkerCmd::Dial(number))
    }

    /// 接听: 仅 Incoming
    pub fn accept(&mut self) -> Option<WorkerCmd> {
        (self.phase == Phase::Incoming).then_some(WorkerCmd::Accept)
    }

    /// 拒接: 仅 Incoming, 立即回 Idle
    pub fn reject(&mut self) -> Option<WorkerCmd> {
        if self.phase != Phase::Incoming {
            return None;
        }
        self.end_call();
        Some(WorkerCmd::Reject)
    }

    /// 挂断/取消: Outgoing 或 InCall 有效, 立即回 Idle
    /// (不等 worker 确认, 界面对讲机场景手感优先; worker 失败会再报 Failed)
    pub fn hangup(&mut self) -> Option<WorkerCmd> {
        if !matches!(self.phase, Phase::Outgoing | Phase::InCall) {
            return None;
        }
        self.end_call();
        Some(WorkerCmd::Hangup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dial_from_idle_goes_outgoing() {
        let mut app = AppState::default();
        assert_eq!(
            app.dial("1002".into()),
            Some(WorkerCmd::Dial("1002".into()))
        );
        assert_eq!(app.phase, Phase::Outgoing);
        assert_eq!(app.peer, "1002");
    }

    #[test]
    fn dial_ignored_when_busy_or_empty() {
        let mut app = AppState::default();
        assert_eq!(app.dial("  ".into()), None);
        assert_eq!(app.phase, Phase::Idle);
        app.dial("1002".into());
        assert_eq!(app.dial("1003".into()), None, "呼叫中不能再拨");
    }

    #[test]
    fn outgoing_confirmed_goes_in_call_then_terminated_back_idle() {
        let mut app = AppState::default();
        app.dial("1002".into());
        app.on_event(UiEvent::Confirmed {
            peer: "1002".into(),
        });
        assert_eq!(app.phase, Phase::InCall);
        app.on_event(UiEvent::Terminated {
            reason: "peer bye".into(),
        });
        assert_eq!(app.phase, Phase::Idle);
        assert_eq!(app.peer, "");
    }

    #[test]
    fn incoming_accept_reject_flow() {
        let mut app = AppState::default();
        app.on_event(UiEvent::Incoming {
            caller: "1001".into(),
        });
        assert_eq!(app.phase, Phase::Incoming);
        assert_eq!(app.accept(), Some(WorkerCmd::Accept));
        // accept 不切状态, 等 Confirmed
        assert_eq!(app.phase, Phase::Incoming);
        app.on_event(UiEvent::Confirmed {
            peer: "1001".into(),
        });
        assert_eq!(app.phase, Phase::InCall);

        let mut app = AppState::default();
        app.on_event(UiEvent::Incoming {
            caller: "1001".into(),
        });
        assert_eq!(app.reject(), Some(WorkerCmd::Reject));
        assert_eq!(app.phase, Phase::Idle);
    }

    #[test]
    fn stale_confirmed_does_not_hijack_idle() {
        let mut app = AppState::default();
        app.dial("1002".into());
        app.hangup();
        assert_eq!(app.phase, Phase::Idle);
        app.on_event(UiEvent::Confirmed {
            peer: "1002".into(),
        });
        assert_eq!(
            app.phase,
            Phase::Idle,
            "迟到的 Confirmed 不得把界面拽进通话"
        );
    }

    #[test]
    fn failed_shows_error_and_returns_idle() {
        let mut app = AppState::default();
        app.dial("1002".into());
        app.on_event(UiEvent::Failed {
            reason: "486 Busy".into(),
        });
        assert_eq!(app.phase, Phase::Idle);
        assert!(app.error.contains("486"));
    }

    #[test]
    fn stale_failed_in_idle_keeps_error_empty() {
        let mut app = AppState::default();
        app.on_event(UiEvent::Failed {
            reason: "408 Timeout".into(),
        });
        assert_eq!(app.phase, Phase::Idle);
        assert_eq!(app.error, "", "Idle 下迟到的 Failed 不得弹错误条");
        assert_eq!(app.peer, "");
    }

    #[test]
    fn incoming_ignored_when_not_idle() {
        let mut app = AppState::default();
        app.dial("1002".into());
        app.on_event(UiEvent::Incoming {
            caller: "1001".into(),
        });
        assert_eq!(app.phase, Phase::Outgoing, "呼叫中收到来电不得被劫持");
        assert_eq!(app.peer, "1002");
    }

    #[test]
    fn registration_state_tracked() {
        let mut app = AppState::default();
        assert!(!app.registered);
        app.on_event(UiEvent::Registered);
        assert!(app.registered);
        app.on_event(UiEvent::RegisterFailed("403".into()));
        assert!(!app.registered);
        assert!(app.error.contains("403"));
    }

    #[test]
    fn visitors_counted_on_incoming() {
        let mut app = AppState::default();
        app.on_event(UiEvent::Incoming {
            caller: "1001".into(),
        });
        app.on_event(UiEvent::Terminated {
            reason: "cancel".into(),
        });
        app.on_event(UiEvent::Incoming {
            caller: "1002".into(),
        });
        assert_eq!(app.today_visitors, 2);
    }

    #[test]
    fn missed_call_logged_on_peer_cancel() {
        let mut app = AppState::default();
        app.on_event(UiEvent::Incoming {
            caller: "1001".into(),
        });
        app.on_event(UiEvent::Terminated {
            reason: "peer cancel".into(),
        });
        assert_eq!(app.call_log.len(), 1);
        assert_eq!(app.call_log[0].kind, "未接");
        assert_eq!(app.call_log[0].peer, "1001");
        assert!(app.call_log[0].missed);
    }

    #[test]
    fn rejected_call_logged_once() {
        let mut app = AppState::default();
        app.on_event(UiEvent::Incoming {
            caller: "1001".into(),
        });
        app.reject();
        // 拒接后迟到的 Terminated 不得重复记账
        app.on_event(UiEvent::Terminated {
            reason: "486 sent".into(),
        });
        assert_eq!(app.call_log.len(), 1);
        assert_eq!(app.call_log[0].kind, "未接");
    }

    #[test]
    fn outgoing_unanswered_logged_on_hangup() {
        let mut app = AppState::default();
        app.dial("1002".into());
        app.hangup();
        assert_eq!(app.call_log.len(), 1);
        assert_eq!(app.call_log[0].kind, "呼出");
        assert!(app.call_log[0].detail.contains("未接通"));
        assert!(!app.call_log[0].missed);
        // 挂断后迟到的 Terminated 不得重复记账
        app.on_event(UiEvent::Terminated {
            reason: "bye".into(),
        });
        assert_eq!(app.call_log.len(), 1);
    }

    #[test]
    fn confirmed_call_logs_duration() {
        let mut app = AppState::default();
        app.dial("1002".into());
        app.on_event(UiEvent::Confirmed {
            peer: "1002".into(),
        });
        app.on_event(UiEvent::Terminated {
            reason: "peer bye".into(),
        });
        assert_eq!(app.call_log.len(), 1);
        assert_eq!(app.call_log[0].kind, "呼出");
        assert!(app.call_log[0].detail.contains("秒"));
    }
}
