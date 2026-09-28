## ADDED Requirements

### Requirement: RTP 收包统计

`RtpReceiver` SHALL 在内部维护 video/audio 包计数器，并在固定间隔内打印收包进度日志，格式为 `rtp receiver stats video_pkts=X audio_pkts=Y`。

#### Scenario: 正常接收时打印统计
- **WHEN** RTP 接收器正在接收音视频 RTP 包
- **THEN** 每 2 秒打印一次收包统计日志

#### Scenario: 未收到任何包时打印零值
- **WHEN** RTP 接收器已启动但未收到任何 RTP 包
- **THEN** 仍然打印零值的统计日志 `video_pkts=0 audio_pkts=0`

#### Scenario: stop 后停止统计日志
- **WHEN** 调用方调用 `stop()` 或 `RtpReceiver` 被 drop
- **THEN** 统计日志任务立即停止，不再产生新的日志输出

### Requirement: 调用方可查询当前收包数

`RtpReceiver` SHALL 提供 `packet_counts()` 方法返回当前累计的 `(video_pkts, audio_pkts)` 元组，供调用方在通话结束后做最终统计或告警。

#### Scenario: 返回当前计数器值
- **WHEN** 调用方调用 `receiver.packet_counts()`
- **THEN** 返回自接收器启动以来的累计 video 包数和 audio 包数
