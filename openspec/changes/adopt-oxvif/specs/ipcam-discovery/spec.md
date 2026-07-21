## MODIFIED Requirements

### Requirement: ONVIF WS-Discovery 主动探测
系统 SHALL 在所有非 loopback IPv4 网络接口上,通过 `oxvif::discovery::probe()`(版本 0.12.0,严格 pin)主动向 UDP 组播地址 `239.255.255.250:3702` 发送 ONVIF `Probe` 报文,并 SHALL 在可配置的超时时间(默认 5 秒)内解析所有 `ProbeMatches` 响应,从每条响应中提取 `XAddr`、`Types` 与 `Scopes`。Scan 顺序、bound interface 列表、socket 数由 oxvif 内部决定(原项目 6-socket 每接口绑定逻辑不再适用)。

#### Scenario: 至少一台摄像头响应
- **WHEN** 应用在至少一台 ONVIF 兼容的 IP 摄像头可达的局域网上启动,设置 `MEDIA_TALK_DISCOVERY_BACKEND=oxvif`(默认)
- **THEN** 在可配置的超时时间(默认 5 秒)内,系统 SHALL 至少产生一条包含 `xaddr`、`address`、`types`、`scopes` 字段的发现记录

#### Scenario: 无摄像头响应
- **WHEN** 应用启动所在网络内没有 ONVIF 兼容设备
- **THEN** 系统 SHALL 返回空的发现列表且不发生 panic,并 SHALL 输出诊断日志

#### Scenario: 通过 env 切回 legacy backend
- **WHEN** 板端运维设置 `MEDIA_TALK_DISCOVERY_BACKEND=legacy` 后重启 media_talk 服务
- **THEN** 系统 MUST 走 `crates/ipcam-discovery/src/_legacy/` 内的旧实现,绑定每个非 loopback IPv4 接口的独立 UDP socket(原 6-socket 行为),与原项目 `ipcam-discovery` 0.1.0 行为字节对齐

### Requirement: ONVIF 设备鉴权
系统 SHALL 使用 oxvif 内置的 WS-Security UsernameToken + HTTP Digest Auth(版本 0.12.0 行为)。`OnvifSession::builder(xaddr).with_credentials(u, p).build()` 一次 `GetCapabilities` + 缓存后续调用。

#### Scenario: 已提供凭据时枚举 profile
- **WHEN** ONVIF 端点需要鉴权且运维已配置用户名/密码
- **THEN** 系统 SHALL 在收到凭据后 3 秒内枚举出至少一个 media profile,并将该 profile 的 RTSP URI 暴露到上层

#### Scenario: 凭据缺失或错误
- **WHEN** 设备拒绝当前的鉴权 token
- **THEN** 系统 SHALL 将该设备的 `auth_status` 标记为 `invalid_credentials`,且 SHALL 不崩溃;设备仍出现在发现列表中,但其 RTSP URI 字段 SHALL 保持为 `None`

#### Scenario: 区分鉴权失败与其他网络错误
- **WHEN** oxvif 报告任何 SOAP fault 含 `"authenticated or authorized"` / `"NotAuthorized"` / `HTTP 401` 子串
- **THEN** `auth_status` MUST 是 `invalid_credentials`;其它错误(HTTP 502、超时、refused 等)MUST 归为 `anonymous`

### Requirement: 获取 profile 与 RTSP URI
对每一个被发现的摄像头,系统 SHALL 通过 `OnvifSession::get_profiles()` 与 `OnvifSession::get_stream_uri(profile_token)` 顺序获取 profiles 与对应 RTSP URI,并 SHALL 为每个 profile 持久化 `(profile_id, media_uri)` 元数据。`codec` / `width` / `height` / `fps` 字段保留但 SHALL 默认为 `Unknown` / 0 / 0 / 0.0,原因是 oxvif `MediaProfile` 不携带这些字段;后续若有性能需求可开新 change 加 `GetVideoEncoderConfiguration` 二次调用拉全字段。

#### Scenario: 单摄像头多 profile
- **WHEN** 某摄像头报告 N ≥ 2 个 profile
- **THEN** 系统 SHALL 在该设备的 `profiles` 列表中输出 N 条记录,每条 SHALL 至少携带 `profile_id` 与(可能存在的)`uri`

### Requirement: 发现结果 API
系统 SHALL 通过进程内的 `Discovery` trait 暴露已发现设备与其 profile 的合集,并 SHALL 同时提供 HTTP 端点 `GET /api/devices` 返回 JSON 数组 `Device[]`。

#### Scenario: HTTP 客户端拉取设备列表
- **WHEN** 浏览器调用 `GET /api/devices`
- **THEN** 响应体 SHALL 为 JSON 数组,至少包含 `id`、`address`、`profiles[]` 字段;每个 profile SHALL 至少包含 `uri`、`codec`、`width`、`height`、`fps` 字段(codec / width / height / fps 在 oxvif 路径下可能为 Unknown / 0)

## REMOVED Requirements

### Requirement: 自定义多接口 UDP socket 绑定
**Reason**: 替换为 `oxvif::discovery::probe()` 内部绑定策略;boxvif 文档承诺行为等效(每非 loopback IPv4 接口 + 0.0.0.0 兜底)。
**Migration**: 旧实现永久迁到 `crates/ipcam-discovery/src/_legacy/` 子模块,通过 `MEDIA_TALK_DISCOVERY_BACKEND=legacy` env 可逆切换。
