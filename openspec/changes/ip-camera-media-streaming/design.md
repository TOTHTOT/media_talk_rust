## Context

- 仓库当前仅有 `Cargo.toml`（裸）、`src/main.rs`（"Hello, world!"）与一份中文 readme（功能期望），无任何 Rust 代码骨架，无 CI，无 workspace。
- 目标板：`radxa-cm3-rpi-cm4-io`，SoC 为 `rk356x`，OS 为 `Linux 5.10.160-19-rk356x`，板子 `uname -a` 显示 aarch64、GNU 用户空间。开发机是 Windows 11 + `bash`（本会话环境）。
- readme 已明确功能期望（搜索网络摄像头并拉到音视频 → 硬件解码 → 显示到 web）与工具链（`cargo zigbuild --target aarch64-unknown-linux-gnu.2.31 --release` 通过 zig）。
- 工具链/部署前提：板子挂载 Windows SMB 共享做开发挂载；目标机网络下 ONVIF 摄像头已通过路由器可达。
- 既有 OpenSpec 状态：`openspec/` 已初始化，`openspec/specs/` 为空，没有现存 capabilities，无冲突。
- 关键约束：硬件解码依赖 Rockchip MPP（`librockchip_mpp`），需要在交叉编译场景下正确链接运行库；板上可能没有 H.265 解码授权（mp1 之后一般都有）。

## Goals / Non-Goals

**Goals：**
1. 在本仓库落地一条端到端的纵向链路：发现（局域网单播/ONVIF Probe）→ RTSP 拉取一帧 H.264 → MPP VPU 解码 → NV12 帧重封为 fMP4 → 通过 WebSocket 推到浏览器播放。
2. 关键子系统以独立 crate 形式分离（`ipcam-discovery`、`ipcam-rtsp`、`hardware-decode`、`web-display`、`ipcam-alsa`、`v4l2-device-cap`、`ipcam-core`），便于单测和未来替换实现。
3. 提供 workspace + `build.rs`（`bindgen` RKMPP 头）+ 软件解码回退，使开发者 Win/Mac/Linux 主机也能逐 crate 编译运行测试。
4. 把 `media_talk_rust` 的当前 hello world 替换为带子命令的 `media_talk` 二进制，覆盖 `discover`、`serve` 两个子命令。

**Non-Goals：**
- 不实现 WebRTC。
- 不实现视频录制/录像点播。
- 不实现本地摄像头的 V4L2 拉流（只做能力探测）。
- 不实现 ONVIF 的事件订阅（仅 device management 子集）。
- 不实现 OAUTH / 对接某嵌入式厂商云账号 / 云端录制。
- 不做回放控制条（play/pause UI 不在本期内）。

## Decisions

### D1. 运行时形态：tokio + 多 actor + 通道
- **决策**：所有 IO / 网络 / ONVIF 都跑在 `tokio` runtime 上；解码（涉及阻塞 C 调用 + DRM 操作）跑在一个独立 `tokio` task 里，通过 `mpsc::channel` 接收压缩帧。
- **备选**：每子系统用独立线程 + `crossbeam_channel`。
- **理由**：tokio 与 axum/ws 是一脉的，跨 crate 共享超时/取消模型简单；解码 task 可用 `spawn_blocking` 兜底。
- **影响**：所有 IO 代码 MUST 是 `Send + 'static`；crate 内部状态 SHALL 用 `Arc<Mutex/RwLock>`。

### D2. 硬件解码：直接调用 MPP，而非 FFmpeg-rockchip
- **决策**：`hardware-decode` crate 直接 `bindgen` `rockchip_mpp.h`，用 `mpp_create_decoder` + `mpp_decode_put_packet/get_frame` 走 VPU；不依赖 FFmpeg。
- **备选 1**：依赖 FFmpeg 的 `rkmpp` 解封装器。
- **备选 2**：自研 + gstreamer 的 `mppvideodec`。
- **理由**：依赖少、可控性高；FFmpeg 路径会引入 GPL/LGPL 决策与编解码器冲突。代价是需要手写 NALU 入队的去除开销代码，但量小。
- **影响**：在头文件中出现 RKMPP 类型多、API 不易 mock ⇒ 软件 fallback 由 `openh264` / `dav1d` crate 抽象同形态的 `Decoder` trait，背后可选 MPP 或软件实现。

