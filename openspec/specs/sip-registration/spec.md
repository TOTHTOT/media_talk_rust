## ADDED Requirements

### Requirement: 客户端状态可观测
`SipClient` SHALL 通过 `status()` 暴露客户端状态快照（`ClientStatus`）的可订阅视图，其中 `registered` 表示"当前是否已向 SIP 服务器注册成功"。新增其他状态时 MUST 以新增字段的方式扩展快照，而不是更换通道。

#### Scenario: 初始状态为未注册
- **WHEN** 调用方在 `SipClient::new` 之后立即读取 `status()` 的当前值
- **THEN** `registered` MUST 为 `false`
- **AND** 在首次注册成功之前 MUST NOT 广播 `registered = true`

#### Scenario: 多调用方订阅同一状态
- **WHEN** 两个调用方分别 clone `status()` 返回的 `watch::Receiver`
- **THEN** 注册成功时两个订阅者 MUST 都能观察到 `registered` 变为 `true`

#### Scenario: 晚订阅者读取当前值
- **WHEN** 注册成功后，调用方才获取 `status()`
- **THEN** 当前快照的 `registered` MUST 已是 `true`，无需等待下一次状态变化

### Requirement: 注册成功时广播在线状态
`process_register()` 每完成一轮成功注册（含首次注册与续期）时，SHALL 将快照的 `registered` 置为 `true`。

#### Scenario: 首次注册成功
- **WHEN** 服务器对 REGISTER 返回 200 OK
- **THEN** `registered` MUST 变为 `true`
- **AND** 已订阅的调用方 MUST 收到变化通知

#### Scenario: 续期成功
- **WHEN** 已注册状态下一次续期 REGISTER 返回 200 OK
- **THEN** `registered` MUST 保持 `true`，且 MUST NOT 重复唤醒订阅者

### Requirement: 永久失败时广播离线状态
`process_register()` 因 401/403/404 等永久失败而返回 `Err` 之前，SHALL 将快照的 `registered` 置为 `false`。

#### Scenario: 首次注册即失败
- **WHEN** 服务器对 REGISTER 返回 403 Forbidden
- **THEN** `process_register()` MUST 返回 `Err`
- **AND** `registered` MUST 为 `false`

#### Scenario: 续期时凭据失效
- **WHEN** 已注册状态下续期 REGISTER 返回 401 Unauthorized
- **THEN** `process_register()` MUST 返回 `Err`
- **AND** `registered` MUST 变为 `false`

### Requirement: 注册状态驱动 GUI 显示
`call-ui` worker SHALL 订阅注册状态，并映射为 UI 事件：`true` → `UiEvent::Registered`，`false` → `UiEvent::RegisterFailed`。

#### Scenario: 注册成功刷新在线状态
- **WHEN** worker 观察到注册状态变为 `true`
- **THEN** MUST 投递 `UiEvent::Registered`
- **AND** 状态机 MUST 将 `AppState.registered` 置为 `true`

#### Scenario: 注册永久失败刷新离线状态
- **WHEN** worker 观察到注册状态变为 `false`
- **THEN** MUST 投递 `UiEvent::RegisterFailed`
- **AND** 状态机 MUST 将 `AppState.registered` 置为 `false`

### Requirement: 不改变现有注册循环的返回值语义
`process_register()` SHALL 继续保持原有返回语义：仅在注册循环终止时返回，成功路径不返回。

#### Scenario: 成功路径不结束循环
- **WHEN** 注册成功且未收到取消信号
- **THEN** `process_register()` MUST NOT 返回
- **AND** MUST 在续期点继续下一轮注册
