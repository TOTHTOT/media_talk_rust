//! UI 状态机: 三态 (Idle/Outgoing/Incoming/InCall) 的全部切换决策,
//! 不依赖 slint, 纯逻辑可单测. slint 层只把字段映射到属性.

use chrono::{DateTime, Local};
use tracing::info;

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
/// 只存结构化字段, 展示文案由 Slint 侧 @tr 组合, 便于 i18n
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    /// true=呼出 false=呼入 (未接必然是呼入)
    pub outgoing: bool,
    /// 未接来电
    pub missed: bool,
    /// 是否接通 (收到过 Confirmed)
    pub connected: bool,
    pub peer: String,
    /// 结束时刻 "10:12"
    pub time: String,
    /// 接通时长 (秒), connected=true 时有意义
    pub duration_secs: i64,
}

/// 后台 → UI 的事件 (SIP 会话 / 监控流等, 统一走 UiHandle 漏斗投递)
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
    /// 与通话生命周期无关的提示/警告 (如端口回退): 不进状态机,
    /// on_event 原样透传给通知管线
    Notify(Notice),
    /// 监控流失败终态 (ingest 侧重试耗尽/认证错): 不进状态机.
    /// 代次校验与复位在 bridge 投递点完成 (依赖 monitor-selected-id
    /// 属性), 到这里只负责出通知; error 为错误原文快照
    MonitorFailed {
        id: i32,
        error: Option<String>,
    },
}

/// 通知级别: 决定呈现方式 (Info/Warn → toast 气泡, Error → 顶部错误条)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum NoticeLevel {
    Info = 0,
    Warn = 1,
    Error = 2,
}

/// 通知种类: 前端按 kind 用 @tr 解析文案 (i18n), 后端不发成品文案
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum NoticeKind {
    /// 无本地化文案, detail 即全文
    Raw = 0,
    RegisterFailed = 1,
    CallFailed = 2,
    /// 摄像头已保存 (设置页添加成功)
    CameraSaved = 3,
    /// 摄像头保存失败 (ONVIF 取流地址/入库), detail 为错误原文
    CameraSaveFailed = 4,
    /// 监控页摄像头连接失败 (同步构建错或异步 RTSP 失败/超时),
    /// detail 为错误原文
    CameraConnectFailed = 5,
    /// 同 IP 设备已保存过同等/更高画质的流, 本次未重复添加
    CameraDuplicate = 6,
}

/// 后端 → UI 的通知 (toast/错误条统一走它): 分级 + 结构化 kind.
/// 前端按 kind 自己解析并显示, 后端不拼文案
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub level: NoticeLevel,
    pub kind: NoticeKind,
    /// 可选补充: 系统错误原文 (SIP 状态行等) 或后端想直接提示的内容;
    /// None 时前端只显示 kind 对应的本地化文案
    pub detail: Option<String>,
}

impl Notice {
    pub fn error(kind: NoticeKind) -> Self {
        Self {
            level: NoticeLevel::Error,
            kind,
            detail: None,
        }
    }

    pub fn warn(kind: NoticeKind) -> Self {
        Self {
            level: NoticeLevel::Warn,
            kind,
            detail: None,
        }
    }

    /// 附加 detail; 空串视为无 (前端不拼分隔符)
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        self.detail = (!detail.is_empty()).then_some(detail);
        self
    }
}

/// UI 决策出的动作, 直接就是 SIP worker 命令, 无需翻译
pub use crate::bridge::WorkerCmd;

