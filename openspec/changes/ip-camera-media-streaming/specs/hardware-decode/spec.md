## ADDED Requirements

### Requirement: Rockchip MPP VPU 解码
系统 SHALL 使用 Rockchip MPP（`librockchip_mpp`）在 rk356x 的 VPU 上解码 H.264 与 H.265 access unit，并 SHALL 将 NV12 帧（Y 单平面 + UV 交错）输出到 `mpp_buffer_group`。

#### Scenario: 解码 H.264 关键帧
- **WHEN** 解码器收到一个 IDR NALU 与后续 GOP 段
- **THEN** 系统 SHALL 在 20 毫秒以内（1080p@30 典型值）输出一帧完整的 NV12 帧，并在 frame 被 release 时将其 buffer 归还到 buffer group。

#### Scenario: 解码 H.265 IDR
- **WHEN** 解码器依次收到 HEVC VPS/SPS/PPS 与紧随其后的 IDR
- **THEN** 系统 SHALL 输出一帧 NV12 帧，且解码后的宽高 SHALL 与 SPS 描述一致。

### Requirement: 解码错误处理
系统 SHALL 在遇到比特错误、IDR 缺失与中途分辨率变化时不终止进程。当出现不可恢复错误时，解码器 SHALL 将当前流标记为 `needs_reset` 并 SHALL 通知其上游消费者提供新的 IDR。

#### Scenario: 比特流损坏
- **WHEN** 解码器上报 `MPP_ERR_STREAM`
- **THEN** 系统 SHALL 丢弃当前帧、记录 `stream_err` 日志，并继续消费后续 NALU；sink 消费者 SHALL 不发生 panic。

#### Scenario: 中途格式变化
- **WHEN** 新的 SPS 指示了不同的分辨率
- **THEN** 系统 SHALL 在 200 毫秒内重新配置解码器（stop/drain/reset/reinit），丢弃已缓冲的帧，并按新尺寸恢复解码。

### Requirement: 帧 surface 所有权
系统 SHALL 通过 `DecodedFrame { buffer, handle, pts, width, height, stride }` 暴露输出帧，下游消费者通过 `Release` trait 释放；释放完成 SHALL 将底层 `mpp_buffer` 归还至 buffer group。

#### Scenario: 释放后复用
- **WHEN** 消费者 drop `DecodedHandle`
- **THEN** 底层 buffer SHALL 在 buffer group 配额内被下一帧解码请求复用。

### Requirement: 构建/目标平台门控
硬件解码 SHALL 仅在 `target_os = "linux"` 且启用 `hw-decode` feature 时编译。在不支持的目标上，系统 SHALL 回落到 CPU 软件解码器（FFmpeg / `openh264` / `dav1d`），以保证代码库在开发者的 x86_64 主机上仍可编译运行。

#### Scenario: x86_64 开发机构建
- **WHEN** 开发者以 `--features sw-decode --no-default-features` 构建
- **THEN** 构建 SHALL 成功，不链接 `librockchip_mpp`，并 SHALL 在同一 API 契约下运行。