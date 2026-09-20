# ipcam-gst 流媒体引擎

`ipcam-gst` 是基于 GStreamer 的 RTSP → WebRTC 转发引擎：从相机拉流，零转码推给
浏览器；可选地把解码后的原始帧/Pcm 通过回调交给原生 GUI 或分析模块（RawTaps）。

设计目标：**低延迟**（视频不转码、web 支路无缓冲）、**可自愈**（断流指数退避重连）、
**可观测**（状态机 + 帧计数）。

## 公开 API

```rust
// 1. 启动进程级信令服务器（ws://0.0.0.0:8443，幂等，整个进程调一次）
ipcam_gst::ensure_signalling_server()?;

// 2. 启动一路流
let cfg = GstStreamConfig {
    uri: "rtsp://admin:changeme@192.168.1.10:8554/ch01".into(),
    stream_name: "front-door".into(),   // 浏览器按这个名字订阅
    ..Default::default()
};
let handle = ipcam_gst::start(cfg)?;

// 3. 观测与控制
handle.state();        // StreamState: Connecting / Playing / Reconnecting / Ended / Failed
handle.stats();        // StreamStats: frames_video / frames_audio / bytes / reconnects ...
handle.stop();         // 幂等；立即停止（打断重连退避）
```

需要原始帧时用 `start_with_taps(cfg, taps)`，见下文 [RawTaps](#rawtaps-原始帧出口)。

### GstStreamConfig

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `uri` | — | 完整 RTSP URI，允许内嵌 userinfo |
| `credentials` | `None` | 显式凭据，设置后覆盖 URI 里的 userinfo |
| `latency_ms` | `200` | rtspsrc 抖动缓冲深度（0–5000） |
| `audio_output` | `Disabled` | `Alsa { device }` 时额外 tee 一路到板端扬声器 |
| `reconnect` | 1s→30s，无限 | 指数退避策略（`ReconnectPolicy`） |
| `stream_name` | `"stream"` | 信令频道上的 producer 名（`meta,name=...`），浏览器按它匹配 |
| `signalling_host/port` | `127.0.0.1:8443` | 进程内信令服务器地址 |

`ipcam_gst::validate(&cfg)` 可做无网络静态校验（URI 前缀、latency 范围、退避参数等）。

## 管线拓扑

### 视频（无转码直通）

```text
无 tap：
  rtspsrc ═pad-added═▶ rtph264depay → h264parse → webrtcsink (video_%u)

有 tap：
  rtspsrc ═pad-added═▶ rtph264depay → h264parse → tee ─┬─▶ webrtcsink (video_%u)
                                                       └─▶ queue(leaky) → avdec_h264
                                                           → videoconvert → appsink(RGBA)
```

- 视频**不转码**：H.264/H.265 编码码流直接进 webrtcsink，浏览器硬解。
- **web 支路不加 queue**：tee 直推 webrtcsink，保持最低延迟；tap 支路的
  `queue(leaky=downstream, max-size-buffers=5)` 满了丢自己的帧，绝不把背压
  传染给 web 支路。
- H.265 走 `rtph265depay / h265parse / avdec_h265`，由 `encoding-name` 自动选择。

### 音频（必转码）

```text
rtspsrc ═pad-added═▶ *depay → decode → tee ─┬─▶ audioconvert → audioresample
                                             │    → opusenc → webrtcsink (audio_%u)
                                             ├─▶ queue → convert → resample → alsasink
                                             │    （audio_output = Alsa 时）
                                             └─▶ queue(leaky) → convert → resample
                                                  → appsink(S16LE)（tap 时）
```

- webrtcsink 的 `audio_%u` 只收 Opus/raw，相机发 G.711(PCMA/PCMU)/AAC，
  所以音频**必须先解码成 PCM 再重编码**。
- tee 是音频链的**固定分发点**（decode 链恒以 tee 结尾），web / alsa / tap
  三路都是可选挂载；这与视频的"有 tap 才插 tee"不同——因为 web 支路本身
  就挂在 tee 上。
- 无音轨的相机：日志打印 `stream has no audio track`，音频分支整段不建。
- AAC 解码需要 `avdec_aac`（gst-libav；Debian 包 `gstreamer1.0-libav`）。

### 信令服务器

`ensure_signalling_server()` 起一个**锚定管线**：一个设了
`run-signalling-server=true` 的 webrtcsink 常驻进程（静态 `OnceLock` 持有），
它只做信令、不传媒体。每个流会话的 webrtcsink 通过
`signaller.uri = ws://{signalling_host}:{signalling_port}` 连上它并注册为
producer（`meta,name = stream_name`）。浏览器作为 listener 连同一个端口，
按 `stream_name` 匹配后走标准 SDP offer/answer + ICE 建连。

> 注意：8443 是进程级单例。同机调试时旧的 serve/gst 进程没退干净会导致
> bind 失败（10048），先杀旧进程再排障。

## 链接纪律（血泪教训，改动管线前必读）

1. **先 add 入管 + sync_state_with_parent，再 link**。孤儿态（未入 bin）建好的
   链接会在 `add_many` / `sync_state_with_parent` 时被**静默拆掉**，首帧 push 即
   `not-linked (-1)`。代码里固定模式是 `add_and_sync(pipeline, &all)` 然后才
   `link_chain(...)`。
2. **webrtcsink 的 sink pad 是 request pad**（`video_%u` / `audio_%u`），没有静态
   `sink` pad。从 tee 接过去必须 `request_ws_pad(ws, "video_%u")` 拿到 pad 再
   `link_tee_to_pad`；对它调 `static_pad("sink")` 恒为 `None`。
3. **tee 的分支数无上限**：每次 `request_pad_simple("src_%u")` 得一个新出口，
   buffer 只加引用计数不复制；但任一支路阻塞会拖死全部，所以慢消费支路必须
   自带 `queue(leaky)`。
4. `rtspsrc` 的媒体 pad 是动态 pad，统一由 `pad-added` 信号分发到
   `link_video` / `link_audio`；同类型第二路 track 会被拒绝并忽略（单路设计）。

## 重连与状态机

会话由独立线程驱动（`spawn_session_loop`），watch bus：

- bus ERROR / EOS / rtspsrc 超时 → `Reconnecting`，按
  `initial_delay * 2^n`（封顶 `max_delay`，默认 1s→30s）退避后**整条管线重建**
  （含 taps 的 Arc 闭包复用，不丢回调注册）。
- 401/Unauthorized → 不重试，直接 `Failed`。
- `handle.stop()` → 打断退避，`Ended`。`stop` 幂等。
- 统计探针挂在 depay/parse 出口：视频数 AU，音频数 depay 输出帧。

## RawTaps 原始帧出口

给原生 GUI / 录像 / 分析用的解码帧回调：

```rust
let taps = RawTaps {
    video: Some(Arc::new(Mutex::new(move |f: RawVideoFrame| {
        // f.data: &[u8]，RGBA，stride 可能 > width*4（对齐 padding）
    }))),
    audio: Some(Arc::new(Mutex::new(move |c: RawAudioChunk| {
        // c.data: &[u8]，交错 S16LE；c.rate / c.channels
    }))),
};
let handle = ipcam_gst::start_with_taps(cfg, taps)?;
```

### 生命周期与线程规则（务必遵守）

- **`data` 只在回调期间有效**——它借用 GStreamer 内部 buffer（零拷贝），回调返回即
  unmap。Rust 的 `'a` 生命周期在编译期禁止把它存出去；要跨线程用，**回调里自己拷贝**
  （`to_vec()` / 上传纹理）。
- **回调跑在 GStreamer 流线程上**：不能直接碰 UI 状态，用 channel 转发给 UI 线程。
- **回调期间持有 sink 的 Mutex**：回调里不要再去锁同一把锁（比如等 UI 线程而 UI
  线程正在持锁换 sink），会自死锁。parking_lot 锁不可重入。
- **回调不能 panic**：panic 会 unwind 穿过 GStreamer 的 C ABI trampoline，
  Rust 1.81+ 直接 abort 整个进程。用户回调内自己做好错误处理。
- 解码器是软解（`avdec_*`，CPU）；`videoconvert` 的 YUV→RGBA 是主要开销
  （720p@24fps ≈ 88MB/s 内存写）。appsink 侧 `drop=true + max-buffers=2`，
  消费慢就丢帧，不累积延迟。
- 每路 tap 增配一条解码支路；`None` 时管线与无 tap 完全一致，零额外成本。

### 实机验证

```bash
cargo run -p ipcam-gst --example rawtap -- \
  rtsp://admin:changeme@192.168.1.10:8554/ch01 8 target/tmp/rawtap.ppm
# 输出每秒 编码帧/解码帧 计数，并把第 30 帧落成 PPM 供肉眼检查
```

## 调试

```bash
# 管线拓扑导出（Graphviz .dot，纯按 link 关系绘制，与 add 顺序无关）
GST_DEBUG_DUMP_DOT_DIR=/tmp/dots media_talk_rust serve ...

# GStreamer 内部日志
GST_DEBUG=3                   # 全局
GST_DEBUG=webrtcsink:5        # 单模块

# 元素能力查询（排 "missing element" / pad 模板问题）
gst-inspect-1.0 webrtcsink
gst-inspect-1.0 rtspsrc
```

## 平台依赖速查

| 平台 | 需要 |
| --- | --- |
| 构建（所有） | GStreamer + pkg-config（Windows：官方 MSVC runtime + devel，`bin` 在 PATH 最前） |
| 运行（基础） | plugins-good（rtspsrc、rtp depay）、plugins-base（audioconvert 等） |
| 运行（AAC） | gst-libav（`avdec_aac`） |
| 运行（板端放音） | gstreamer1.0-alsa（`alsasink`） |
| 运行（WebRTC） | rswebrtc（webrtcsink 插件） |
