## ADDED Requirements

### Requirement: PCM 采集与播放
系统 SHALL 提供对 Linux ALSA（`libasound`）的 Rust 绑定，能够以交错的有符号 16-bit LE 模式打开 PCM 流，支持可配置的采样率（8k/16k/44.1k/48 kHz）、可配置的通道数（1 或 2）、可配置的周期大小（默认 1024 帧）。

#### Scenario: 从默认设备采集 16 kHz 单声道
- **WHEN** 运维调用 `Alsa::open_capture("default", rate: 16000, channels: 1)`
- **THEN** 系统 SHALL 读取并以尽量零拷贝的方式暴露 PCM 帧，并通过 `AlsaError` 报告 underrun / overrun。

#### Scenario: 播放 PCM 样本
- **WHEN** 运维调用 `Alsa::open_playback(...)` 后调用 `write(samples)`
- **THEN** 设备 SHALL 接受交错 S16LE 帧，并在成功时返回 `ALSA_ERR_OK`。

### Requirement: 设备枚举
系统 SHALL 通过 ALSA hints API 枚举 PCM 设备 ID（如 `plughw:0,0`、`hw:1,0`）及其人类可读名称。

#### Scenario: 至少列出一个采集设备
- **WHEN** 在 radxa-cm3-rpi-cm4-io 上调用 `enumerate_capture_devices()`
- **THEN** 系统 SHALL 返回至少一条记录，包含 hint name 与 pcm hint id。

### Requirement: 仅 Linux 编译
`ipcam-alsa` crate SHALL 仅在 `target_os = "linux"` 时编译。

#### Scenario: Windows / macOS 构建
- **WHEN** 开发者构建在非 Linux 主机上
- **THEN** `ipcam-alsa` crate SHALL 不参与编译，依赖它的代码路径 SHALL 使用返回 `NotImplemented` 的 stub trait。