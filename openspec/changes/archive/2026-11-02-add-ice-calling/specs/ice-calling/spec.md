## ADDED Requirements

### Requirement: ICE 会话建立
`ipcam-gst` SHALL 提供基于 libnice 的 ICE 会话能力. 呼叫方 gather 完成 SHALL 产出本地 blob (非 trickle, 一次性全量候选与凭据); 对端 blob 入库 SHALL 触发连通性检查; 状态到达 `Connected` / `Ready` SHALL 表示媒体通路可用. 两端 MUST 各跑一个本程序实例, 会话 MUST NOT 依赖 SIP 链路.

#### Scenario: gather 完成产出本地 blob
- **WHEN** 一方发起呼叫且 ICE gather 完成
- **THEN** `local_blob` MUST 返回包含候选地址与凭据的非空 blob

#### Scenario: 对端 blob 入库触发连通性检查
- **WHEN** 一方收到对端 blob 并成功 `add_remote`
- **THEN** agent MUST 进入连通性检查
- **AND** 状态变化 MUST 经 watch 通道推送给调用方

#### Scenario: 打洞失败可见
- **WHEN** 双方候选无法配对 (如双方对称 NAT, 无 TURN)
- **THEN** 状态 SHALL 到达 `Failed`
- **AND** MUST NOT 假报连通

### Requirement: 双通道信令 (WS sigserver / 手动 blob)
信令 SHALL 支持两条通道交换 blob: 局域网经 WS sigserver 自动转发 (join / offer / answer / hangup, 房间两两配对, 满员拒绝; hangup 转发保证仅适用信令建立期内); 跨网 sigserver 不可达时 SHALL 退化为界面导出 / 导入 blob 手动搬运. 手动通道 MUST 与 WS 通道共用同一份序列化代码.

#### Scenario: WS 通道自动交换
- **WHEN** 两端加入 sigserver 同一房间
- **THEN** offer / answer blob MUST 原样互转
- **AND** 第三人进房 MUST 收到 `room full` 错误

#### Scenario: 手动通道搬运
- **WHEN** sigserver 不可达且手动模式开启
- **THEN** 界面 SHALL 支持导出本地 blob / 导入对端 blob
- **AND** 导入的 blob MUST 走与 WS 通道相同的解析代码

#### Scenario: 挂断通知对端
- **WHEN** 任一端在信令建立期内挂断 (WS 信令连接存活)
- **THEN** 对端 MUST 收到 hangup
- **AND** 双方会话资源 MUST 回收; 媒体已开始后对端挂断 SHALL 经 ICE 状态变化本地呈现为 "ICE 连接中断" (通话期 hangup 通知通道列为后续项)

### Requirement: 媒体经 nicesink / nicesrc 收发
媒体 SHALL 经 ICE 选定通路收发: 发送走 nicesink, 接收走 nicesrc. 视频 SHALL 为 H264 pt 96 clock 90000 (component 1), 音频 SHALL 为 Opus pt 111 clock 48000 双通道 (component 2). 编码参数 MUST 由两端程序直接约定, MUST NOT 依赖 SDP offer-answer 协商.

#### Scenario: 连通后双向收发
- **WHEN** ICE 状态可达且两端媒体管线已启动
- **THEN** 视频与音频数据 SHALL 在选定通路上双向流动

#### Scenario: 编码参数固定
- **WHEN** 构造 ICE 媒体管线
- **THEN** 视频 MUST 配置为 pt 96, 音频 MUST 配置为 pt 111
- **AND** MUST NOT 发起协商

#### Scenario: 挂断清理媒体通路
- **WHEN** 通话挂断
- **THEN** pipeline MUST 先置 `Null` 再 close agent
- **AND** MUST NOT 残留收发

### Requirement: 状态与字节统计可观测
通话中 SHALL 在 UI 状态行展示 ICE 状态与收发字节. 字节统计 SHALL 来自 nicesink 前 / nicesrc 后的 pad probe, 状态 SHALL 来自 agent 状态通道.

#### Scenario: 状态行实时显示
- **WHEN** 呼叫进行中
- **THEN** UI 状态行 MUST 显示当前 ICE 状态

#### Scenario: 字节统计随媒体增长
- **WHEN** 媒体数据在通路上流动
- **THEN** 收发字节计数 MUST 单调增长并显示

#### Scenario: 挂断后复位
- **WHEN** 通话结束
- **THEN** 状态行与字节统计 MUST 复位
