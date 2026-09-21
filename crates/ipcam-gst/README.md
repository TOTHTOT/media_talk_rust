# ipcam-gst

GStreamer 拉流与发流引擎，服务于 IP 摄像头媒体会话。

## 定位

`ipcam-gst` 是媒体管线的 GStreamer 绑定层，所有 `gstreamer` API 调用都限制在此 crate 内，纯逻辑（config / packet / stats）保持独立可测。

依赖方向：

```
media_talk_rust
    └── ipcam-gst
            └── ipcam-core (共享类型)
```

## 两大核心模块

### ingest — 拉流进（Web 预览）

将 RTSP 流 republish 到浏览器。管线拓扑：

```
rtspsrc → rtph264depay ! h264parse → webrtcsink(video_%u)  ← 浏览器 WebRTC 订阅
                                      ↕
                              (音频) rtppcmadepay ! alawdec ! tee
                                        ├── queue ! audioconvert ! audioresample ! opusenc → webrtcsink(audio_%u)
                                        └── queue ! alsasink  (本地 ALSA 播放, 可选)
```

**设计要点**：

- 视频 H.264/H.265 **直接透传**，不解码，延迟 = WebRTC jitter buffer（毫秒级）
- 音频 G.711/AAC **转码为 Opus**（webrtcsink 只接受 raw/opus）
- 信令服务器默认 `0.0.0.0:8443`，进程内共享
- 支持重连策略（指数退避）

可选的 **tap branch** 分叉解码后的原始帧用于 GUI / 分析：

```
parse → tee → (直推 webrtcsink)
            ↓
         queue(leaky) → avdec → videoconvert → appsink(RGBA) → 回调
```

### rtp_send — 发出去（SIP 通话）

多源 → RTP 发送器，每路媒体独立指定源（文件 / RTSP / 相机 / 麦克风），解码后重编码发送：

```
视频: 源 → [解码] → queue ! videoconvert ! videoscale
        → capsfilter(width≤640) → x264enc → rtph264pay → udpsink

音频: 源 → [解码] → queue ! audioconvert ! audioresample
        → capsfilter(8kHz/mono) → alawenc|mulawenc → rtppcmapay|rtppcmupay → udpsink
```

**支持的源类型**：

| 类型 | 格式 | 平台差异 |
|------|------|----------|
| `file:<path>` | mp4 / wav / mp3 | 无 |
| `rtsp://<uri>` | H.264/H.265 / G.711/AAC | 无 |
| `camera[:device]` | v4l2src (Linux) / ksvideosrc (Windows) | 采集元件不同 |
| `mic` | alsasrc (Linux) / wasapisrc (Windows) | 采集元件不同 |

## 公开 API

### 启动流会话

```rust
// Web 预览（无 GUI 分支）
pub fn start(cfg: GstStreamConfig) -> Result<GstStreamHandle, GstStreamError>

// Web 预览 + GUI 原始帧回调
pub fn start_with_taps(cfg: GstStreamConfig, taps: RawTaps) -> Result<GstStreamHandle, GstStreamError>
```

### 发送 RTP 流（SIP 通话用）

```rust
pub fn start_rtp_sender(cfg: RtpSendConfig) -> Result<RtpSender, GstStreamError>
```

### 信令服务器

```rust
// 启动进程内 WebRTC 信令服务器（默认 0.0.0.0:8443）
// 幂等，可安全重复调用
pub fn ensure_signalling_server() -> Result<(), GstStreamError>
```

### Session 状态查询

```rust
pub struct GstStreamHandle {
    pub fn state(&self) -> StreamState        // Connecting / Playing / Reconnecting / Failed / Ended
    pub fn stats(&self) -> StreamStats        // frames_video, frames_audio, bytes, reconnects, last_error
    pub fn stop(&self)                        // 停止会话
}

pub struct StreamStats {
    pub frames_video: u64,
    pub frames_audio: u64,
    pub bytes: u64,
    pub reconnects: u32,
    pub last_error: Option<String>,
}
```

### 原始帧 Tap（GUI / 分析用）

```rust
pub struct RawTaps {
    pub video: Option<VideoFrameSink>,  // RGBA 回调
    pub audio: Option<AudioChunkSink>,  // S16LE 回调
}

pub struct RawVideoFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: usize,   // 每行字节数（含 padding）
    pub data: &'a [u8], // RGBA 像素
}

pub struct RawAudioChunk<'a> {
    pub rate: u32,
    pub channels: u32,
    pub data: &'a [u8], // interleaved S16LE
}
```

### 源解析

```rust
// 解析 CLI 字符串为 TrackSource
pub fn parse_track_source(s: &str) -> Result<TrackSource, String>
// 例: "file:./a.mp4", "rtsp://10.0.0.1:554/stream", "camera", "mic"
```

## 文件结构

```
src/
├── lib.rs           # 公开 API 入口、Error 类型、ensure_signalling_server
├── config.rs        # GstStreamConfig / RtpSendConfig / ReconnectPolicy
├── stats.rs         # StreamState / StreamStats / GstStreamHandle
├── ingest.rs        # RTSP → WebRTC 管线实现（拉流进）
├── rtp_send.rs      # 多源 → RTP 发送器（发出去）
├── tap.rs           # 原始帧回调类型（VideoFrameSink / AudioChunkSink）
└── gstutil.rs      # 建元件 / 链接 / 同步小工具（ingest 和 rtp_send 共用）
```

## 线程模型

- `ingest`: 一个 session 线程（bus 监控）+ 一个 stats ticker 线程
- `rtp_send`: 一个 bus 线程 + 一个 stats ticker 线程
- 所有 GStreamer 元件在各自线程运行，通过 pad link 传递 buffer
- `GstStreamHandle` 线程安全（`Send + Sync`），可跨线程查询状态

## 错误处理

```rust
pub enum GstStreamError {
    InvalidConfig(String),  // 静态配置错误（URI 格式、参数范围）
    Init(String),           // GStreamer 初始化失败（缺插件）
    Connect(String),        // 连接失败（含 401 认证失败）
    Stream(String),         // 运行时流错误（EOS / ERROR）
    Link(String),           // 分支链接失败
}
```

## Examples

```bash
# Web 预览
cargo run -p ipcam-gst --example ingest -- rtsp://admin:pass@192.168.1.64:554/stream

# SIP 发流
cargo run -p ipcam-sip --example call -- \
    --video-src "file:./assets/oceans.mp4" \
    --audio-src "file:./assets/oceans.mp4" \
    1000

# 原始帧 tap
cargo run -p ipcam-gst --example rawtap -- rtsp://192.168.1.64:554/stream
```

## 平台差异

| 功能 | Windows | Linux (aarch64) |
|------|---------|------------------|
| 相机采集 | ksvideosrc | v4l2src |
| 麦克风采集 | wasapisrc | alsasrc |
| 视频解码 | avdec_h264 / avdec_h265 (gst-libav) | 同 |
| 硬件解码 | 不支持 | 可选 rockchip MPP (`--features hw-decode`) |
| 视频 tap branch | 支持 | `aarch64` 禁用（软件解码太慢） |

## 构建依赖

- GStreamer 1.18+（需 `gstreamer1.0-dev` / GStreamer MSVC SDK）
- pkg-config
- `gstreamer1.0-plugins-good`（提供 rtspsrc / rtph264depay 等）
- `gstreamer1.0-plugins-bad`（提供 webrtcsink / rtph265depay）
- `gstreamer1.0-libav`（提供 avdec_* 软件解码器）
