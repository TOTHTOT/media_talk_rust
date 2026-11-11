//! libnice ICE agent 的安全封装: 手写 FFI (sys) + 安全包装 (agent/blob).
//! 独立于 gstreamer 媒体栈 (仅依赖 glib/gio), 供 UI 与无头测试工具复用.
/// 安全 NiceAgent 包装: glib 线程纪律 + 信令 blob 交换 + 状态订阅.
pub mod agent;
/// ICE blob: 原生 SDP 文本包装与候选行解析 (供 UI 状态展示).
pub mod blob;
pub(crate) mod sys;

pub use agent::{IceAgent, IceConfig, IceError, IceState, SelectedKind};
