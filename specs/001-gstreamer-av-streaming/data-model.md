# Phase 1 Data Model: GStreamer 音视频拉流

**Date**: 2026-08-20 | **Feature**: specs/001-gstreamer-av-streaming

规格中的 4 个实体到 Rust 类型的映射。复用 `ipcam-core` 已有类型优先，不为对得上的概念造新类型。

## StreamSource（流源）→ `GstStreamConfig`（新，ipcam-gst）

| 字段 | 类型 | 说明 |
|------|------|------|
| `uri` | `String` | 完整 RTSP 地址；userinfo 允许出现（rtspsrc 原生支持） |
| `credentials` | `Option<(String, String)>` | 显式凭据，非空时映射到 rtspsrc `user-id`/`user-pw`，优先于 URI userinfo |
| `latency_ms` | `u32` | jitterbuffer 缓冲，默认 200（FR：可配置） |
| `audio_output` | `AudioOutput` | `Disabled` / `Alsa { device: String }`（默认 `Disabled` + 仅回调） |
| `reconnect` | `ReconnectPolicy` | `initial_delay: Duration=1s`、`max_delay: Duration=30s`、`max_attempts: Option<u32>=None`（无限） |

校验规则：`uri` 必须以 `rtsp://` 开头；`latency_ms` ∈ [0, 5000]；`max_delay ≥ initial_delay`。

## MediaTrack（媒体轨）→ 复用 `ipcam_core::{VideoCodec, AudioCodec}` + `TrackKind`

```rust
pub enum TrackKind { Video(VideoCodec), Audio(AudioCodec) }
```

- 分流判定来源：rtspsrc pad caps 的 `media` + `encoding-name` 字段；兼容 PCMA/PCMU 静态 payload（`payload=8/0` 无 `encoding-name`）。
- 编码参数（宽高/帧率、采样率/声道）从 appsink caps 读取：视频 `width/height/framerate`，音频 `rate/channels`。G.711 固定 8000 Hz 单声道。

## EncodedFrame（编码帧）→ 视频复用 `EncodedPacket`，音频新增 `AudioPacket`

**视频**（复用 `ipcam_core::EncodedPacket`，契约不变）：
- `data`: Annex-B 单 NAL，含 `00 00 00 01` 起始码，≥5 字节
- `codec`: `H264` / `H265`
- `rtp_ts`: GST PTS 换算 90kHz（`pts_ns * 90000 / 1_000_000_000`）
- `arrival_us`: `ipcam_core::now_micros()`
- `is_keyframe`: H264 看 `nal_type==5`；H265 看 `nal_type` ∈ {19,20}（IDR_W_RADL/IDR_N_LP）
- `marker`: AU 最后一个 NAL 置 true（h264parse `alignment=au` 的 AU 内多 NAL 场景）

**音频**（新增 `AudioPacket`，ipcam-gst）：

| 字段 | 类型 | 说明 |
|------|------|------|
| `codec` | `AudioCodec` | `Aac` / `G711A` / `G711U` |
| `data` | `Bytes` | 编码帧（AAC raw 或 G.711 样本块） |
| `pts_us` | `i64` | GST PTS 换算微秒 |
| `rate` | `u32` | 采样率（G.711 恒 8000） |
| `channels` | `u32` | 声道数（G.711 恒 1） |

> 不扩展 `EncodedPacket.codec` 为统一枚举：`EncodedPacket` 被 muxer/解码器多处按视频语义消费，新增独立类型比泛化改动小，且为二期对讲回传留出独立演进空间（FR-009 预留点）。

## StreamSession（流会话）→ `GstStreamHandle` + `StreamStats`（新，ipcam-gst）

状态机：

```text
Connecting ──► Playing ──► Reconnecting ──► Connecting（退避后重建）
    │             │              ▲
    ▼             ▼              │（重试上限到达时，None=永不）
  Failed        Ended ◄──────────┘
```

- 触发迁移的事件：管线进入 PLAYING（→Playing）、bus ERROR/EOS/RTSPSrcTimeout（→Reconnecting 或 Ended）、重试耗尽（→Failed）、主动 `stop()`（→Ended）。
- `StreamStats`：`frames_video: u64`、`frames_audio: u64`、`bytes: u64`、`reconnects: u32`、`last_error: Option<String>`；每 10s 由 tracing 周期性输出一次（FR-006）。

## 关系

- 一个 `GstStreamHandle` 绑定一个 `GstStreamConfig`（1:1）。
- 一个会话输出 0..1 视频轨 + 0..1 音频轨（IPCam 实际形态；多视频轨只取第一路并 warn）。
- `AudioOutput::Alsa` 非 Disabled 时，音频支路额外 tee 出解码播放链；回调链路与播放链路互不影响。
