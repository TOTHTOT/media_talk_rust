# rtp_send 多输入源设计

日期: 2026-09-20
状态: 已实现 (branch rtp-send-multi-source, 2026-09-20)
范围: `crates/ipcam-gst/src/rtp_send.rs` 重构 + `ipcam-sip/examples/call.rs` 参数扩展

## 背景

`rtp_send.rs` 目前只支持单一 MP4 文件输入 (filesrc → qtdemux), SIP 通话
联调时想验证真实对讲场景 (摄像头视频 + 麦克风音频) 做不了。用户在工作区
里起了个草图 (`PacketSource` trait / `MediaSource` 单源枚举), 但形状不对:
把发送目标 (dest) 和源混在一起, 且单源枚举与"音视频分别指定来源"的
需求冲突。本设计取而代之。

## 需求 (用户已确认)

- 输入源四种全要: RTSP 网络相机 / 本机 USB 相机 / 本机麦克风 /
  单独音频或视频文件
- 音视频两路可分别指定来源 (例: 视频用相机 + 音频用麦克风)
- 跨平台: Windows 开发机 + aarch64 Linux 板子 (RK3566); macOS 不支持
  但不阻断编译

## 方案: 按轨抽象 + 统一解码重编码 (方案 A)

每一路媒体 (音频/视频) 独立指定源。源只负责产出解码后的裸流, 后续的
编码 → pay → udpsink 链路对所有源共用。统一重编码把"片源/相机编码参数
超出对端解码能力"这一类问题从源头消灭 (参见 docs/pitfalls.md 门口机
黑屏系列): slice-max-size / IDR 间隔 / 分辨率 / 无 AUD 全部由我们的
编码器控制, 不再挑输入。

否决的备选: B (uridecodebin 统一 URI, 设备源无 URI 形式抽象会漏),
C (每源独立 example, 管线代码复制四份, 黑屏修复经验要维护四份)。

## 组件

### TrackSource 枚举 (每轨一个)

```rust
pub enum TrackSource {
    /// 文件 (mp4/wav/mp3 均可, 只取当前需要的轨)
    File(PathBuf),
    /// RTSP 网络相机 (H264/H265 均可, uridecodebin 负责解)
    Rtsp { uri: String },
    /// 本机相机: Linux v4l2src (USB 和 MIPI/CSI 同元件, 设备节点不同),
    /// Windows ksvideosrc. None = 平台默认设备
    LocalCamera { device: Option<String> },
    /// 本机麦克风: Windows wasapisrc, Linux alsasrc
    Mic,
}
```

### RtpSendConfig 改为按轨 (源, 目标) 配对

```rust
pub struct RtpSendConfig {
    pub audio: Option<(TrackSource, AudioDest)>,
    pub video: Option<(TrackSource, RtpDest)>,
}
```

### 源段构建 (新增)

`build_source_track(pipeline, source, kind) -> Result<gst::Pad>`:
按 kind (音频/视频) 从源上拿到解码后裸流的 src pad (链头带 queue)。

- `File` / `Rtsp`: 统一 `uridecodebin` (rtsp:// 它原生支持), pad-added
  里按当前要建的轨匹配媒体类型, 直接出裸流
- `LocalCamera` / `Mic`: 平台分发, 集中在一个函数里
  (`if cfg!(windows) { "ksvideosrc" } else { "v4l2src" }`), 不用 #[cfg]
  散布; 未知平台编译通过, 运行时 make() 报元件不存在

### 发送链 (所有源共用)

- 视频: `queue → videoconvert → videoscale → capsfilter →
  x264enc(aud=false, option-string="slice-max-size=1300",
  key-int-max=每秒一个 IDR) → rtph264pay(config-interval=1) → udpsink`
  — 黑屏三轮修复的经验 (无 AUD / 单 NAL / SPS-PPS 周期重发) 固化在这里
- 音频: `queue → audioconvert → audioresample → capsfilter(8kHz/mono)
  → alawenc|mulawenc → rtppcmapay|rtppcmupay → udpsink` (现有后半段,
  编码仍按 SDP answer 选)

### 不动的部分

`RtpSender` / `stop()` / `Drop` / SendStats 包计数 probe / bus 线程 /
每 2s stats 日志, 全部保持现状。

## 数据流

每轨: 独立源段 + 一条发送链, 插进同一个 pipeline。
音视频用同一文件时会解码两次 — 联调工具, 不做共享优化 (YAGNI)。

## 错误处理

- 源里没有请求的轨 (例: 纯音频文件拿去发视频): pad 永远不来, 不阻塞
  另一路; 启动打 warn, 靠现有 stats 0 包发现
- 源元件缺失 (平台不支持/插件没装): 启动即报错, 走 GstStreamError
- MIPI 相机系统层 (设备树 / media-ctl 链路配置) 不在本模块范围

## 测试

- 沿用 loopback 集成测试: 文件源发 127.0.0.1, 3 秒断言两路出包
  (现有 `sender_actually_emits_packets` 改造)
- 新增: 音视频来自不同文件源的组合 (验证按轨选源通路)
- 设备源 (USB/MIPI/麦克风/RTSP) 无 CI 环境, 留 example 手动验证;
  MIPI 待带相机的板子实测

## call example 参数

```
--video-src file:assets/oceans.mp4 | rtsp://... | camera[:/dev/video0]
--audio-src file:assets/oceans.mp4 | mic
```

默认保持现行为 (都取 oceans.mp4), 不传参数时和今天一样。

## 明确不做

- 不做 H265 发送 (对端 answer 从不选它; 接收侧 H265 由 ingest 管)
- 不做源共享 (同一文件两轨解码两次, 可接受)
- 不做 macOS 适配
- 不做板子硬编码器 (留分发点, 后续 mpph264enc 插进同一处)
