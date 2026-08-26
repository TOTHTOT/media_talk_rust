# media_talk_rust

[![Rust CI](https://github.com/TOTHTOT/media_talk_rust/actions/workflows/rust.yml/badge.svg)](https://github.com/TOTHTOT/media_talk_rust/actions/workflows/rust.yml)

`media_talk` 是某嵌入式厂商 (mediatalk) Linux 设备上跑的网络摄像头媒体服务。它在局域网内
通过 ONVIF 发现 IP 摄像头、拉 RTSP 流、做 (可选) 硬件解码、把 H.264 重新打成
fMP4 通过 WebSocket 推到浏览器里用 `MediaSource` 播放。

- **目标板**: `radxa-cm3-rpi-cm4-io` (SoC `rk356x`,aarch64,GNU libc 2.31)
- **工具链**: `cargo zigbuild --target aarch64-unknown-linux-gnu.2.31 --release`
- **运行时**: 板端 `librockchip_mpp.so` 必须位于动态链接器搜索路径 (见
  `media_talk.service` 注释)

## 仓库结构

```text
media_talk_rust/
├── Cargo.toml              # workspace
├── crates/
│   ├── media_talk/        # 二进制入口 (clap subcommands)
│   ├── ipcam-core/         # 共享类型 + Decoder/Sink trait
│   ├── ipcam-discovery/    # ONVIF WS-Discovery + Device Management
│   ├── ipcam-gst/          # GStreamer RTSP 拉流引擎
│   ├── hardware-decode/    # Rockchip MPP (hw-decode) + SoftwareDecoder (sw-decode)
│   ├── web-display/        # axum + WS + fMP4 muxer
│   ├── ipcam-alsa/         # ALSA 设备枚举 / PCM (仅 Linux)
│   └── v4l2-device-cap/    # V4L2 capture 能力探测 (仅 Linux)
├── web/                    # 前端 index.html + play.js
├── scripts/smoke.sh        # 烟雾测试
├── media_talk.service     # systemd unit
├── .github/workflows/      # CI: fmt / clippy / test / cross-check
└── openspec/changes/       # OpenSpec change specs
```

## Quickstart

### 1. 开发主机 (x86_64 Linux / macOS / Windows)

```bash
# 编译 + 跑测试 (不需要 RKMPP / ALSA / V4L2)
cargo build --workspace --features sw-decode
cargo test  --workspace --features sw-decode

# 列出本机音频 / V4L2 设备 (仅 Linux 下有内容)
cargo run --bin media_talk -- audio list-devices
cargo run --bin media_talk -- v4l2 list
```

### 2. 交叉编译到板端

```bash
rustup target add aarch64-unknown-linux-gnu
# Windows:
choco install zig
# macOS:
brew install zig
cargo install cargo-zigbuild

cargo zigbuild --target aarch64-unknown-linux-gnu.2.31 \
               --release --features hw-decode

# 产物: target/aarch64-unknown-linux-gnu.2.31/release/media_talk
```

### 3. 部署到板端

```bash
# 上传二进制 + systemd unit
scp target/aarch64-unknown-linux-gnu.2.31/release/media_talk \
    radxa@radxa-cm3-rpi-cm4-io:/usr/local/bin/
scp media_talk.service radxa@radxa-cm3-rpi-cm4-io:/etc/systemd/system/

# 板端需要 librockchip_mpp.so
# 注意: 不能用 LD_LIBRARY_PATH (systemd 沙箱会吞),改走 ld.so.conf.d:
ssh radxa@radxa-cm3-rpi-cm4-io
echo '/opt/mediatalk/lib' | sudo tee /etc/ld.so.conf.d/mediatalk.conf
sudo ldconfig

# 专用账户 (service file 默认以 mediatalk 身份运行)
sudo useradd --system --home /var/lib/media_talk --shell /usr/sbin/nologin mediatalk

sudo systemctl daemon-reload
sudo systemctl enable --now media_talk
journalctl -u media_talk -f
```

### 4. 浏览器播放

打开 `http://<板子>:8080/`,左侧列出发现的摄像头,点 profile 后右侧 `<video>` 走
`MediaSource` 拉 WebSocket 上的 fMP4 帧。

## CLI 子命令

| 子命令 | 作用 |
| --- | --- |
| `discover` | WS-Discovery 探活,可带凭据验证 ONVIF 凭据有效性 |
| `serve` | 启 HTTP/WS 服务器,前端 + 摄像头流聚合 |
| `decode-bench` | 单文件 H.264 → 解码器 → 吞吐估算 (调试解码器性能用) |
| `audio list-devices` | ALSA 设备枚举 (仅 Linux) |
| `v4l2 list` | V4L2 capture 设备能力 (仅 Linux) |

```bash
media_talk discover --timeout-secs 5 --json \
  --username admin --password changeme

# 标准 ONVIF 走法
media_talk serve --bind 0.0.0.0:8080 \
  --username admin --password changeme

# 绕过 ONVIF,直接喂 RTSP URL (允许多次传入多路)
media_talk serve --bind 0.0.0.0:8080 \
  --rtsp-url rtsp://admin:changeme@192.168.1.10/Streaming/Tracks/101
media_talk serve --bind 0.0.0.0:8080 \
  --rtsp-url rtsp://camera-a/track1 --rtsp-url rtsp://camera-b/track1

media_talk decode-bench path/to/clip.h264 --max-frames 300
```

## GStreamer 拉流

拉流引擎 `ipcam-gst` (rtspsrc → depay/parse → appsink, 替代旧 `ipcam-rtsp::play_loop`)
是**无条件依赖**——构建机必须装有 GStreamer + pkg-config。

- **目标板运行时**: 需要 `gstreamer1.0-plugins-good` (rtspsrc / rtp depay)、
  `gstreamer1.0-plugins-base` (audioconvert 等)、`gstreamer1.0-alsa` (alsasink);
  AAC 扬声器播放另需 `gstreamer1.0-libav` (`avdec_aac`)。
  用 `gst-inspect-1.0 rtspsrc` 等逐个验证,缺元件时程序以
  `GstStreamError::Init` 报错并指明元件名。
- **Windows 开发机调试**: 装 GStreamer 官方 MSVC runtime + devel 两个 MSI,
  把 `C:\gstreamer\1.0\msvc_x86_64\bin` 放到 PATH **最前** (自带 pkg-config.exe
  必须优先),然后 `cargo build -p media_talk` 即可。
- **交叉编译 (WSL2/Docker 内)**: sysroot 含 `libgstreamer1.0-dev` 与
  `libgstreamer-plugins-base1.0-dev` (可直接用目标板 rootfs),然后:

```bash
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR=/path/to/rk356x-sysroot
export PKG_CONFIG_PATH=$PKG_CONFIG_SYSROOT_DIR/usr/lib/aarch64-linux-gnu/pkgconfig
cargo build --release --target aarch64-unknown-linux-gnu
```

板端扬声器播放摄像机现场声音 (G.711/AAC 管线内解码):

```bash
media_talk serve --rtsp-url "rtsp://..." --audio-out hw:0,0
```

断流自动重连默认开启: bus ERROR/EOS/RTSPSrcTimeout 后按 1s→30s 指数退避
重建整条管线,无限重试; 401 认证失败不重试直接标记 `Failed`。

## 架构

```text
+----------+   UDP/Multicast   +----------------+   UDP/Unicast  +-----------+
|  ONVIF   |  WS-Discovery    | ws_discovery    |                |   IP      |
| Camera ──┼─────────────────▶│ probe loop      │                |  network  |
|          |  3702            +----------------+                │           │
+----------+                   │ xaddr + auth    │                │           │
                              ▼                  ▼                │           │
                              +------------------+   RTSP        │           │
                              │ DeviceManagement │   ───────────▶ +-----------+
                              │ (GetProfiles,    │
                              │  GetStreamUri)   │   OPTIONS/DESCRIBE/SETUP/PLAY
                              +------------------+
                                            │
                                            ▼
+----------+    EncodedPacket    +-----------+    DecodedFrame    +-------------+
|  rtspsrc | ───────────────────▶| Hardware  | ──────────────────▶|  fMP4 muxer |
|  depay   |  ipcam-gst (gst)   | Decoder   |  ipcam-core        | (web-display)|
|  appsink |                    |  (MPP)    |                    | + axum + WS |
+----------+                    +-----------+                    +------+------+
                                                                         │
                                                                         ▼
                                                                  +------+------+
                                                                  |  Browser    |
                                                                  |  MSE/Player |
                                                                  +-------------+
```

**运行时特性:**

- **发现层** — `ipcam-discovery` 在每块非 loopback IPv4 网口分别绑 UDP socket
  发 multicast probe,合并所有 ProbeMatch;带凭据时再并发去做 `GetProfiles`
  验证账号可读。
- **拉流层** — `ipcam-gst` 用 GStreamer `rtspsrc` 动态分流 +
  depay/parse + appsink 回调,音视频统一走管线;断流自动退避重连。
- **解码层** — `hw-decode` 板端用 Rockchip MPP (目标板 FFI 由 `mpp-bindgen`
  子命令板子上生成),其它平台走 `SoftwareDecoder` stub;通过 `Decoder` trait
  抽象。
- **呈现层** — `web-display` 把 NAL 拼成 access unit 后整组写进 `mdat`
  (这是 MSE H.264 唯一接受的样本格式),`init segment` 走 `avcC` 自动嵌
  SPS/PPS,WebSocket 推流。

## OpenSpec 工作流

1. `/opsx:explore` — 探索项目并梳理项目框架
2. `/opsx:propose <name>` — 提议变更,会生成 `proposal.md` / `design.md` /
   `tasks.md` / `specs/**/*.md`
3. `openspec validate <name> --strict` — 严格校验
4. `/opsx:apply <name>` — 按 tasks 清单逐项实现
5. `/opsx:archive <name>` — 变更归档

## 已知问题

1. ~~RTSP Digest 鉴权~~ **已实现** (7.8.1)。`RtspConfig::with_credentials(u,p)`
   传入凭据;`send_method` 收到 401 + `WWW-Authenticate: Digest ...` 时按
   RFC 7616 计算 `HA1/HA2/response` 自动重发原请求,覆盖 qop=auth 与无 qop
   两种形态。
2. **暂停** — 当前实现 `OPTIONS / DESCRIBE / SETUP / PLAY / TEARDOWN`,
   未实现 `PAUSE`。
3. **音频** — `gst` 路径下音频帧经 `AudioPacket` 回调暴露,`--audio-out` 可在
   管线内解码播到板端扬声器 (G.711/AAC); 网页 fMP4 音轨仍未做 (MSE 兼容性
   评估后另行扩展 muxer)。非 gst 构建无拉流能力。
4. **重连** — `gst` 路径下 bus ERROR/EOS 触发指数退避整管线重建 (1s→30s,
   默认无限); 旧自研 `play_loop` 已随 T018 退役删除。
5. **MPP FFI** — `bindgen` 由板端 `mpp-bindgen` 子命令在板子目录里生成;
   dev 主机上是 stub,无真实 FFI 调用。
6. **H.265** — fMP4 复用器只写 H.264 路径;HEVC 在 v2 加入。
7. **WebRTC / metrics / `/api/sessions` 鉴权** — 列入 8.x stretch,未实现。

## License

仓库内尚未声明 license。所有权保留,在补 `LICENSE` 文件前请勿重新分发。
