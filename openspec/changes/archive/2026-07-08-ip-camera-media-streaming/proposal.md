## Why

 Linux 设备需要在其所在局域网内发现并接入网络摄像头（IP Camera），取得音视频流，并把这些流通过硬件解码后在 Web 端实时呈现给住户。当前 `media_talk_rust` 工程仅有空的 `src/main.rs`，本 change 在此 Rust 工程上端到端打通「**设备发现 → RTSP/ONVIF 拉流 → 硬件解码 → Web 实时显示**」的第一条纵向链路。

## What Changes

- 新增**网络摄像头发现**能力：基于 ONVIF WS-Discovery（UDP multicast `239.255.255.250:3702`）与 ONVIF Device Management 的 `GetCapabilities` / `GetProfiles` / `GetStreamUri` 接口，发现摄像头并获取 RTSP 取流 URI；记录发现的设备、profile、`XAddr`。
- 新增**RTSP 媒体拉流**能力：通过 ONVIF 取到的 RTSP URI 建立 RTSP 客户端，解析 SDP，接收 RTP/RTCP 报文，提取 H.264/H.265/AAC 等基本流。
- 新增**音频采集与播放**能力（设备本机音频）：通过 ALSA 在 Linux 上做本地音频输入/输出；本期先完成本地 ALSA 通路打通，便于后续与远端摄像头音频流混音。
- **v4l2** 列在本能力范围内但本期仅做依赖引入与设备能力探测，不实现具体采集通路（设备本机无摄像头场景）。
- 新增**硬件解码**能力：基于 Rockchip MPP（RK356x 平台）将 H.264/H.265 NAL 单元送入 VPU 解码，得到 NV12 帧；以 DRM-prime / mmap 与 `mpp_buffer_group` 共享解码输出。
- 新增**Web 实时显示**能力：后端用 axum 提供 HTTP/WS，前端浏览器通过 `MediaSource` 播放 fragmented MP4（fMP4），每个摄像头一个 session，端到端延迟约 1–2 秒。第一版暂不实现 WebRTC。
- 新增可执行目标 `media_talk` 与可重用的库 crates：`ipcam-discovery`、`ipcam-rtsp`、`ipcam-decode`、`ipcam-web`、`ipcam-alsa`、`ipcam-core`。

## Capabilities

### New Capabilities

- `ipcam-discovery`: 通过 ONVIF WS-Discovery 和 Device Management 发现局域网内网络摄像头，并输出摄像头清单（IP、profile、ONVIF token、RTSP URI、能力集）。
- `ipcam-rtsp`: RTSP 客户端，按 SDP 描述协商传输，建立 RTP/RTCP 会话并按基本流输出压缩视频帧（H.264/H.265 access unit）与音频帧（AAC）。
- `hardware-decode`: 将基本流（Annex-B / ADTS）送入 Rockchip MPP 硬件解码器，输出可在 Linux 上的渲染管线消费的原始帧（NV12/RGB）。
- `web-display`: 提供 HTTP API 与 WebSocket，按 fMP4 切片把解码+重封装的视频帧推送到浏览器，完成实时显示。
- `ipcam-alsa`: 封装 Linux ALSA 库的本地音频输入/输出，做到 PCM 流的读出与播放。
- `v4l2-device-cap`: 探测目标板 / 主机上 V4L2 capture 设备的能力，本期仅做能力枚举，不实现具体采集通路。

### Modified Capabilities

_（仓库当前没有 `openspec/specs/` 下的旧 capabilities，无修改项）_

## Impact

- 新增 crates（依赖项）：`tokio`、`axum`、`reqwest`、`rtsp-rs` 或自研 RTSP 状态机、`onvif-rs` 或自研 SOAP/WS-Discovery 客户端、`mpp`/`rockchip-mpp` 的 FFI 绑定 `bindgen` 产物、`alsa`/`alsa-sys`、`nix`（ioctl/mmap）、`tokio-tungstenite`（WS）、`bytes` / 自定义帧协议。
- 系统依赖：板端 `librknn_api` / `librockchip_mpp`、`libasound2`、`v4l2` 内核头文件、`libavformat` / `libavcodec` 可选（封 fMP4）。
- 构建：本期会调整 `Cargo.toml` 引入工作空间（workspace）、`build.rs` 用于 `bindgen`，并验证 `cargo zigbuild --target aarch64-unknown-linux-gnu.2.31 --release` 在 `target=radxa-cm3-rpi-cm4-io` 上的产物。
- 受影响代码：当前仅有 `src/main.rs`（"hello world"），本 change 将其替换为 `media_talk` 主入口；新增 `crates/*` 工作空间子 crate。
- 风险点：
  1. RKMPP 是 C API，需要为 `mpp_buffer`、`mpp_packet`、`mpp_dec_cfg`、`mpp_create_decoder` 等写安全包装，并做好 DRM-prime / `drm_prime` 的零拷贝路径（否则会回落到 CPU 解码）。
  2. ONVIF 鉴权（WS-Security UsernameToken，密码摘要）必须早期实现，否则设备列表会大片空白。
  3. 工具链 zigbuild 首次开通可能踩坑（链接 RKMPP 的 `.so` 在交叉场景需要拷到板子 `LD_LIBRARY_PATH`），本任务里要跑一次完整构建闭环。

