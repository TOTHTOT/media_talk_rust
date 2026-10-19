## ADDED Requirements

### Requirement: 运行时可切换的 discovery backend
系统 SHALL 启动时读取环境变量 `MEDIA_TALK_DISCOVERY_BACKEND`,其值 SHALL 为 `oxvif`(默认)或 `legacy` 之一;二者 MUST 均能完成 WS-Discovery + GetProfiles + GetStreamUri 三个最小必要操作并返回与 `media_talk discover --json` 字段对齐的 JSON。

#### Scenario: 默认 backend 是 oxvif
- **WHEN** media_talk 服务启动,环境变量 `MEDIA_TALK_DISCOVERY_BACKEND` 未设置
- **THEN** 系统 MUST 通过 `oxvif` backend 处理所有 discovery / get_profiles / get_stream_uri 调用,版本严格 pin 为 `0.12.0`

#### Scenario: 显式切到 legacy
- **WHEN** media_talk 服务启动时 `MEDIA_TALK_DISCOVERY_BACKEND=legacy` 被设置
- **THEN** 系统 MUST 走 `ipcam-discovery` 内置的旧实现路径,行为与 8af677d 等价(每非 loopback 接口独立 UDP socket + 手写 SOAP envelopes + WS-Security UsernameToken)

#### Scenario: 非法 env 值
- **WHEN** `MEDIA_TALK_DISCOVERY_BACKEND` 设置为非 `oxvif` / `legacy` 的字符串(如 `foo`)
- **THEN** 系统 SHALL 启动失败并打印一行 `error: invalid MEDIA_TALK_DISCOVERY_BACKEND=foo, expected oxvif|legacy`,退出码非 0

#### Scenario: 运行时无需重启 binary 即可切换
- **WHEN** 板端通过 `systemctl edit media_talk` 加一行 `Environment=MEDIA_TALK_DISCOVERY_BACKEND=legacy` 后 `daemon-reload && systemctl restart`
- **THEN** 整个回滚操作 MUST 在 30 秒内完成(不需重新 git / 重 build / 重 deploy)
