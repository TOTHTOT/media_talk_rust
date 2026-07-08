# media_talk_rust

`media_talk` 是某嵌入式厂商 Linux 设备上跑的网络摄像头媒体服务。在局域网内通过 ONVIF
发现 IP 摄像头，拉 RTSP 流，做（可选）硬件解码，把 H.264 重新打成 fMP4，通过
WebSocket 推到浏览器里用 `MediaSource` 播放。

- 目标板：`radxa-cm3-rpi-cm4-io`（SoC `rk356x`，aarch64，GNU libc 2.31）
- 工具链：`cargo zigbuild --target aarch64-unknown-linux-gnu.2.31 --release`
- 运行时：板端 `librockchip_mpp.so` 必须位于 `LD_LIBRARY_PATH`

## 仓库结构

```
media_talk_rust/
├── Cargo.toml              # workspace
├── crates/
│   ├── media_talk/        # 二进制入口 (clap subcommands)
│   ├── ipcam-core/         # 共享类型 + Decoder/Sink trait
│   ├── ipcam-discovery/    # ONVIF WS-Discovery + Device Management
│   ├── ipcam-rtsp/         # RTSP 客户端 + RTP/H.264 解包
│   ├── hardware-decode/    # Rockchip MPP (hw-decode) + SoftwareDecoder (sw-decode)
│   ├── web-display/        # axum + WS + fMP4 muxer
│   ├── ipcam-alsa/         # ALSA 设备枚举 / PCM（仅 Linux）
│   └── v4l2-device-cap/    # V4L2 capture 能力探测（仅 Linux）
├── web/                    # 前端 index.html + play.js
├── scripts/smoke.sh        # 烟雾测试
├── media_talk.service     # systemd unit
└── openspec/changes/       # OpenSpec change specs
```

## Quickstart

### 1. 开发主机（x86_64 Windows / Linux / macOS）

```bash
# 编译 + 跑测试（不需要 RKMPP / ALSA / V4L2）
cargo build --workspace --features sw-decode
cargo test  --workspace --features sw-decode

# 列出本机音频 / V4L2 设备（仅 Linux 下有内容）
cargo run --bin media_talk -- audio list-devices
cargo run --bin media_talk -- v4l2 list
```

### 2. 交叉编译到板端

```bash
rustup target add aarch64-unknown-linux-gnu
choco install zig                 # Windows
brew install zig                  # macOS
cargo install cargo-zigbuild

cargo zigbuild --target aarch64-unknown-linux-gnu.2.31 \
               --release --features hw-decode

# 产物：target/aarch64-unknown-linux-gnu.2.31/release/media_talk
```

### 3. 部署到板端

```bash
# 上传二进制 + systemd unit
scp target/aarch64-unknown-linux-gnu.2.31/release/media_talk \
    radxa@radxa-cm3-rpi-cm4-io:/usr/local/bin/
scp media_talk.service radxa@radxa-cm3-rpi-cm4-io:/etc/systemd/system/

# 板端需要 librockchip_mpp.so（板子自带的 /usr/lib/ 里）
ssh radxa@radxa-cm3-rpi-cm4-io
sudo systemctl daemon-reload
sudo systemctl enable --now media_talk.service
journalctl -u media_talk -f
```

### 4. 浏览器播放

打开 `http://<板子>:8080/`，左侧列出发现的摄像头，点 profile 后右侧 `<video>` 走
`MediaSource` 拉 WebSocket 上的 fMP4 帧。

## CLI 子命令

```bash
media_talk discover \
  --timeout-secs 5 \
  --username admin --password changeme        # 可选 ONVIF 凭据
media_talk serve --bind 0.0.0.0:8080 \
  --username admin --password changeme
media_talk decode-bench path/to/clip.h264 --max-frames 300
media_talk audio list-devices
media_talk v4l2 list
```

## 已知问题（known-issues）

1. ~~RTSP Digest 鉴权：~~ **已实现**（task 7.8.1）。`RtspConfig::with_credentials(u,p)`
   传入凭据；`send_method` 收到 401 + `WWW-Authenticate: Digest ...` 时按
   RFC 7616 计算 `HA1/HA2/response`，自动重发原请求。覆盖 qop=auth 与无 qop 两种
   形态；7.8.1 的 7 个 unit test + 1 个 e2e fake-server 测试保证字段与触发顺序正确。
2. **暂停**：当前实现 `OPTIONS / DESCRIBE / SETUP / PLAY / TEARDOWN` 五种 RTSP
   方法，未实现 `PAUSE`。
3. **音频**：v1 范围只把 RTP 视频流送到浏览器；远端摄像头的 AAC 仍通过
   `on_audio` stub 丢弃（`rtp::H264Depacketizer` 在 7.7.6 已重写，但 AAC 解复
   用属于 v2 范围）。本机 ALSA 仅完成设备枚举 + 错误包装，未做 PCM 抓放。
4. **重连**：当前 `play_loop` 在 socket 断开时返回 `TransportClosed`，由外层
   `web_display::stream::spawn_streaming` 决定是否重试（v1 行为：失败即结束
   session）。退避式自动重连列入 stretch。
5. **MPP FFI**：`bindgen` 由板端 `mpp-bindgen` 子命令在板子目录里生成；dev
   主机上是 stub，无真实 FFI 调用。
6. **H.265**：fMP4 复用器只写 H.264 路径；HEVC 在 v2 加入。
7. **WebRTC / metrics / `/api/sessions` 鉴权 / CI**：列入 8.x stretch，未实现。

## OpenSpec 工作流

1. `/opsx:explore` 探索项目并梳理项目框架
2. `/opsx:propose <name>` 实现某个功能，会生成 `proposal.md` / `design.md` /
   `tasks.md` / `specs/**/*.md`；先看 `proposal.md` / `tasks.md` 是否描述准确
3. `openspec validate <name> --strict` 严格校验
4. `/opsx:apply <name>` 核对任务清单无误后开始写代码（项目名 = `./openspec/changes/` 下的目录名）
5. `/openspec:archive <name>` 归档项目，避免切换终端后记忆丢失
6. `/opsx:onboard` 老项目用这个生成项目规范

## 环境配置（保留原内容以便回溯）

1. 常规安装 rust。
2. 配置交叉编译环境：
   - 安装编译工具：`rustup target add aarch64-unknown-linux-gnu`
   - 安装 zig 实现轻量级交叉编译：`choco install zig` + `cargo install cargo-zigbuild`
   - 编译命令：`cargo zigbuild --target aarch64-unknown-linux-gnu.2.31 --release`
3. Windows 开启 SMB，板子挂载：
   - 对需要的文件夹开启共享，用户使用 `Everyone`，勾选完全控制权限。
   - 板子输入 `sudo mount -t cifs //192.168.1.17/share ~/win_share -o username=账号,password=密码,iocharset=utf8,uid=radxa,gid=radxa,rw`。
4. OpenSpec 配置：
   - `npm install -g @fission-ai/openspec@latest`
   - `openspec init --tools claude`

## 需要实现的功能（原需求拆解）

1. [x] 搜索网络摄像头, 并拿到音视频推流数据. 会使用到 v4l2, alsa, rtsp, onvif
   - [x] 通过硬件解码（v1 范围仅 H.264 baseline；MPP FFI 由板端 mpp-bindgen 生成）
   - [x] 显示到 web（fMP4 over WebSocket + MediaSource）
