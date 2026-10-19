## 1. ipcam-sip 状态快照广播

- [x] 1.1 新增 `ClientStatus { registered: bool }`，`SipClient` 增加 `status: watch::Sender<ClientStatus>` 字段，`new` 中初始化为默认快照
- [x] 1.2 新增 `pub fn status(&self) -> watch::Receiver<ClientStatus>`
- [x] 1.3 `register_cycle` 在 `Cycle::Registered` 分支 `set_registered(true)`
- [x] 1.4 `process_register` 永久失败 `return Err` 前 `set_registered(false)`
- [x] 1.5 `set_registered` 用 `send_if_modified`，值未变化时（续期）不唤醒订阅者

## 2. ipcam-sip 单元测试

- [x] 2.1 测试初始快照 `registered == false`
- [x] 2.2 测试注册成功后晚订阅者读到 `registered == true`
- [x] 2.3 测试多个 `Receiver` 同时收到变化

## 3. call-ui 接入

- [x] 3.1 worker `setup` 后订阅 `client.status()`
- [x] 3.2 `select!` 增加 `client_status.changed()` 分支（不另起任务）：`registered` 为 true 时 post `UiEvent::Registered`
- [x] 3.3 永久失败仍由主循环 `RegisterFailed` 投递（保留 401/403 detail），状态分支不重复映射 `false`（见 design 决策 3）

## 4. 验证

- [x] 4.1 `cargo fmt --all`
- [x] 4.2 `cargo clippy --workspace --all-targets --features sw-decode -- -D warnings`
- [x] 4.3 `cargo test --workspace --features sw-decode`
- [ ] 4.4 手工验证：连上 SIP 服务器后状态栏 phone 图标变绿、主页"通话服务"显示在线；服务器拒绝时回到离线并显示错误（需真实服务器/设备）
