# v4l2-device-cap Specification

## Purpose
TBD - created by archiving change ip-camera-media-streaming. Update Purpose after archive.
## Requirements
### Requirement: V4L2 能力探测
系统 SHALL 通过 V4L2 的 `VIDIOC_QUERYCAP`、`VIDIOC_ENUM_FMT`、`VIDIOC_ENUM_FRAMESIZES` ioctl 探测 `/dev/video*` 视频采集设备，并 SHALL 返回结构化的能力记录。

#### Scenario: 枚举一台 V4L2 采集设备
- **WHEN** 存在 `/dev/video0` 且支持 `YUYV` / `MJPEG` / `H264` / `NV12` 像素格式
- **THEN** 系统 SHALL 返回至少包含总线信息、驱动名、支持的像素格式及各格式对应分辨率的记录。

### Requirement: 仅暴露能力，不暴露采集流
`v4l2-device-cap` 能力 SHALL 仅暴露能力枚举结果，本 change SHALL **不**提供 `V4l2Capture` 流。

#### Scenario: 不存在采集设备
- **WHEN** 当前机器没有 V4L2 采集设备（如开发者主机）
- **THEN** 系统 SHALL 返回空列表并正常退出，且 SHALL 不再执行任何额外的 ioctl。

### Requirement: 仅 Linux 编译
`v4l2-device-cap` crate SHALL 仅在 `target_os = "linux"` 时编译。

#### Scenario: Windows / macOS 构建
- **WHEN** 开发者构建在非 Linux 主机上
- **THEN** `v4l2-device-cap` crate SHALL 不参与编译；依赖它的代码 SHALL 针对 stub trait 继续编译通过。