### D3. 抽象 `Decoder` trait + 软件 fallback
```rust
#[async_trait]
pub trait Decoder: Send + Sync {
    async fn submit(&self, packet: EncodedPacket) -> Result<DecodedFrame>;
    fn set_recovery_strategy(&mut self, s: RecoveryStrategy);
}
```
- **理由**：开发机无板子系统也能跑；可在 CI 里跑软件解码跑通 RTSP → MP4 → MSE 全链路。
- **备选**：仅 MPP 路径、CI 里跳过 e2e。否决：开发体验差。

### D4. Web 端：fMP4 over WebSocket（v1），不引入 WebRTC
- **决策**：每路会话一个 WS，服务端把每个 GOP 切为 `ftyp` + `moof` + `mdat` 推下去，浏览器用 `MediaSource` 拉。
- **理由**：纯浏览器能力、不需要 stun/TURN、可以穿越同一局域网/网关；端到端 ~1–2 秒延迟满足"实时查看"诉求。
- **备选**：WebRTC（~250 ms，但需要 coturn/P2P 路径，复杂）。
- **影响**：客户端用 `MSE`，需要首帧拉 init segment + 第一个 GOP 才能播放；带宽高（无 SRTP、无相机原始码率）。

### D5. 发现协议：ONVIF WS-Discovery（核心）+ 可选 mDNS
- **决策**：v1 仅实现 ONVIF WS-Discovery + Device Management；mDNS 留作后续可选发现（`ipcam-discovery` 保留 trait 接口）。
- **理由**：海康/大华/宇视摄像头普遍支持 ONVIF；一个协议覆盖 90% 场景，开发量可控。
- **影响**：非 ONVIF 摄像头必须手动加 URI。

### D6. 解码器 fallback 与解码器热切换
- **决策**：每个 `MediaSession` 持有一个 `Decoder`。MPP 路径在某些异常（如 `MPP_ERR_TIMEOUT`、`MPP_ERR_STREAM` 超过 N 次）后自动重建一次，仍失败则把对应 session 标 `stalled`。

### D7. 工作空间结构

```
media_talk_rust/
├── Cargo.toml                      # workspace
├── crates/
│   ├── ipcam-core/                 # 通用类型与 trait：EncodedPacket、DecodedFrame
│   ├── ipcam-discovery/            # ONVIF WS-Discovery + Device Management
│   ├── ipcam-rtsp/                 # RTSP 客户端 + RTP/RTCP demux
│   ├── hardware-decode/            # MPP 绑定 + 软件 fallback
│   ├── web-display/                # axum + WS + fMP4 封装器
│   ├── ipcam-alsa/                 # ALSA 封装（Linux-only）
│   ├── v4l2-device-cap/            # V4L2 设备能力探测（Linux-only）
│   └── media_talk/                # 二进制入口（主程序）
└── openspec/...
```

### D8. 构建/部署
- Rust toolchain 默认 stable。
- 在板子目标上 `cargo zigbuild --target aarch64-unknown-linux-gnu.2.31 --release --features hw-decode`。
- 在开发机上 `cargo build --features sw-decode` 或 `--no-default-features --features sw-decode` 跑测试。
- 板上运行需要 `LD_LIBRARY_PATH` 包含板子自带的 `librockchip_mpp.so`；这事写在板端 systemd unit 里，不在仓库代码范围。

### D9. 数据流概览

```
+----------------+   Probe    +-----------+   Get*    +---------------+
|  application   │───UDP──────▶ ONVIF Dev │──SOAP────▶  IP Camera     │
|  (discover)    │            │ Mgmt      │           │ (XAddr)       │
+----------------+ ◀──List───── +---------+            +--------------+
                                                         │ RTSP URI
                                                         ▼
+----------------+           +-----------+   RTP    +---------------+
|  web-display   │──WS fMP4─▶│ media_talk│◀────────│ ipcam-rtsp    │
|  (browser MSE) │           │  (binary) │         │  client       │
+----------------+           +-----+-----+         +---------------+
                                 ▲   ▲                  ▲  ▲
            ┌────alac────────────┘   └────OMX─────────────┘
            ▼                                              ▼
+----------------+                                 +---------------+
| ipcam-alsa     │                                 │ hardware-decode│
| (本地音频流)   │                                 │  (MPP/software)│
+----------------+                                 +---------------+
```

### D10. 关键外部依赖（候选）