#[derive(Debug, Default)]
pub struct AppState {
    pub phase: Phase,
    pub peer: String,
    /// SIP 注册状态 (状态栏绿灯/主页"通话服务"行)
    pub registered: bool,
    /// 今日访客数 (来电次数, 进程内计数, 不持久化)
    pub today_visitors: u32,
    /// 最近通话记录, 新的在前, 上限 20 条 (主页只显示前 2)
    pub call_log: Vec<CallRecord>,
    /// 已记账但还没落盘的记录 (1s 定时器批量写 SQLite, 见 lib.rs)
    unsaved: Vec<CallRecord>,
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
        let time = now.format("%H:%M").to_string();
        let (outgoing, missed, connected, duration_secs) =
            match (self.phase, self.call_dir, self.confirmed_at) {
                (Phase::InCall, dir, Some(t0)) => (
                    dir != Some(CallDir::Incoming),
                    false,
                    true,
                    (now - t0).num_seconds(),
                ),
                (Phase::InCall, _, None) => (true, false, false, 0),
                (Phase::Outgoing, _, _) => (true, false, false, 0),
                (Phase::Incoming, _, _) => (false, true, false, 0),
                (Phase::Idle, _, _) => return,
            };
        self.call_log.insert(
            0,
            CallRecord {
                outgoing,
                missed,
                connected,
                peer: self.peer.clone(),
                time,
                duration_secs,
            },
        );
        self.call_log.truncate(20);
        // 待落盘副本 (持久化不阻塞状态机, 由外部定时器取走写库)
        self.unsaved.push(self.call_log[0].clone());
    }

    /// 取走全部待落盘记录 (调用方负责写 SQLite)
    pub fn take_unsaved_records(&mut self) -> Vec<CallRecord> {
        std::mem::take(&mut self.unsaved)
    }

    /// 通话收尾公共段: 记账 + 回 Idle + 清通话上下文
    fn end_call(&mut self) {
        self.push_call_record();
        self.phase = Phase::Idle;
        self.peer.clear();
        self.call_dir = None;
        self.confirmed_at = None;
    }

    /// SIP 事件驱动状态迁移, 返回值是要弹给用户的通知 (None = 不弹).
    /// 事件可能迟到 (比如挂断后才来 Confirmed): Incoming/Confirmed 与当前
    /// 阶段不匹配时忽略不迁移, 避免界面乱跳; Terminated/Failed 是终态事件,
    /// 无条件回 Idle 并清 peer, 但 Failed 仅在非 Idle 时才产生通知
    /// (Idle 下属于迟到事件, 不弹错误条) — 通知的唯一发源点在这里,
    /// bridge 不再自行判断
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `ev`: SIP 后台发来的 UI 事件
    ///
    /// returns: Option<Notice>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn on_event(&mut self, ev: UiEvent) -> Option<Notice> {
        info!(?ev, "deal with a event");
        match ev {
            UiEvent::Registered => {
                self.registered = true;
                None
            }
            UiEvent::RegisterFailed(reason) => {
                self.registered = false;
                Some(Notice::error(NoticeKind::RegisterFailed).with_detail(reason))
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
                None
            }
            UiEvent::Confirmed { peer } => {
                if matches!(self.phase, Phase::Outgoing | Phase::Incoming) {
                    self.phase = Phase::InCall;
                    self.peer = peer;
                    self.confirmed_at = Some(Local::now());
                }
                None
            }
            UiEvent::Terminated { .. } => {
                self.end_call();
                None
            }
            UiEvent::Failed { reason } => {
                let notify = (self.phase != Phase::Idle)
                    .then(|| Notice::error(NoticeKind::CallFailed).with_detail(reason));
                self.end_call();
                notify
            }
            UiEvent::Notify(n) => Some(n),
            UiEvent::MonitorFailed { error, .. } => Some(
                Notice::warn(NoticeKind::CameraConnectFailed)
                    .with_detail(error.unwrap_or_default()),
            ),
        }
    }

    /// 拨打: 仅 Idle 且号码非空; 返回 WorkerCmd 给 worker
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `number`: 用户输入的号码 (内部先 trim, 全空白视为未输入)
    ///
    /// returns: Option<WorkerCmd>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn dial(&mut self, number: String) -> Option<WorkerCmd> {
        let number = number.trim().to_string();
        if self.phase != Phase::Idle || number.is_empty() {
            return None;
        }
        self.phase = Phase::Outgoing;
        self.peer = number.clone();
        self.call_dir = Some(CallDir::Outgoing);
        self.confirmed_at = None;
        Some(WorkerCmd::Dial(number))
    }

    /// 接听: 仅 Incoming
    pub fn accept(&mut self) -> Option<WorkerCmd> {
        (self.phase == Phase::Incoming).then_some(WorkerCmd::Accept)
    }

    /// 拒接: 仅 Incoming, 立即回 Idle
    ///
    ///
    ///
    /// returns: Option<WorkerCmd>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn reject(&mut self) -> Option<WorkerCmd> {
        if self.phase != Phase::Incoming {
            return None;
        }
        self.end_call();
        Some(WorkerCmd::Reject)
    }

    /// 挂断/取消: Outgoing 或 InCall 有效, 立即回 Idle
    /// (不等 worker 确认, 界面对讲机场景手感优先; worker 失败会再报 Failed)
    ///
    ///
    ///
    /// returns: Option<WorkerCmd>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
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
        let notice = app.on_event(UiEvent::Failed {
            reason: "486 Busy".into(),
        });
        assert_eq!(app.phase, Phase::Idle);
        assert_eq!(
            notice,
            Some(Notice::error(NoticeKind::CallFailed).with_detail("486 Busy"))
        );
    }

    #[test]
    fn stale_failed_in_idle_produces_no_notice() {
        let mut app = AppState::default();
        let notice = app.on_event(UiEvent::Failed {
            reason: "408 Timeout".into(),
        });
        assert_eq!(app.phase, Phase::Idle);
        assert_eq!(notice, None, "Idle 下迟到的 Failed 不得弹错误条");
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
        assert_eq!(app.on_event(UiEvent::Registered), None);
        assert!(app.registered);
        let notice = app.on_event(UiEvent::RegisterFailed("403".into()));
        assert!(!app.registered);
        assert_eq!(
            notice,
            Some(Notice::error(NoticeKind::RegisterFailed).with_detail("403"))
        );
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
        assert!(app.call_log[0].missed);
        assert!(!app.call_log[0].outgoing);
        assert_eq!(app.call_log[0].peer, "1001");
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
        assert!(app.call_log[0].missed);
    }

    #[test]
    fn outgoing_unanswered_logged_on_hangup() {
        let mut app = AppState::default();
        app.dial("1002".into());
        app.hangup();
        assert_eq!(app.call_log.len(), 1);
        assert!(app.call_log[0].outgoing);
        assert!(!app.call_log[0].connected);
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
        assert!(app.call_log[0].outgoing);
        assert!(app.call_log[0].connected);
        assert!(app.call_log[0].duration_secs >= 0);
    }

    #[test]
    fn monitor_failed_yields_warn_notice_without_touching_call_state() {
        let mut app = AppState::default();
        let n = app
            .on_event(UiEvent::MonitorFailed {
                id: 7,
                error: Some("401 Unauthorized".into()),
            })
            .expect("notice");
        assert_eq!(n.level, NoticeLevel::Warn);
        assert_eq!(n.kind, NoticeKind::CameraConnectFailed);
        assert_eq!(n.detail.as_deref(), Some("401 Unauthorized"));
        // 无错误原文时 detail 为 None, 前端只显示 kind 本地化文案
        let n2 = app
            .on_event(UiEvent::MonitorFailed { id: 7, error: None })
            .expect("notice");
        assert!(n2.detail.is_none());
        // 不进状态机: 通话状态不受监控事件影响
        assert_eq!(app.phase, Phase::Idle);
    }
}
