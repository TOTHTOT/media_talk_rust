## Context

`ipcam-sip::SipClient::process_register()` 是注册/续期/注销的常驻循环：

- 每轮 `register_cycle` 发一次 REGISTER，按响应分类为 `Cycle::Registered` 或 `Cycle::Retry`
- 401/403/404 视为永久失败，`return Err(e)` 结束循环
- 成功与重试只在内部 `debug!` 记录，调用方看不到中间状态

`call-ui` worker 把 `process_register()` 的 future 放进 `tokio::select!`，只有 future 结束（即永久失败）时才能 post `UiEvent::RegisterFailed`。注册成功的时刻没有任何信号到达 UI，因此 `AppState.registered` 永远为 `false`。

调用方现状：
- `call-ui`（GUI）：需要注册成功/失效来更新状态栏与主页
- `ipcam-sip/examples/*`：命令行示例，不需要状态广播
- web-display IPC 路径：不使用注册状态

## Goals / Non-Goals

**Goals:**

- `ipcam-sip` 向外广播注册状态变化（已注册 / 失效），且不改变 `process_register()` 现有返回语义
- GUI 在首次注册成功时立即显示在线，永久失败/掉线时回到离线
- 新增接口非破坏性，现有调用方无需改动即可编译

**Non-Goals:**

- 不实现更细粒度的注册事件（如每轮重试次数、剩余有效期）
- 不改变 401/403/404 的永久失败判定逻辑
- 不处理"续期失败但尚未过期"的中间态（仍按永久失败处理，与现状一致）
- 不改动 web-display / IPC 的注册相关行为

## Decisions

### 决策 1：用 `tokio::sync::watch` 广播状态快照，而不是回调或事件总线

- **watch**：`Sender<ClientStatus>` 存在 `SipClient` 内，`Receiver` 由调用方按需 clone；内部就是 `Arc` 共享，后订阅者能立即读到当前快照，天然多消费者，且与本仓库已有的 tokio 异步模型一致
- **快照而非单一 bool**：后续要同步传输状态、网络状态等只需给 `ClientStatus` 加字段，通道形态和订阅代码不变；事件（来电/挂断）仍走 dialog state 通道，不混入状态快照
- **备选：`Arc<AtomicBool>` 轮询**：需要调用方自己定时轮询，没有唤醒，延迟不可控
- **备选：闭包回调** `process_register(on_change: impl Fn(bool))`：需要给每个调用点补参数，且回调在库的异步上下文里执行，调用方若阻塞会卡住注册循环
- **备选：`tokio::sync::mpsc` / `broadcast`**：没有"当前值"语义，晚订阅者会错过首次注册成功；broadcast 还会把状态变化当成事件消费

结论：`watch` + 状态快照最贴合"可查询的当前状态"语义，改动最小且可扩展。

### 决策 2：状态在 `SipClient` 内维护，由 `register_cycle` 的成功分支写入

- `SipClient::new` 创建 `watch::channel(ClientStatus::default())`
- `register_cycle` 返回 `Cycle::Registered` 时 `set_registered(true)`
- 永久失败 `return Err` 前 `set_registered(false)`
- 写入用 `send_if_modified`：值未变化（首次后的每次续期）不唤醒订阅者，避免无意义刷新
- 关闭/注销路径（`cancel` 后）不改状态，避免窗口关闭过程中把"已注册"翻成"离线"造成 UI 闪动

### 决策 3：把 `changed()` 并进 worker 已有的 `select!`，不另起任务

worker 主循环本来就在 `select!` 上跑，状态快照只是多一个分支；后续新增状态字段都在同一个分支里派生：

```rust
let mut client_status = client.status();
let mut status_open = true;
loop {
    tokio::select! {
        r = client_status.changed(), if status_open => match r {
            Ok(()) => {
                if client_status.borrow_and_update().registered {
                    ui.post(UiEvent::Registered);
                }
            }
            // sender 随 client drop 时退出该分支, 避免 Err 立即重试空转
            Err(_) => status_open = false,
        }
        // ... 原有 reg / incoming / state_loop / cmd 分支
    }
}
```

- 不额外 `spawn`: 少一个任务、少一次 `UiHandle` clone; 唤醒由 select 本身处理
- 用 `borrow_and_update` 标记已读，避免 `changed()` 立即再次触发
- **永久失败仍由 worker 主循环投递**（保留 `process_register` 返回的 401/403 detail），watch 分支只映射 `true`。若这里也映射 `false`，会与主循环的通知重复且丢失 detail —— 单一来源原则在这里让位于"错误原因必须可见"

## Risks / Trade-offs

- **watch 初始值为 `false`，订阅瞬间不会触发 `changed()`** → GUI 初始离线状态与现状一致；首次注册成功后广播才翻转，符合需求
- **永久失败时通知重复**（主循环 + watch 各一次）→ 实现时二选一：主循环保留 `UiEvent::RegisterFailed`（带 detail），watch 只负责 `Registered`；或统一由 watch 负责并让主循环只打印日志。倾向后者，保持事件单一来源
- **`send_replace` 在无接收者时不报错** → 无调用方订阅时行为与现状一致，无额外开销
- **注册状态与真实网络可用性存在窗口**（续期间隙、NAT 映射失效）→ 沿用服务器 REGISTER 响应作为唯一事实来源，不额外探测

## Migration Plan

1. `ipcam-sip` 新增字段与访问器，`register_cycle` 写入状态；现有示例无需改动
2. `call-ui` worker 接入订阅任务；删除主循环里对永久失败的重复 post（如采用单一来源）
3. 回滚：删除新增字段/访问器、还原 worker 的 select 分支即可，不涉及数据迁移

## Open Questions

- 续期永久失败（`registered == true` 之后的 401/403）当前会直接返回 `Err` 并跳过 `Expires: 0` 注销，服务器绑定残留到自然过期。此行为不在本 change 范围内，是否需要单独提案修复？
