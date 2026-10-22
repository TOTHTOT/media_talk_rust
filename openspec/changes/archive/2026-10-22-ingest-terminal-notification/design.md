## Context

`call-ui` 监控页选中一路摄像头后，`MonitorController::select` 调用 `ipcam_gst::start_with_taps(cfg, taps)`。这个调用是**同步返回句柄**的：RTSP 连不上时它照样返回 `Ok(handle)`，真正的失败发生在 ingest 的后台总线线程里（bus ERROR → 重连循环 → 重试耗尽才转 `Failed`）。

调用方因此面临两个空缺：

1. **何时算失败** —— `ReconnectPolicy::max_attempts` 默认是 `None`（无限重试），网络不通时 ingest 永远不会自己到 `Failed`。旧实现用「5 秒宽限窗 + 轮询 `state()`」在 UI 侧猜测一个失败时限。
2. **怎么知道失败** —— 没有回调，UI 只能轮询句柄状态。

现状实现（`crates/call-ui/src/camera/mod.rs`）是：每次 select 起一个 `std::thread`，300ms 轮询 `handle.state()`，5s 内没到 `Playing` 判失败。派生问题：

- 宽限窗 5s 是拍脑袋的常数，与 ingest 的重试策略（1s→2s→4s 退避）没有关联，两者一旦失配，UI 会在 ingest 还在正常重连时误报失败，或在 ingest 已放弃后迟迟不报。
- 每次 select 起一个线程 + 一次 300ms 轮询，线程只在终态时才退出。
- `MAX_WAIT = 60s` 是"状态机异常"时的兜底魔数，到点后静默退出 —— 用户看不到任何提示。

`ipcam-gst` 侧已有 `ReconnectPolicy::max_attempts`（`ingest/config.rs`）能把"重试耗尽"变成 `Failed` 终态，缺的只是把这个结论**推给调用方**的手段。

## Goals / Non-Goals

**Goals:**

- 终态判定只有一处权威来源：ingest 侧。UI 侧不再持有任何"宽限窗"概念。
- 调用方有两种等终态的方式，二选一：注册回调（`on_terminal`）或阻塞等待（`wait_for_terminal`）。
- 终态结论自带失败原因快照，调用方不必在回调里再读一次 `stats()`（存在竞态：回调触发后句柄可能已被 `stop()` 改写）。
- `call-ui` 监控页删除看门狗线程与魔法超时常数。

**Non-Goals:**

- 不改 `ReconnectPolicy` 的语义与退避算法。
- 不改 `start()` / `start_with_taps()` 的签名，不改 `StreamState` 状态机本身的状态集合。
- 不做 WebRTC 侧（`rtp_send`）的同类改造，本次只覆盖 ingest 拉流路径。
- 不引入新的依赖或 feature flag。

## Decisions

### 决策 1: 用新增类型 `StreamTerminal` 而非复用 `StreamState`

`StreamState` 有 5 个变体，其中 `Failed` / `Ended` 是终态，但 `StreamState` 不携带失败原因。终态通知要带 `last_error`，所以另立类型：

```rust
pub enum StreamTerminal {
    Failed { error: Option<String> },
    Ended,
}
```

**备选**：直接用 `StreamState` + 调用方另读 `stats().last_error`。否决 —— 回调触发后句柄状态可能被 `stop()` 抢先改写，调用方读到的 `last_error` 与这次终态不一定对应；把原因冻结进终态值里更可靠。

**备选**：把 `Failed` 改成 `Failed { error: Option<String> }`。否决 —— `StreamState` 是状态机状态，携带错误详情会让 `can_transition` 之类的纯逻辑被 payload 污染；两者关注点不同。

### 决策 2: 回调放 `GstStreamConfig` 而不放 `GstStreamHandle`

`on_terminal` 是**会话启动时**就要知道的订阅意图（调用方在 `start()` 之前还不知道句柄），所以放配置里。

**备选**：`GstStreamHandle::on_terminal(cb)` 启动后再注册。否决 —— 会话线程可能在注册之前就已经到终态（极短-lived 的失败），存在丢通知的窗口；注册在配置里保证"会话一启动订阅就在"。

