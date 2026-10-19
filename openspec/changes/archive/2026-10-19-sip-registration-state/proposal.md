## Why

call-ui 的注册状态始终显示为未注册：`ipcam-sip::SipClient::process_register()` 是常驻循环，首次注册成功与每次续期成功都只在内部 `Cycle::Registered` 分支打一条 `debug!` 日志；调用方只能拿到永久失败（401/403/404）时的 `Err`，无法感知注册成功，也无法感知续期失败后的掉线。因此 `call-ui` 中 `UiEvent::Registered` 只有定义和单测引用，从未被投递，`AppState.registered` 永远是 `false`，状态栏一直显示 phone-off，主页"通话服务"状态也不可信。

## What Changes

- **新增** `ipcam-sip` 客户端状态快照：`ClientStatus { registered: bool }`，由 `SipClient` 内部的 `tokio::sync::watch::Sender<ClientStatus>` 广播；后续新增状态只加字段，不换通道形态
- **新增** 公开只读接口 `SipClient::status(&self) -> watch::Receiver<ClientStatus>`（非破坏性新增，不改现有方法签名）
- **修改** `call-ui` worker：`setup` 拿到 client 后在已有的 `select!` 中订阅状态变化，`registered` 为 true 时投递 `UiEvent::Registered`；由状态机按现有规则刷新 `registered`
- **保持** `process_register()` 的返回值语义不变：仍然只作为"注册循环已终止"的信号使用
- 不新增依赖（`tokio::sync::watch` 随现有 `tokio` 已可用），无 **BREAKING** 变更

## Capabilities

### New Capabilities
- `sip-registration`: SIP 注册状态的可观测性 —— 库向外广播"已注册 / 注册失效"，调用方（GUI、IPC）据此展示在线状态

### Modified Capabilities
<!-- 无现有 spec 的需求变更 -->

## Impact

- `crates/ipcam-sip/src/client.rs`：新增 `ClientStatus` 快照与 `SipClient.status` watch 发送端；`register_cycle` 在 `Cycle::Registered` 时置 `registered = true`，永久失败返回前置 `false`；新增 `status()` 访问器
- `crates/call-ui/src/session/worker.rs`：`select!` 增加状态变化分支，`registered` 为 true 时 post `UiEvent::Registered`；永久失败仍由注册循环返回值分支 post `RegisterFailed`
- `crates/call-ui/src/app.rs`：无需改动（`UiEvent::Registered` 与 `registered` 字段已存在）
- 其他调用方（`ipcam-sip/examples/*`、web-display 的 IPC 路径）不受影响，可选接入
