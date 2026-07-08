# ipcam-discovery Specification

## Purpose
TBD - created by archiving change ip-camera-media-streaming. Update Purpose after archive.
## Requirements
### Requirement: ONVIF WS-Discovery 主动探测
系统 SHALL 在每一个已配置的网络接口上，主动向 UDP 组播地址 `239.255.255.250:3702` 发送 ONVIF `Probe` 报文，并 SHALL 解析 `ProbeMatches`（以及 `Hello`/`Bye` 通知报文），从中获取每个摄像头的 `XAddr`、`ReferenceToken` 与 `Types`。

#### Scenario: 至少一台摄像头响应
- **WHEN** 应用在至少一台 ONVIF 兼容的 IP 摄像头可达的局域网上启动
- **THEN** 在可配置的超时时间（默认 5 秒）内，系统 SHALL 至少产生一条包含 `xaddr`、`address`、`types`、`scopes` 字段的发现记录。

#### Scenario: 无摄像头响应
- **WHEN** 应用启动所在网络内没有 ONVIF 兼容设备
- **THEN** 系统 SHALL 返回空的发现列表且不发生 panic，并 SHALL 输出诊断日志。

### Requirement: ONVIF 设备鉴权
系统 SHALL 按 ONVIF Device Management 规范为每个会话申请 WS-Security UsernameToken（nonce + created + password digest），并 SHALL 在每一次需要鉴权的 `GetCapabilities` / `GetProfiles` / `GetStreamUri` 调用中携带该 token。

#### Scenario: 已提供凭据时枚举 profile
- **WHEN** ONVIF 端点需要鉴权且运维已配置用户名/密码
- **THEN** 系统 SHALL 在收到凭据后 3 秒内枚举出至少一个 media profile，并将该 profile 的 RTSP URI 暴露到上层。

#### Scenario: 凭据缺失或错误
- **WHEN** 设备拒绝当前的鉴权 token
- **THEN** 系统 SHALL 将该设备的 `auth_status` 标记为 `invalid_credentials`，且 SHALL 不崩溃；设备仍出现在发现列表中，但其 RTSP URI 字段 SHALL 保持为 `None`。

### Requirement: 获取 profile 与 RTSP URI
对每一个被发现的摄像头，系统 SHALL 顺序调用 ONVIF `GetCapabilities` → `media.GetProfiles` → `GetStreamUri`，并 SHALL 为每个 profile 持久化 `(profile, media_uri, codec, resolution, frame_rate)` 元数据。

#### Scenario: 单摄像头多 profile
- **WHEN** 某摄像头报告 N ≥ 2 个 profile
- **THEN** 系统 SHALL 在该设备的 `profiles` 列表中输出 N 条记录，每条 SHALL 至少携带 URI 或 codec 之一的差异标识。

### Requirement: 发现结果 API
系统 SHALL 通过进程内的 `Discovery` trait 暴露已发现设备与其 profile 的合集，并 SHALL 同时提供 HTTP 端点 `GET /api/devices` 返回 JSON 数组 `Device[]`。

#### Scenario: HTTP 客户端拉取设备列表
- **WHEN** 浏览器调用 `GET /api/devices`
- **THEN** 响应体 SHALL 为 JSON 数组，至少包含 `id`、`address`、`profiles[]` 字段；每个 profile SHALL 至少包含 `uri`、`codec`、`width`、`height`、`fps` 字段。

