//! UI 状态机: 三态 (Idle/Outgoing/Incoming/InCall) 的全部切换决策,
//! 不依赖 slint, 纯逻辑可单测. slint 层只把字段映射到属性.

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
}

impl AppState {
    /// SIP 事件驱动状态迁移. 事件可能迟到 (比如挂断后才来 Confirmed):
    /// Incoming/Confirmed 与当前阶段不匹配时忽略不迁移, 避免界面乱跳;
    /// 但 Terminated/Failed 是终态事件, 无条件回 Idle 并清 peer,
    /// 其中 Failed 仅在非 Idle 时才写 error (Idle 下属于迟到事件, 不弹错误条)
    pub fn on_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Registered => self.error.clear(),
            UiEvent::RegisterFailed(reason) => self.error = format!("注册失败: {reason}"),
            UiEvent::Incoming { caller } => {
                // 占线时 worker 已直接拒掉, 到这里一定是 Idle
                if self.phase == Phase::Idle {
                    self.phase = Phase::Incoming;
                    self.peer = caller;
                }
            }
            UiEvent::Confirmed { peer } => {
                if matches!(self.phase, Phase::Outgoing | Phase::Incoming) {
                    self.phase = Phase::InCall;
                    self.peer = peer;
                    self.error.clear();
                }
            }
            UiEvent::Terminated { .. } | UiEvent::Failed { .. } => {
                if let UiEvent::Failed { reason } = &ev {
                    if self.phase != Phase::Idle {
                        self.error = reason.clone();
                    }
                }
                self.phase = Phase::Idle;
                self.peer.clear();
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
        self.phase = Phase::Idle;
        self.peer.clear();
        Some(WorkerCmd::Reject)
    }

    /// 挂断/取消: Outgoing 或 InCall 有效, 立即回 Idle
    /// (不等 worker 确认, 界面对讲机场景手感优先; worker 失败会再报 Failed)
    pub fn hangup(&mut self) -> Option<WorkerCmd> {
        if !matches!(self.phase, Phase::Outgoing | Phase::InCall) {
            return None;
        }
        self.phase = Phase::Idle;
        self.peer.clear();
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
}