## 实施过程中的发现（端到端联调后追加）

在 v1 范围代码全部写完后，**首条纵向链路未能在浏览器里渲染视频**。逐步走查后定位到以下结构性 bug，列入本 change 的修复范围（v1 必修）：

1. **`SessionRegistry::muxer()` 克隆而非共享**（P0）
   当前实现 `let mux = kv.muxer.lock().clone(); parking_lot::Mutex::new(mux)`——每次返回的 `Fmp4Muxer` 是 *深克隆*，与 registry 持有的 `entry.muxer` 完全是两个实例。`spawn_streaming` 把 SPS/PPS 与 moof/mdat 推到克隆上；`ws_loop` 通过 `init_segment_for` / `take_segments_since` 读 `entry.muxer`（原始那份）。**两边不共享任何状态**。后果：浏览器 15 秒内拿不到 init 段（`is_ready()` 永远 false），触发 `POLICY` close，画面黑屏。
2. **fMP4 sample 语义错（P0）**
   `mux.rs` 把每个 NAL 单元当作一个独立的 fMP4 sample。MSE 期望"一个 sample = 一个完整 access unit"（即可独立解码的一帧）。一帧 H.264 通常含 IDR + SEI + 多 slice NAL；按当前实现会输出数个"半帧"sample，浏览器 H.264 decoder 无法解码。修复方向：攒 NAL 到一个 `Vec<Bytes>`，遇 RTP marker=1 或 NAL 类型 5/7/8 出现时 flush，flush 时一次性写一个 moof+mdat，mdat 内放 length-prefixed NAL 序列，trun 的 `sample_count` 等于该帧 NAL 数。
3. **`trun.data_offset = 0`（P0）**
   正确值是 mdat 内首个 sample 的偏移（=8，跳过 mdat 头）。当前 0 让浏览器把 mdat 头（4 字节 size + 4 字节 'mdat'）当成长度前缀去解析 NAL，长度全错位。
4. **`trun.flags` 与 body 不一致（P0）**
   flags = `0x000F0003` 声明了 5 个字段，但 body 只写 `data_offset` + `sample_size` 两个。flags 与 body 字段数必须一致；按实际写的字段缩减 flags。
5. **RTSP digest 鉴权缺失（P1）**
   `send_method` 不处理 401 后的 `Authorization: Digest` 重试。多数 ONVIF 摄像头在 RTSP 层会再要 digest。ONVIF `GetStreamUri` 已通过 SOAP 鉴权，URL 本身可用，但 RTSP 握手可能再被 401。修复方向：捕获 401 + `WWW-Authenticate: Digest ...`，按 RFC 2617 / RFC 7616 计算 response，重发原请求。
6. **SETUP `mode=record` 应为 `mode=play`（P1）**
   `RTP/AVP/TCP;interleaved=0-1;mode=record` 是摄像头向 server 推流方向；server 拉流应为 `mode=play`（或省）。多数相机容忍，但严格相机可能拒。
7. **`read_response` 不按 Content-Length 切 body（P2）**
   当 server 响应无 Content-Length 头时，会把后续 RTP 数据当 body。一般 ONVIF 相机都有 Content-Length，影响有限，列入 v1 末修。
8. **未按 access unit 聚合 NAL（P2）**
   `play_loop` 每个 NAL 单独调 `on_video`。P0-#2 修完后，stream.rs 也需相应调整：把 NAL 攒到 `current_access_unit: Vec<Bytes>`，遇 mark=1 或 NAL 类型 5/7/8 触发 flush。

修复策略详见 `design.md` 中追加的 D12–D15 决策。修复任务追加到 `tasks.md` 的 7.7–7.11。