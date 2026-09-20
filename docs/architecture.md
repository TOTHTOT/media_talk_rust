# 整体架构

`media_talk_rust` 是跑在 Linux 设备（目标板 `radxa-cm3-rpi-cm4-io`，SoC rk356x，
aarch64）上的网络摄像头媒体服务：局域网内 ONVIF 发现摄像头、GStreamer 拉 RTSP 流、
通过 WebRTC（webrtcsink）转发给浏览器播放，同时支持板端扬声器出声和原生 GUI 取帧。

## Workspace 构成

```text
crates/
├── media_talk_rust/        # 二进制入口（clap 子命令：discover / serve / decode-bench / audio / v4l2）
├── ipcam-core/         # 共享类型 + Decoder/Sink trait（不依赖任何兄弟 crate）
├── ipcam-discovery/    # ONVIF WS-Discovery 探活 + Device Management（GetProfiles/GetStreamUri）
├── ipcam-gst/          # GStreamer 拉流与转发引擎（核心，见 streaming-engine.md）
├── hardware-decode/    # 视频解码抽象：Rockchip MPP（hw-decode）+ 软解 stub（sw-decode）
├── web-display/        # axum HTTP + WebSocket，前端静态资源与会话管理
├── ipcam-alsa/         # ALSA 设备枚举 / PCM 播放（Linux；其它平台为 stub）
├── v4l2-device-cap/    # V4L2 采集能力探测（Linux；其它平台为 stub）
└── ipcam-sip/          # SIP 对讲（占位，规划中）
```

**依赖方向**：上层 → 下层，只允许单向。`ipcam-core` 是所有 crate 的公共底座；
`media_talk_rust`（二进制）处于最顶端，串联 discovery / gst / web-display。
`ipcam-sip` 为楼宇对讲功能预留，尚无任何实现。

## 运行时数据流

```text
                UDP 3702 multicast
 IP Camera ───────────────────────▶ ipcam-discovery（WS-Discovery probe）
     │                                     │
     │        ONVIF GetProfiles/           │ 拿到 RTSP URI + 凭据校验
     │        GetStreamUri                 ▼
     │                              media_talk_rust serve
     │ RTSP (TCP/UDP)                      │
     └─────────────────────────────▶ ipcam-gst 管线
                                     rtspsrc → depay → parse → tee
                                          │            │
                        webrtcsink ◀──────┘            └──────▶ RawTaps（可选）
                              │                                 解码帧 → 回调（GUI/分析）
                              │ WebRTC
                              ▼
                    浏览器（web/ 前端，经 ws://:8443 信令）

  音频支路（有音轨时）：depay → decode → tee ─┬─ opusenc → webrtcsink（浏览器）
                                             ├─ alsasink（--audio-out，板端扬声器）
                                             └─ appsink（tap，PCM 回调）

  web-display：http://:8080 提供前端页面与设备/会话 REST API
  信令服务器：ws://:8443，进程级单例（ensure_signalling_server）
```

要点：

- **视频零转码**：相机 H.264/H.265 直通浏览器，板端 CPU 只做过路。
- **音频必转码**：浏览器只收 Opus，相机发 G.711/AAC，所以音频走"解码 → tee → opus 重编码"。
- **一次拉流多处消费**：tee 分流，web / 本地播放 / tap 互不阻塞（tap 支路 leaky queue）。

## 构建与依赖约定

- **Rust 版本**：1.85+，edition 2024；workspace 统一 `[workspace.package]` 继承。
- **依赖版本全部收编在根 `Cargo.toml` 的 `[workspace.dependencies]`**，叶子 crate 一律
  `xxx = { workspace = true }` 引用，禁止在叶子里写版本号（oxvif 这类钉版注释也随版本
  写在上层）。
- **GStreamer 是无条件依赖**：构建机必须有 GStreamer + pkg-config（Windows 用官方
  MSVC 安装包并把 `bin` 放 PATH 最前；交叉编译配 `PKG_CONFIG_SYSROOT_DIR`）。
- **特性开关**：`sw-decode`（默认）/ `hw-decode`（板端 MPP，`hardware-decode`）。
- **平台 stub**：`ipcam-alsa` / `v4l2-device-cap` 在非 Linux 平台编译为 stub，
  上层可以无条件依赖，无需 `cfg` 隔离。

## 开发约定

- 提交信息：`[类型] 描述`，类型 ∈ 新增 / 修复 / 重构 / 优化（见 `git log`）。
- 合并前必须本地通过：`cargo fmt --all -- --check`、`cargo clippy --workspace -- -D warnings`、
  `cargo test --workspace`。
- CI（`.github/workflows/rust.yml`）：fmt / clippy / test / cross-check 四个 job，
  Ubuntu 上装 `libasound2-dev pkg-config libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev`。
- 日志统一 `tracing`，库 crate 不打印 stdout。
- 排障工具：`GST_DEBUG` 环境变量、`GST_DEBUG_DUMP_DOT_DIR` 管线拓扑导出、
  `cargo run -p ipcam-gst --example rawtap` 实机验证 tap 链路。

## 端口与配置

| 端口 | 用途 | 来源 |
| --- | --- | --- |
| 8081 | web-display HTTP/WS（前端 + API） | `serve --bind`（默认值） |
| 8443 | WebRTC 信令服务器（进程级单例） | `ensure_signalling_server` |

相机凭据通过 `--username/--password`（发现 + 拉流共用）或直接 `--rtsp-url` 绕过 ONVIF。
