## ADDED Requirements

### Requirement: HTTP 控制 API
系统 SHALL 在 `/api/*` 路径下提供返回 JSON 的 HTTP API，至少包含以下端点：
- `GET /api/devices`：列出已发现的摄像头及其 profile。
- `GET /api/sessions`：列出当前活跃的播放会话及其状态。
- `POST /api/sessions`，请求体 `{device_id, profile_id}`：创建一个新的播放会话，返回 `{session_id}`。

#### Scenario: 创建会话
- **WHEN** 客户端向 `/api/sessions` 提交 `{device_id, profile_id}`，且该 device / profile 有效
- **THEN** 服务端 SHALL 返回 `201 Created`，响应体为 `{"session_id":"..."}`，且系统 SHALL 开始向匹配的 WebSocket 推送视频数据。

### Requirement: 基于 WebSocket 的 fMP4 切片推流
对每个会话，系统 SHALL 在 `/ws/{session_id}` 上接受 WebSocket 升级请求，并通过该连接向客户端推送 fragmented MP4 片段（`ftyp` + `moof` + `mdat`）以及一个 init segment。

#### Scenario: 浏览器播放流
- **WHEN** 浏览器加入会话并将字节送入 `MediaSource` 的 `SourceBuffer('video/mp4; codecs="avc1.42E01E"')`
- **THEN** 浏览器 SHALL 在收到首个 GOP 后 1 秒内开始播放，并对 1080p@25 源 SHALL 持续播放 ≥ 10 分钟不出现卡顿。

### Requirement: 会话生命周期
每个会话 SHALL 维护 `state ∈ {creating, ready, stalled, ended}`。当解码帧的 wall-clock 间隔超过 5 秒时，会话 SHALL 转入 `stalled`。服务端在清理会话时 SHALL 在 200 毫秒内释放其解码器 buffer。

#### Scenario: 摄像头掉线
- **WHEN** RTSP 重连逻辑连续失败 3 次
- **THEN** 会话 SHALL 转入 `stalled` 并 SHALL 保留在列表中，直到 UI 操作将其移除。

### Requirement: 静态首页
系统 SHALL 在 `/` 路径下提供静态 HTML/JS 页面，列出已发现的设备，允许运维点击查看，并通过 `MediaSource` 消费每个会话的 WebSocket 数据。

#### Scenario: 首屏渲染
- **WHEN** 浏览器访问 `http://<板子地址>:8080/`
- **THEN** 页面 SHALL 在 500 毫秒内列出所有已发现的摄像头，并 SHALL 暴露点击处理器，能进入单摄像头的播放视图。

## Out of scope (v1)

基于 WebRTC 的亚秒级低延迟传输在本 change 中**显式不属于**强制要求；仅本节前述的 fMP4 over WebSocket 路径属于 v1 范围。