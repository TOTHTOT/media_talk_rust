## ADDED Requirements

### Requirement: 会话终态可被调用方观测
`ipcam-gst` SHALL 在一路 RTSP 会话到达终态（不可恢复失败 `Failed` / 用户主动停止 `Ended`）时，向调用方提供该终态结论。调用方 MUST NOT 需要自行轮询 `GstStreamHandle::state()` 才能得知失败。终态结论 SHALL 以新增类型 `StreamTerminal` 表达，`StreamTerminal` MUST 携带失败原因快照（当终态为失败时）。

#### Scenario: 失败终态携带错误原文
- **WHEN** 一路会话因认证失败或重试耗尽到达失败终态
- **THEN** 调用方 MUST 收到表示失败的终态结论
- **AND** 该结论 MUST 包含流累计的最近错误原文（若存在）

#### Scenario: 主动停止是终态但不携带错误
- **WHEN** 调用方主动 `stop()` 一路会话
- **THEN** 调用方 MUST 收到表示结束的终态结论
- **AND** 该结论 MUST NOT 携带错误原文

#### Scenario: 终态只判定一次
- **WHEN** 一路会话已经到达终态
- **THEN** 调用方 MUST NOT 再收到第二个终态结论

### Requirement: 终态通知支持回调与阻塞等待两种方式
`ipcam-gst` SHALL 同时提供两种等终态的手段，二者语义等价：配置级回调 `GstStreamConfig::on_terminal`，以及句柄上的阻塞方法 `GstStreamHandle::wait_for_terminal`。未注册回调的调用方 MUST 仍能通过阻塞方法得知终态。

#### Scenario: 注册回调后收到终态通知
- **WHEN** 调用方在 `GstStreamConfig` 中设置了终态回调
- **THEN** 会话到达终态时该回调 MUST 被调用一次
- **AND** 回调参数 MUST 是该次终态结论

#### Scenario: 阻塞等待返回终态
- **WHEN** 调用方调用 `wait_for_terminal` 并在其返回前会话到达终态
- **THEN** MUST 返回表示该终态的结论

#### Scenario: 阻塞等待超时
- **WHEN** 调用方调用 `wait_for_terminal` 且在超时前会话仍未到达终态
- **THEN** MUST 返回表示"未等到终态"的值
- **AND** MUST NOT 改变会话状态

### Requirement: 失败时限由 ingest 侧重连策略决定
"多久算失败" SHALL 只有一处权威标准：ingest 侧 `ReconnectPolicy::max_attempts`。调用方 MUST NOT 自行引入宽限窗之类的失败时限常数。`max_attempts` 为有限值时，重试耗尽 SHALL 转为失败终态。

#### Scenario: 重试耗尽转为失败终态
- **WHEN** 一路会话的配置指定了有限的 `max_attempts` 且连续重连全部失败
- **THEN** 会话 MUST 转为失败终态
- **AND** 调用方 MUST 收到该终态结论

#### Scenario: 出流成功后重试计数归零
- **WHEN** 一路会话在重连过程中成功出流
- **THEN** 重试计数 MUST 归零
- **AND** 之后的网络抖动 MUST 仍能按同一策略自愈

#### Scenario: 认证失败不重试
- **WHEN** 一路会话遇到 401/Unauthorized 类认证错误
- **THEN** 会话 MUST 直接转为失败终态
- **AND** MUST NOT 进入重连循环

### Requirement: GUI 侧不再持有失败时限
GUI 调用方 SHALL 不再为等终态而维护看门狗线程、轮询间隔或宽限窗常数。终态结论抵达后，GUI SHALL 复位选中态并按既有通知管线发出失败提示。

#### Scenario: 连接失败时提示并复位选中
- **WHEN** 监控页选中的摄像头在会话终态为失败
- **THEN** GUI MUST 复位选中高亮
- **AND** MUST 清除残留画面
- **AND** MUST 发出摄像头连接失败提示

#### Scenario: 用户主动停止不提示
- **WHEN** 监控页选中的摄像头因用户切台 / 离开页面 / 通话抢占而终止
- **THEN** GUI MUST NOT 发出失败提示
