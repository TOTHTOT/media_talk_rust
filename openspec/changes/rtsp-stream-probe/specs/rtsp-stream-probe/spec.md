## ADDED Requirements

### Requirement: CLI 子命令 probe 入口

`media_talk` 二进制 SHALL 提供 `probe` 子命令，接受以下参数：
- `--rtsp-url <URL>`：必填，可重复多次；URL 形如 `rtsp://[user[:pwd]@]host[:port]/path`。
- `--username <U>`：可选，RTSP 鉴权用户名；与 URL 中 userinfo 同时存在时 CLI 参数优先。
- `--password <P>`：可选，RTSP 鉴权密码。
- `--duration <秒>`：可选，默认 `5`；`0` 表示「拉到一个完整 access unit 就退出」。
- `--json`：可选；开启时所有诊断信息以单行 JSON 输出到 stdout，错误信息走 stderr；不开启时输出人类可读的多行摘要。

子命令 SHALL 在拉流结束后通过进程退出码报告结果，**不**留下后台进程或端口占用。

#### Scenario: 成功拉流到 IDR

- **WHEN** 用户运行 `media_talk probe --rtsp-url rtsp://camera/stream --username admin --password changeme --duration 5`，且摄像头在 5 秒内正常吐出至少 1 个 IDR
- **THEN** 进程退出码为 `0`，且报告至少包含：`access_units >= 1`、`idr_count >= 1`、`sps_hex` 非空、`pps_hex` 非空、`time_to_first_idr_ms` 非空、`bytes_total > 0`

#### Scenario: 鉴权失败返回 401

- **WHEN** 用户提供的用户名/密码被摄像头拒绝
- **THEN** 进程退出码为 `2`，错误信息指出 RTSP 401（`WWW-Authenticate` 头）且不暴露密码

#### Scenario: 拉流时长内无 IDR

- **WHEN** `RtspClient::play_loop` 在 `--duration` 指定的时长内未观察到 NAL type 5（IDR）
- **THEN** 进程退出码为 `1`，报告 `exit_reason = "duration_reached_without_idr"`，且其它计数（access_units、bytes_total、nal_by_type）仍按已观察到的 NAL 真实统计

#### Scenario: 多个 URL 依次探测

- **WHEN** 用户传入多个 `--rtsp-url`
- **THEN** probe 顺序逐个探测，**不**并发；每个 URL 独立计时并独立报告，**不**因前一个 URL 失败而中断后续；最终退出码按"参数错误(4) > 鉴权(2) > 无 IDR(1) > 其它(3) > 成功(0)"的优先级取所有 URL 中"最严重"的那一个

#### Scenario: JSON 模式单行输出

- **WHEN** 用户传入 `--json`
- **THEN** 每个 URL 的诊断结果以**单行** JSON 写到 stdout（多 URL 写多行），人类可读文本不出现；JSON 字段集合 SHALL 包含：`url`、`ok`、`elapsed_ms`、`video_codec`、`audio_codec`、`access_units`、`idr_count`、`time_to_first_idr_ms`（可空）、`sps_hex`（可空）、`pps_hex`（可空）、`nal_by_type`（对象，键为 NAL type 数字）、`bytes_total`、`bitrate_kbps`、`exit_reason`

### Requirement: NAL 单元统计语义

NAL 统计 SHALL 在 probe 内部的 `NalStats` 结构中累计，由 `ipcam-core` 暴露，且 SHALL 基于 `RtspClient::play_loop` 推送的 Annex-B 起始码 NAL 单元（`EncodedPacket.data`）直接读取 NAL 类型字节，**不**重新解 RTP。

