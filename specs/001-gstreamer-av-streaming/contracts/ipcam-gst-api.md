# Contract: `ipcam-gst` crate 公开 API

**Date**: 2026-08-20 | **Feature**: specs/001-gstreamer-av-streaming

本特性是纯内部能力（不新增 HTTP/CLI 对外接口），契约即新 crate `ipcam-gst` 的 Rust 公开 API。调用方为 `web-display::stream`（替换 `RtspClient::play_loop` 的位置）。

## 公开类型

```rust
// ---- 配置 ----
pub struct GstStreamConfig {
    pub uri: String,
    pub credentials: Option<(String, String)>,
    pub latency_ms: u32,                 // default 200
    pub audio_output: AudioOutput,       // default Disabled
    pub reconnect: ReconnectPolicy,      // default 1s→30s 指数退避，无限重试
}

pub enum AudioOutput {
    Disabled,                            // 仅回调编码音频帧
    Alsa { device: String },             // 管线内 tee 出解码播放支路
}

pub struct ReconnectPolicy {
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub max_attempts: Option<u32>,       // None = 无限
}

// ---- 帧 ----
pub struct AudioPacket {
    pub codec: AudioCodec,
    pub data: Bytes,
    pub pts_us: i64,
    pub rate: u32,
    pub channels: u32,
}

// 视频帧复用 ipcam_core::EncodedPacket（契约见 data-model.md）

// ---- 会话 ----
pub enum StreamState { Connecting, Playing, Reconnecting, Failed, Ended }

pub struct StreamStats {
    pub frames_video: u64,
    pub frames_audio: u64,
    pub bytes: u64,
    pub reconnects: u32,
    pub last_error: Option<String>,
}

pub struct GstStreamHandle { /* opaque */ }
```

## 公开函数

```rust
/// 校验配置（uri 前缀、latency 范围、退避参数），不触网。
pub fn validate(cfg: &GstStreamConfig) -> Result<(), GstStreamError>;

/// 启动拉流会话：构建管线并进入后台事件循环。
/// 视频/音频帧分别在对应回调中投递；回调在 GStreamer 流线程上执行，
/// 实现必须轻量（拷贝 Bytes 后转交，禁止阻塞）。
/// 返回的 handle 用于查询状态与停止。
pub fn start<V, A>(
    cfg: GstStreamConfig,
    on_video: V,
    on_audio: A,
) -> Result<GstStreamHandle, GstStreamError>
where
    V: FnMut(EncodedPacket) + Send + 'static,
    A: FnMut(AudioPacket) + Send + 'static;

impl GstStreamHandle {
    pub fn state(&self) -> StreamState;
    pub fn stats(&self) -> StreamStats;
    /// 停止会话：管线置 Null，状态迁移到 Ended，不再重连。
    pub fn stop(&self);
}
```

## 错误模型

```rust
pub enum GstStreamError {
    InvalidConfig(String),     // validate 阶段的静态错误
    Init(String),              // GStreamer 初始化/元件缺失（如未装 plugins-good）
    Connect(String),           // 建连失败（含认证失败，message 中含 401/Unauthorized 字样）
    Stream(String),            // 播放期 bus ERROR
}
```

- 认证失败与网络不可达都映射为 `Connect`，与 `probe` 命令现有 401 分类逻辑（字符串匹配）保持兼容。
- `Init` 通常意味着目标板缺插件包，错误信息须包含缺失元件名（如 `rtspsrc`）。

## 行为契约（调用方可依赖的保证）

1. **顺序**：同一路轨的帧按到达顺序投递；视频关键帧前保证 SPS/PPS 已随流出现（h264parse byte-stream 自带周期性注入）。
2. **状态可见**：每次状态迁移（Connecting/Playing/Reconnecting/Failed/Ended）产生一条 `info!` 结构化日志；统计每 10s 一条。
3. **重连透明**：重连过程中回调暂停投递，恢复后继续；调用方通过 `state()` 或日志感知，无需处理重建。
4. **停止幂等**：`stop()` 多次调用安全；stop 后回调保证不再被调用。
5. **线程**：回调发生在 GStreamer 内部线程；`GstStreamHandle` 是 `Send + Sync`。

## 被替换方契约（web-display 侧改动面）

`web-display::stream::spawn_streaming` 的**签名与对外行为不变**（SessionId/mux/state_tx 参数照旧），仅内部把 `RtspClient::connect + play_loop` 换成 `ipcam_gst::start`。`ipcam-rtsp` 保留 `RtspClient::connect/teardown`、`RtspError`、`sdp` 模块（probe 命令依赖），删除 `play_loop` 与 `rtp` 模块的 depacketizer。
