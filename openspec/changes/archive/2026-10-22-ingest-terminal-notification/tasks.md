## 1. ipcam-gst 终态类型与等待方法

- [x] 1.1 `ingest/stats.rs` 新增 `StreamTerminal` 枚举（`Failed { error: Option<String> }` / `Ended`），带 `Debug, Clone, PartialEq, Eq` 派生与 rustdoc 说明
- [x] 1.2 `ingest/stats.rs` 新增 `GstStreamHandle::wait_for_terminal(timeout) -> Option<StreamTerminal>`：用 `Condvar` 阻塞等终态，超时返回 `None`，不改变会话状态
- [x] 1.3 `ingest/stats.rs` 的 `transition()` 在进入终态时 `notify_all()` 唤醒等待者；`Inner` 持有 `Condvar`（或句柄持有 `Arc<Condvar>`，与 `inner` 同生命周期）
- [x] 1.4 补单测：终态可被 `wait_for_terminal` 观察到、超时返回 `None`、`Failed` 携带 `last_error` 快照、`Ended` 不携带
- [x] 1.5 `lib.rs` 导出 `StreamTerminal`

## 2. ipcam-gst 终态回调

- [x] 2.1 `ingest/config.rs` 的 `GstStreamConfig` 新增 `on_terminal: Option<TerminalHook>` 字段（`TerminalHook` 是包 `Arc<dyn Fn(StreamTerminal) + Send + Sync>` 的 newtype，单独一层让配置能继续派生 `Debug`/`Clone`），默认 `None`
- [x] 2.2 `GstStreamConfig` 的 `PartialEq`/`Eq` 改为手工实现：回调字段按 `Option` 的 `is_some()` 比较，不比较闭包内容
- [x] 2.3 `ingest/session.rs` 在会话循环退出前统一触发一次终态回调：`Failed` 携带 `handle.stats().last_error` 快照，`Ended` 不带错误
- [x] 2.4 终态回调在 `transition()` 之外触发（`transition` 保持纯状态转移、无副作用）
- [x] 2.5 补单测：会话到终态时回调恰好触发一次；`Failed` 携带错误原文；`Ended` 不携带；`stop()` 触发的 `Ended` 也触发回调

## 3. call-ui 监控页切换到回调

- [x] 3.1 `camera/mod.rs` 删除 `spawn_monitor_watch` 看门狗线程、`MAX_WAIT` 常量、`WatchVerdict` 枚举与 `watch_verdict` 纯函数及其单测
- [x] 3.2 `camera/mod.rs` 的 `MonitorController::select` 构造 `GstStreamConfig` 时挂 `on_terminal` 回调；回调内经 `slint::invoke_from_event_loop` 复位 `monitor-selected-id`、清画面、发 `CameraConnectFailed` 提示
- [x] 3.3 回调内的代次校验：`monitor-selected-id` 不等于本次 id 时静默丢弃（切台 / 离开 / 通话抢占场景不误报）
- [x] 3.4 `camera/mod.rs` 的监控配置设 `ReconnectPolicy { max_attempts: Some(3), .. }`
- [x] 3.5 `camera/mod.rs` 的 `select` 需要拿到 `slint::Weak<MainWindow>` 才能构造回调 —— 调整 `MonitorController::select` 签名或由调用方传入回调闭包，`wire_monitor` 负责装配
- [x] 3.6 确认 `MonitorController::current_handle` 无其他调用方后决定保留 / 删除

## 4. 文档与验证

- [x] 4.1 `docs/streaming-engine.md` 补「终态通知」小节：回调与阻塞等待两种方式、失败时限由 `max_attempts` 决定
- [x] 4.2 `docs/call-ui.md` 监控页相关描述同步（不再有宽限窗看门狗）
- [x] 4.3 `cargo fmt --all`
- [x] 4.4 `cargo clippy --workspace --all-targets --features sw-decode -- -D warnings`
- [x] 4.5 `cargo test --workspace --features sw-decode`
- [ ] 4.6 手工验证：选中不可达摄像头 → 约 7s 后弹提示 + 复位高亮 + 清画面；选中可达摄像头 → 正常出流不提示（需真实设备 / mediamtx 测试源）
