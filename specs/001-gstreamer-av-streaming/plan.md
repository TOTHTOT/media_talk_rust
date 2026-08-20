# Implementation Plan: GStreamer 音视频拉流

**Branch**: `001-gstreamer-av-streaming` | **Date**: 2026-08-20 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `/specs/001-gstreamer-av-streaming/spec.md`

## Summary

设备 ONVIF 发现已通，但自研 `ipcam-rtsp` 的 `play_loop` 只解 H.264 视频、音频完全未接。按 FR-008 决策，新增 `ipcam-gst` crate，用 GStreamer（`rtspsrc` 动态分流 + depay/parse + `appsink` 回调）统一承担音视频拉流，完全替换自研拉流路径；视频帧沿用 `EncodedPacket`（Annex-B 单 NAL + marker 定 AU 边界）喂给现有 `Fmp4Muxer` → WebSocket → MSE 链路，web-display 侧零接口改动；音频本期经管线内 `alawdec/mulawdec/aacparse!avdec_aac ! alsasink` 支路出目标板扬声器，同时以 `AudioPacket` 回调暴露编码帧，为二期双向对讲预留扩展点。断线重连采用 bus ERROR/EOS 触发整条管线重建 + 指数退避。

## Technical Context

**Language/Version**: Rust 1.85（edition 2024，workspace 统一）

**Primary Dependencies**: `gstreamer 0.24` + `gstreamer-app 0.24`（MSRV 1.83，向后兼容系统库 ≥1.14）；tokio / tracing / bytes / parking_lot（workspace 已有）

**Storage**: N/A

**Testing**: `cargo test`（unit：配置校验、AU→NAL 切分、时间戳换算、状态机迁移）+ 目标板实机验收（quickstart.md S1~S6）

**Target Platform**: 运行 rk356x（aarch64-unknown-linux-gnu，Rockchip 固件 GStreamer ≥1.20）；开发/调试 Windows x86_64（MSVC）

**Project Type**: 嵌入式媒体服务（cargo workspace，新增一个库 crate）

**Performance Goals**: 2 路 1080p30 并发；服务启动到首帧 ≤10s；音视频同步偏差 <200ms

**Constraints**: 目标板须预装 gstreamer1.0-plugins-good/-base/-alsa；fMP4 muxer 仅支持 H264（H265 接收解封装进回调，网页播放扩展为最低优先级任务）；交叉编译走 WSL2/Docker + sysroot

**Scale/Scope**: 单设备 ≤8 路并发拉流

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

`.specify/memory/constitution.md` 仍是未实例化的模板（占位符未填写），无任何已生效的原则或门禁 → **PASS（无 gates 可违反）**。

Phase 1 设计后复核：新增一个单一职责 crate、复用既有数据契约、错误经 `CoreError` 语义兼容的自有错误类型上报、全程 tracing 结构化日志——与项目现有惯例（一 crate 一职责、CLI 可观测）一致。无违规，无需 Complexity Tracking。

## Project Structure

### Documentation (this feature)

```text
specs/001-gstreamer-av-streaming/
├── plan.md              # 本文件
├── research.md          # Phase 0：R1~R10 决策与来源
├── data-model.md        # Phase 1：实体→类型映射、状态机
├── contracts/
│   └── ipcam-gst-api.md # Phase 1：新 crate 公开 API 契约
├── quickstart.md        # Phase 1：S1~S6 端到端验收
└── checklists/
    └── requirements.md  # specify 阶段产物
```

### Source Code (repository root)

```text
crates/
├── ipcam-gst/                # 新增：GStreamer 拉流引擎
│   ├── Cargo.toml            # gstreamer 0.24 / gstreamer-app 0.24
│   └── src/
│       ├── lib.rs            # 公开 API（契约见 contracts/ipcam-gst-api.md）
│       ├── config.rs         # GstStreamConfig / AudioOutput / ReconnectPolicy + 校验
│       ├── pipeline.rs       # 管线构建、pad-added 分流、bus 监听、重建退避
│       ├── packet.rs         # AudioPacket；AU→Annex-B NAL 切分；PTS 换算
│       └── stats.rs          # StreamState 状态机 + StreamStats 周期日志
├── web-display/src/stream.rs # 改：spawn_streaming 内部换 ipcam_gst::start（签名不变）
├── ipcam-rtsp/src/lib.rs     # 改：删除 play_loop；保留 connect/teardown/sdp/RtspError
├── ipcam-rtsp/src/rtp.rs     # 删除（depacketizer 随 play_loop 退役；e2e 测试同步删除）
└── media_talk/src/ipc/probe.rs # 改：play_until 迁移到 ipcam_gst::start（NalStats 基于 EncodedPacket 重建）
```

**Structure Decision**: 遵循 workspace "一 crate 一职责" 惯例，GStreamer 依赖隔离在 `ipcam-gst` 内；web-display 通过 `ipcam-gst` 的 feature（`gst`，默认开启）接入，保证未安装 GStreamer 的开发机仍可 `cargo test`（Windows 装有 MSVC 版 GStreamer 时可完整调试，见 quickstart.md §2）。注意：`probe` 命令的 `play_until` 同样依赖被删的 `play_loop`，必须与 web-display 同批迁移，否则 `play_loop` 删除后编译即破。

## Complexity Tracking

无违规，无需记录。