| 用途 | crate 选择 | 备注 |
|---|---|---|
| 异步 runtime | `tokio` (multi-threaded) | 已经在生态主流 |
| HTTP/WS | `axum` 0.7+ + `tokio-tungstenite` | 和 axum 同源 |
| ONVIF SOAP/WSD | `onvif-rs` 或自研小型 client（`soap-rs` + `quick-xml`） | 评估中 |
| RTSP | `rtsp-rs`（已停维护）或自研（建议） | 状态机简单，依赖少 |
| ALSA | `alsa` 0.9（`alsa-sys`） | 主流绑定 |
| V4L2 | `nix` + `v4l2-ctl` 头文件 + 自研最小绑定 | 用 `bindgen` 一次性产出 |
| MPP | `bindgen` 输出 + 手写 safe wrapper | 量大但可控 |
| fMP4 封装 | `mp4-iter` / `mp4-rust` 写 ISO BMFF boxes | 自实现约 300 行 |
| JSON | `serde` + `serde_json` | 标配 |
| 日志 | `tracing` + `tracing-subscriber` | 适配 systemd/journal |

### D11. 错误模型与失败处理

- 每个 crate 暴露自己的 `Error` enum，并实现 `From<XxxError> for CoreError`。
- 顶层 `tracing::error!` 输出到 stderr + （可选）板上 rsyslog。
- 所有长寿命任务（discovery、rtsp、decode）都跑在 `tokio::spawn` 里，外层 `supervisor` actor 在 `JoinError` 时按指数退避重启。

## Risks / Trade-offs

- [Risk] **RKMPP FFI binding 大量且 unsafe** → Mitigation：`hardware-decode` 中所有 MPP 类型用 `#[repr(transparent)]` newtype，unsafe 仅限一个 `MPPSession::decode` 内部函数；CI MUST 打开 miri 跑抽检任务。
- [Risk] **ONVIF 摄像头鉴权失败率高**（不同厂商密码摘要实现差异） → Mitigation：单元测试覆盖 `digest = base64(sha1( nonce + created + password ))` 三种 observed 反例；并在 UI/API 暴露明确 `auth_status`。
- [Risk] **zigbuild 链接失败（RDKMPP runlib）** → Mitigation：`build.rs` 检测 `PKG_CONFIG` 与 linker 输出，分支为 i) copy libs from sysroot；ii) 退化到 `cargo build --target` + 人工拷 `.so`。
- [Risk] **fMP4 切片时间窗抖动 → MSE reject** → Mitigation：以 GOP 边界切；插入 `emsg` / `mfhd` 标记；遵循 H.264/H.265/AV1 各自的 `CodecPrivateData` 注入。
- [Risk] **HEVC 码流实际是相机本地解压后的源 / raw** → Mitigation：把软件解码路径上同样跑 libav；让 CI 同时跑 H.264 baseline。
- [Risk] **WebSocket 推流在长时运行下内存累积** → Mitigation：服务端 `mpsc` 配 `bounded(64)`，客户端订阅器主动 `release`；fMP4 输出加 GC。
- [Risk] **板上音频 ALSA 设备路径不固定 / plughw 与 hw 差异** → Mitigation：枚举设备清单供 UI 选择；默认按 rank 排序。

## Migration Plan

- 仓库存量文件 `src/main.rs` 改为 `crates/media_talk/src/main.rs`（workspace root crate），原文件删除。
- `Cargo.toml` 由 root crate 升级为 workspace 声明。
- CI 第一版只跑 `cargo test --workspace --features sw-decode`，跳过 `hw-decode`（CI 无 aarch64 + MPP 开发包）。
- 部署：
  1. 本机开发 + zigbuild 产出 `target/aarch64-unknown-linux-gnu.2.31/release/media_talk`。
  2. `scp` 到 `radxa-cm3-rpi-cm4-io` `/usr/local/bin/`。
  3. 写入 `/etc/systemd/system/media_talk.service`，`Environment=LD_LIBRARY_PATH=...`；`systemctl enable --now`。
- 回滚：disable + stop 服务即停；旧版本二进制保留为 `media_talk.bak`。

## Open Questions

1. **`onvif-rs` vs 自研**：是否能接受现状依赖（API 抽象、皮亚克类型覆盖度）—— 需要 spike。
2. **首版只支持 H.264 还是同时 H.264 + H.265**：硬件授权差异；建议 v1 先 H.264 baseline，HEVC 留 GStreamer 后端。
3. **ws 协议形态**：`Message<Bytes>` 直接推送裸 ISO BMFF 字节，还是封一个最小的 1 字节 type header？倾向裸 bytes，推 ms 简化。
4. **多摄像头并发**：`mpsc` 还是 `DashMap` + 会话注册表？倾向 `DashMap<SessionId, Session>`。
5. **是否引入 GStreamer 作为可插拔 pipeline**：避免定义一套 wiring DSL，但 v1 直接手写 actor 即可。