`NalStats` 至少 SHALL 包含以下字段：
- `access_units: u64`：触发「帧边界」的次数（实现上等于 RTP marker=1 触发的 NAL 数）。
- `idr_count: u64`：NAL 类型字节 `& 0x1F == 5` 的累计计数。
- `sps: Option<Vec<u8>>`：第一次出现的 NAL 类型 7 的 RBSP（**不含** `00 00 00 01` 起始码）；后续 SPS 不覆盖。
- `pps: Option<Vec<u8>>`：第一次出现的 NAL 类型 8 的 RBSP；后续 PPS 不覆盖。
- `nal_by_type: BTreeMap<u8, u64>`：每个 NAL 类型出现次数。
- `bytes_total: u64`：所有 NAL 字节数（不含起始码）。
- `time_to_first_idr: Option<Duration>`：从 `play_loop` 开始到第一次见到 IDR 的相对耗时；未见到则为 `None`。
- `elapsed: Duration`：实际拉流时长。

#### Scenario: 累计 IDR 计数

- **WHEN** 摄像头在 5 秒内吐出 3 个 IDR slice（NAL type 5）+ 100 个非 IDR slice（NAL type 1）
- **THEN** `idr_count == 3`、`nal_by_type[1] == 100`、`nal_by_type[5] == 3`

#### Scenario: 捕获首条 SPS / PPS

- **WHEN** 拉流期间摄像头先发出 NAL type 7（SPS）后发出 NAL type 8（PPS）
- **THEN** `sps` SHALL 非空（保存 SPS 的 RBSP），`pps` SHALL 非空（保存 PPS 的 RBSP），且二者 SHALL 不包含 `00 00 00 01` 起始码前缀

#### Scenario: 后续 SPS/PPS 不覆盖

- **WHEN** 摄像头在拉流中重新发送 SPS（NAL type 7），且 `sps` 已非空
- **THEN** `sps` SHALL 保持为第一次的值（**不**被新 SPS 覆盖），便于稳定断言

#### Scenario: 首次 IDR 延迟记录

- **WHEN** `play_loop` 在第 412ms 首次观察到 NAL type 5
- **THEN** `time_to_first_idr` SHALL ≈ 412ms，序列化时 `time_to_first_idr_ms` 字段非空

### Requirement: 复用 ipcam-rtsp 现有 RTSP 抽象

probe 子命令 SHALL 复用 `ipcam-rtsp::RtspClient`，不引入新 RTSP 库。`RtspClient::connect()` 失败 SHALL 直接映射到对应的非零退出码（401 → 2、其它 → 3）；`play_loop` 的 `on_video` 闭包 SHALL 累计 `NalStats`，`on_audio` 闭包 SHALL 为空（v1 不统计音频）。

probe SHALL **不**修改 `RtspClient` 的公共方法签名；如需新增能力 SHALL 走**可选**的辅助函数路径（如 `ipcam-core::classify_h264_nal`）。

#### Scenario: 不重复解 RTP

- **WHEN** probe 收到 RTP 包
- **THEN** probe 不直接解析 RTP 头，所有 NAL 数据由 `RtspClient::play_loop` 推送的 `EncodedPacket.data` 取得；NAL 类型判定 SHALL 使用 `ipcam-core::classify_h264_nal` 或同等的「读 `data[4] & 0x1F`」模式

#### Scenario: connect 失败立即退出

- **WHEN** `RtspClient::connect()` 因网络/SDP 错误返回 `Err`
- **THEN** probe 立即停止、打印错误摘要到 stderr、退出码为 `3`（非 401）或 `2`（401），不进入 `play_loop`

### Requirement: 真机端到端可验证

probe SHALL 在**不**依赖 web 浏览器、**不**启动 web-display 服务的前提下，给出可独立判断拉流是否成功的诊断报告。任何能在 shell 里 paste 一行命令的环境 SHALL 能完成验证。

#### Scenario: 仅凭终端运行

- **WHEN** 用户在 Windows / Linux 终端中运行 `media_talk probe --rtsp-url ...`
- **THEN** 进程 SHALL 在 `--duration` 加上 `connect_timeout`（默认 10s）内退出，无后台端口、无 UI、无浏览器依赖

#### Scenario: 退出码可被 shell 消费

- **WHEN** 用户在 PowerShell / bash 中执行 `probe ... ; echo $LASTEXITCODE` 或 `probe ... && echo OK`
- **THEN** `$LASTEXITCODE` / `&&` 分支 SHALL 与「成功 / 鉴权失败 / 无 IDR / 其它」四种语义一一对应
