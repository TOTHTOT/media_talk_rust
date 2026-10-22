## Why

`call-ui` 监控页的 RTSP 失败兜底目前是「UI 自己猜」：`camera/mod.rs` 起一个看门狗线程，轮询 `handle.state()`，并在 5 秒宽限窗内没等到 `Playing` 就判定失败。这有两个问题：

1. **宽限窗是拍脑袋的常数**。网络不通时 ingest 走的是无限重连循环（`ReconnectPolicy::max_attempts` 默认 `None`），UI 却要靠自己的 5s 猜测"这路肯定连不上"。UI 与 ingest 对"多久算失败"有两套标准，改动一边就会失配。
2. **失败信号没有下沉**。真正的失败判定在 ingest 侧（认证错、重试耗尽），但只有 `handle.state()` 能看到；`start_with_taps()` 同步返回 `Ok(handle)`，调用方拿不到任何失败回调，只能靠轮询。

改用 ingest 侧的重连次数上限（`max_attempts`）作为唯一的"多久算失败"标准，并把这套判定结果以回调形式推给调用方，UI 侧不再自己猜。

## What Changes

- **新增** `ipcam-gst` 公开类型 `StreamTerminal`：描述一路流到达终态时的结论 —— `Failed { error: Option<String> }`（不可恢复失败）或 `Ended`（用户主动停止 / EOS 后正常收尾）。新增其他终态语义时以新增变体方式扩展，不更换通道形态
- **新增** `GstStreamConfig::on_terminal` 字段：`Option<Arc<dyn Fn(StreamTerminal) + Send + Sync>>`，在会话到达终态时由 ingest 侧回调一次。回调在 ingest 的会话线程上触发，调用方需自行切回 UI 线程
- **新增** `GstStreamHandle::wait_for_terminal(timeout) -> Option<StreamTerminal>`：阻塞等终态，供不想用回调的调用方（如 `web-display` 的异步任务）轮询式等待；回调与本方法二选一，语义等价
- **修改** `ipcam-gst` 会话状态机：终态判定与"最后一次错误原文"一并冻结，`StreamTerminal::Failed` 携带 `stats.last_error` 快照，调用方无需在回调里二次读 `stats()`
- **修改** `call-ui` 监控页：删除 `spawn_monitor_watch` 看门狗线程、`MAX_WAIT` 兜底常数、`WatchVerdict::Wait` 分支与 `watch_verdict` 纯函数；改为在 `MonitorController::select` 构造 `GstStreamConfig` 时挂 `on_terminal` 回调，回调内经 `invoke_from_event_loop` 回 UI 线程做 toast + 复位选中
- **修改** `call-ui` 监控配置：`ReconnectPolicy::max_attempts` 设为 `Some(3)`（退避 1s→2s→4s，约 7s 后放弃），作为"多久算失败"的唯一标准
- **非破坏性**：现有 `GstStreamConfig` 构造点全部使用 `..Default::default()`，新增字段自动取默认值；`start()` / `start_with_taps()` 签名不变

## Capabilities

### New Capabilities
- `ingest-terminal-notification`: RTSP 会话终态（失败 / 结束）的可观测性 —— ingest 侧判定终态后以回调或阻塞等待两种方式通知调用方，调用方无需轮询 `state()`、也无需自行猜测失败宽限窗

### Modified Capabilities
<!-- 无现有 spec 的需求变更 -->

## Impact

- `crates/ipcam-gst/src/ingest/stats.rs`：新增 `StreamTerminal` 类型、`GstStreamHandle::wait_for_terminal`、`GstStreamConfig` 的 `on_terminal` 字段接线、终态错误快照冻结
- `crates/ipcam-gst/src/ingest/session.rs`：会话循环在终态分支触发 `on_terminal` 回调
- `crates/ipcam-gst/src/ingest/config.rs`：`GstStreamConfig` 新增字段（`PartialEq` 派生需处理回调字段，见 design 决策 3）
- `crates/ipcam-gst/src/lib.rs`：导出 `StreamTerminal`
- `crates/call-ui/src/camera/mod.rs`：删除看门狗线程与 `watch_verdict`，改用 `on_terminal` 回调；`MonitorController::select` 需接收回调闭包
- `crates/call-ui/src/wiring.rs`：把 `on_terminal` 回调所需的 `slint::Weak<MainWindow>` 传入监控装配链路
- `crates/web-display/src/stream.rs`：现有 200ms 轮询 `handle.state()` 的等待循环可改用 `wait_for_terminal`，消除轮询（可选，不在本 change 的硬性范围内）
- 其他 `GstStreamConfig` 构造点（`ipcam-gst/examples/*`）不受影响，走 `..Default::default()`
- 不新增依赖，不新增 feature flag