回调签名取 `Option<Arc<dyn Fn(StreamTerminal) + Send + Sync>>`：需要 `Send + Sync` 是因为回调会在 ingest 的会话线程上触发，而闭包里常要捕获 `slint::Weak<MainWindow>`（`Weak` 是 `Send + Sync`，但 `upgrade` 只能在 UI 线程有效，所以调用方仍需 `invoke_from_event_loop` 摆渡）。

### 决策 3: `GstStreamConfig` 的 `PartialEq` 派生如何处理回调字段

`GstStreamConfig` 目前派生 `PartialEq, Eq`，`Arc<dyn Fn...>` 不满足 `Eq`。处理方式：给回调字段包一层 newtype 并手工实现 `PartialEq`（按"两个回调字段是否都存在"比较，不比较闭包内容），或直接去掉 `GstStreamConfig` 的 `PartialEq` 派生。

**采用**：保留 `PartialEq` 但改为手工实现，回调字段按 `Option` 的 `is_some()` 比较。理由是 `Eq`/`PartialEq` 在测试里比较配置时有用（如 `validate` 的边界测试），直接删派生会牵连现有测试。

**备选**：去掉 `PartialEq` 派生。否决 —— 牵连面比手工实现大，且现有测试没用它比较含回调的配置（已核实：仓库内无 `assert_eq!` 比较 `GstStreamConfig` 的地方），但保留成本更低。

### 决策 4: `wait_for_terminal` 与 `on_terminal` 并存

两者语义等价（都拿终态），但适用场景不同：

- `on_terminal`：回调式，调用方不需要额外线程，适合 GUI（回调里 `invoke_from_event_loop`）。
- `wait_for_terminal`：阻塞式，适合已经在异步任务里等待的调用方（`web-display/src/stream.rs` 现在是 200ms 轮询 `state()`，可换成它消除轮询）。

`wait_for_terminal` 带 timeout 参数返回 `Option`，超时返回 `None` 而非 panic —— 它是"尽力等待"的辅助手段，不是权威判定；权威判定始终是 ingest 自己的状态机。

### 决策 5: 终态只触发一次

回调在会话到达终态时**恰好触发一次**。`Failed` 与 `Ended` 都算终态；`Failed` 之后 `stop()` 不改状态（既有 `stop()` 的幂等语义），`Ended` 之后不再有回调。

实现上由 ingest 的会话循环在退出前统一触发一次，而不是在 `transition()` 里挂 —— `transition()` 是纯状态转移，不该有副作用（回调要调用户代码，可能阻塞）。

### 决策 6: `call-ui` 侧 `max_attempts = Some(3)`

监控页要的是"快速给用户反馈"，不是"无限自愈"（无限自愈是 `web-display` 转推场景的需求）。3 次重试 = 退避 1s→2s→4s ≈ 7s 后放弃，与旧的 5s 宽限窗量级相当但语义正确。

出流成功后 `attempt` 归零（`ingest/bus.rs` 已有此逻辑），之后的网络抖动照样自愈，`max_attempts` 只约束"从连接中一路失败到出流"这一段。

## Risks / Trade-offs

- [回调在 ingest 会话线程上触发，可能阻塞该线程] → 文档明确"回调应尽快返回"；`call-ui` 的实现只做一次 `invoke_from_event_loop` 投递即返回。
- [回调在 `stop()` 与失败竞态时可能不触发或触发顺序异常] → `Failed`/`Ended` 互斥且各只触发一次；`call-ui` 侧用 `monitor-selected-id` 代次校验丢弃过期回调。
- [手工实现 `PartialEq` 后配置比较语义变粗（不比较闭包内容）] → 现有测试无依赖闭包内容比较；`validate()` 不看回调字段。
- [`web-display` 改用 `wait_for_terminal` 后行为变化] → 阻塞等待在 tokio 任务里会占用 worker 线程；本次 change 不强制改造 `web-display`，保持现状（轮询）不动。

## Migration Plan

1. `ipcam-gst` 先加类型与回调字段（纯新增，不改既有行为）。
2. `call-ui` 切换到回调，删除看门狗线程。
3. 回归：监控页选中不可达摄像头 → 约 7s 后弹 toast + 复位选中高亮 + 清画面；选中可达摄像头 → 正常出流。

回滚：`on_terminal` 字段为 `Option`，不设置即保持旧行为；`call-ui` 改动可独立回退。

## Open Questions

- `web-display` 是否在本 change 后一并改用 `wait_for_terminal`？倾向留作独立 change（涉及 tokio 阻塞策略）。
