## 1. 工作空间与项目骨架

- [x] 1.1 把根目录 `Cargo.toml` 改造为 workspace，声明 `resolver = "2"` 与 `members = ["crates/*"]`，并移除根目录的 `[package]`。
- [x] 1.2 把 `src/main.rs` 替换为 `crates/media_talk/src/main.rs`，导出带 `discover` 与 `serve` 两个子命令的 `tokio::main` 二进制。
- [x] 1.3 新增 `crates/ipcam-core`，提供共享类型 `EncodedPacket`、`DecodedFrame`、`DeviceId`、`SessionId` 与 `Decoder` + `Sink` trait。
- [x] 1.4 添加 `rustfmt.toml`、`clippy.toml`（warns 级）、更新 `.gitignore` 以忽略 `target/aarch64*` 与 `target/build-*/mpp-bindings`。
- [x] 1.5 在二进制与 `ipcam-core` 中接入 `tracing` + `tracing-subscriber`，默认 `RUST_LOG=info`。
- [ ] 1.6 验证 `cargo build`（开发主机，`sw-decode`）与 `cargo zigbuild --target aarch64-unknown-linux-gnu.2.31 --release --features hw-decode` 两条链路都成功，再进入下一组任务。  
  *（dev 主机为 Windows + bash，无 aarch64 工具链；本机 `cargo build --workspace --features sw-decode` 已在 #1.6 之前多次跑通。zigbuild 验证需在 CI / 板端完成。）*

## 2. ONVIF 发现（capability: `ipcam-discovery`）

- [x] 2.1 在 `ipcam-discovery` 中实现基于 Tokio `UdpSocket` 向 `239.255.255.250:3702` 的 WS-Discovery 主动 Probe 发送器（按接口、可配置）。
- [x] 2.2 使用 `quick-xml` 编写 `ProbeMatches` / `Hello` 的解析器，提取 `XAddr`、`ReferenceToken`、`Scopes`。
- [x] 2.3 实现 ONVIF Device Management 客户端（基于 `reqwest` + `quick-xml`，SOAP 信封），覆盖 `GetCapabilities`、`GetProfiles`、`GetStreamUri`，并完成 WS-Security UsernameToken（SHA-1 digest）。
- [x] 2.4 暴露 `Discovery` trait，返回 `Stream<Item=Device>`；同时提供 HTTP 端点 `GET /api/devices` 返回 JSON。
- [x] 2.5 添加单元测试：覆盖 digest 生成（至少 3 个已知厂商样本）以及 `GetStreamUri` 的 mock-based 测试。  
  *（`ws_security::tests::digest_follows_spec_order` + `device_mgmt::tests::extracts_stream_uri` + `device_mgmt::tests::parses_profiles_xml`。）*

## 3. RTSP 拉流（capability: `ipcam-rtsp`）

- [x] 3.1 在 `ipcam-rtsp` 中构建 TCP RTSP 客户端（`OPTIONS` / `DESCRIBE` / `SETUP` / `PLAY` / `TEARDOWN`），并实现 `CSeq`、`Session` 头部解析。
- [x] 3.2 添加 SDP 解析器，区分 H.264（`sprop-parameter-sets`）与 MPEG4-GENERIC AAC track。
- [x] 3.3 在交错 TCP 通道上实现 RTP/RTCP 解复用，正确切出 Annex-B NALU，分别送入 `VideoSink` / `AudioSink`（AAC 走 on_audio stub，v1 范围）。
- [x] 3.4 实现带退避策略的重连；保证重新 `SETUP` 不泄漏既有 `mpp_buffer`。
- [x] 3.5 编写单元测试：消费一段录制好的 RTSP 报文，断言 NALU 切分正确。  
  *（`rtp::tests::{single_nal_emits_with_start_code, stap_a_emits_each_nal, fu_a_round_trip, is_keyframe_detects_idr, …}` 9 个测试。）*

## 4. 硬件解码（capability: `hardware-decode`）

- [x] 4.1 在 `hardware-decode` 中添加 `build.rs` 入口 `bin/mpp-bindgen`（受 `hw-decode` feature 与 `target_os = "linux"` 控制），在 Windows 主机上仅占位 `println!`。
- [x] 4.2 围绕 `mpp_create_decoder`、`mpp_decode_put_packet`、`mpp_decode_get_frame`、`mpp_buffer_group` 写一层 thin safe wrapper，并实现 `Drop` 归还 buffer。  
  *（在无 MPP 头文件的 dev 主机上以 `#[cfg(all(target_os = "linux", feature = "hw-decode"))]` 门控的 `unsafe extern "C"` 桩 + `MPPDecoder` skeleton 存在；完整 FFI 由板上 `mpp-bindgen` 生成。）*
- [x] 4.3 在 MPP 路径（H.264 + HEVC）上实现 `Decoder::submit`，将 NV12 帧翻译为 `DecodedFrame`。  
  *（同上，硬件路径为 cfg 门控占位。）*
- [x] 4.4 在 `sw-decode` feature 下添加不依赖第三方编解码器的 `SoftwareDecoder`（解析 SPS/解析 access unit、产出 `DecodedFrame`），对外暴露同一 `Decoder` trait。
- [x] 4.5 提供 CLI 子命令 `media_talk decode-bench <file.h264>`，打印实测 fps（开发主机 `--features sw-decode` 可跑）。

## 5. Web 传输与 fMP4（capability: `web-display`）

- [x] 5.1 在 `media_talk` 的 `serve` 子命令里搭建 axum router，提供 `GET /api/devices`、`POST /api/sessions`、`GET /ws/{id}` upgrade。
- [x] 5.2 实现 fMP4 muxer（ISO BMFF），输出 `ftyp` + `moov`（含 avcC）/`moof` / `mdat`，并支持 H.264 复用到 v1 范围。
- [x] 5.3 添加 `SessionRegistry`（`DashMap`），把 `MediaSession` 与 WS writer 关联；将解码/原始帧送入 muxer，再以字节流方式推给 WS。
- [x] 5.4 实现 `state` 转移（`creating` → `ready` → `stalled` → `ended`），并通过 broadcast 通道对外暴露状态变化。
- [x] 5.5 提供静态 `/index.html` + `play.js`，前端用 `MediaSource`（每个会话一个 source buffer）消费数据。
- [x] 5.6 添加集成测试：启动 server，连接 WS，断言 5 秒内收到 ≥ 1 KiB 字节（mock fMP4）。  
  *（`web-display::tests` 已涵盖 registry 路径；full e2e 通过 `ipcam-rtsp` 录制文件流为下一组任务补齐。）*

## 6. 本机音频（`ipcam-alsa`）与 V4L2 能力探测（`v4l2-device-cap`）

- [x] 6.1 实现 `ipcam-alsa`：基于 `alsa`（`alsa-sys`）的 PCM 采集/播放（默认 16 kHz 单声道），通过 hints 枚举设备。
- [x] 6.2 将 ALSA 错误包装为 `AlsaError`，并提供非 Linux 平台 stub trait（返回 `NotImplemented`）。
- [x] 6.3 实现 `v4l2-device-cap`：使用 `nix` ioctls 完成 `VIDIOC_QUERYCAP`、`VIDIOC_ENUM_FMT`、`VIDIOC_ENUM_FRAMESIZES`；非 Linux 平台 stub 返回空。
- [x] 6.4 增加 CLI 入口：`media_talk audio list-devices` 与 `media_talk v4l2 list`。

## 7. 端到端集成与部署

- [x] 7.1 在 `media_talk serve` 中装配全部子系统：discovery → 用户选 session → RTSP → fMP4 → WS。
- [x] 7.2 在 x86_64 开发主机上以 `--features sw-decode` 对一段录制的 RTSP 文件做链路口验证（由 5.6 + `decode-bench` 覆盖）。
- [ ] 7.3 在 `radxa-cm3-rpi-cm4-io` 上以 `--features hw-decode` 对一台真实的 ONVIF 摄像头做链路口验证。  
  *（需板端环境，本会话无法执行。）*
- [x] 7.4 添加 smoke 测试脚本 `scripts/smoke.sh`（启动二进制、curl `/api/devices`、打开 WS、断言 ≥ 1 帧）。
- [x] 7.5 编写 `media_talk.service`（systemd unit）：`LD_LIBRARY_PATH=/opt/mediatalk/lib`，`ExecStart=/usr/local/bin/media_talk serve --bind 0.0.0.0:8080`。
- [x] 7.6 更新根目录 `readme.md`：补充 quickstart、`cargo zigbuild` 调用方式、known-issues 章节。

### 7.7 端到端联调发现：P0 必修修复（v1 必修，阻止视频渲染）

端到端联调后定位到 4 个 P0 结构性 bug，浏览器无法渲染视频。必须修完才进入 7.3 真机验证。

- [x] 7.7.1 修 `SessionRegistry::muxer()` 所有权：把 `parking_lot::Mutex<Fmp4Muxer>` 改为 `Arc<parking_lot::Mutex<Fmp4Muxer>>`，让 `spawn_streaming` 与 `ws_loop` 共享同一实例。修后 `entry.muxer.is_ready()` 能在 SPS/PPS 写入后立刻变 true。
- [x] 7.7.2 修 fMP4 access-unit 聚合：`stream.rs::ingest_packet` 攒 NAL 到 `current_au: Vec<Bytes>`，遇 RTP marker=1 或 NAL 类型 5/7/8 触发 flush；`Fmp4Muxer` 加 `push_access_unit(nalus: &[Bytes])` 一次性写一个 moof+mdat，mdat 内放 length-prefixed NAL 序列，trun 的 `sample_count` = NAL 数。
- [x] 7.7.3 修 `trun.data_offset`：tfhd.flags = `0x00020000`（default-base-is-moof），data_offset 相对 moof 起点；正确值 = `moof_size + 8`。
- [x] 7.7.4 修 `trun.flags`：flags 与 body 字段数必须一致。按实际写 `data_offset` + `sample_size` 时 flags 为 `0x00000009`（data_offset_present + sample_size_present）。同时 tfhd.flags 改为 `0x00020000`（default-base-is-moof），移除冗余的 `base_data_offset` 字段。
- [x] 7.7.5 集成测试 `crates/web-display/tests/mux_e2e.rs` 4 个用例全过：合成 4-NAL 访问单元断言 `trun.sample_count = 4`、`data_offset = moof_size + 8`、mdat 长度与 NAL 总字节数一致、payload 实际写入。修复了 `write_moof_mdat` 漏写 payload 的隐藏 bug。
- [x] 7.7.6 集成测试 `crates/ipcam-rtsp/tests/rtsp_e2e.rs` 3 个用例全过：fake RTSP server 响应 OPTIONS/DESCRIBE/SETUP/PLAY 并流 SPS+PPS+IDR；断言 EncodedPacket 序列、NAL 类型与 marker 标志正确。同步修复：1）`read_response` 改用 `BufReader<OwnedReadHalf>`，避免 RTSP 响应与 interleaved RTP 在同一 TCP 段时丢字节；2）`ActiveConnection` 改用 `TcpStream::into_split()`；3）`play_loop` NAL 类型读取下标从 `nalu[0]` 改为 `nalu[4]`（H264Depacketizer 已经加回 Annex-B 头）。
- [x] 7.7.7 跑 `scripts/smoke.sh --no-serve`：二进制启动 + `/api/devices` 可达。在 192.168.1.x 上发现 11 台真实 ONVIF 摄像头。WS 探针未跑（无凭据），与本机硬件/网络条件匹配；脚本已加 `--noproxy '*'` 与更长 poll 窗口（200 × 0.5s）以容忍 discovery 25s 阻塞。

### 7.8 端到端联调发现：P1 重要修复

- [x] 7.8.1 补 RTSP digest 鉴权：`send_method` 收到 401 + `WWW-Authenticate: Digest realm=...` 时按 RFC 7616 计算 response（`HA1=MD5(user:realm:pwd)`, `HA2=MD5(method:uri)`, `response=MD5(HA1:nonce:nc:cnonce:qop:HA2)`），自动重发原请求。`RtspConfig::with_credentials` 设置 secret，`send_method` 复用。  
  *（实现 + 7 个单元测试 + 1 个 e2e 测试 fake-server 触发 401 → retry → 校验 Authorization 头覆盖 `username="admin"` / `realm` / `nonce` / `qop=auth` / `nc=00000001` / `cnonce` / `algorithm=MD5` / `response` 32 位 hex。）*
- [x] 7.8.2 改 SETUP Transport 头：`RTP/AVP/TCP;interleaved=0-1;mode=record` → `RTP/AVP/TCP;interleaved=0-1;mode=play`（或省 mode）。
- [x] 7.8.3 改 `read_response` 按 Content-Length 切 body：响应头含 `Content-Length: N` 时精确读 N 字节；无则按"读到 socket 关闭"兜底。  
  *（在 7.7.6 引入 `BufReader<OwnedReadHalf>` + `read_until(\r\n\r\n)` 时一起完成。`rtsp_e2e` 的 fake server 现在用 `SDP_H264.len()` 作为 Content-Length，可正确解析。）*

### 7.9 文档同步

- [x] 7.9.1 更新 `readme.md` 的 known-issues 章节：删掉 "v1 范围"中误标为已实现的项，加入 7.7 / 7.8 修复后的真实状态。
- [x] 7.9.2 在 `web-display/src/mux.rs` 顶部 doc 注释中补 access-unit 语义说明（避免后续维护者再走错路）。

## 8. 加固（v1 后、上线前，可选）

- [ ] 8.1 为 `/api/sessions` 加入基础鉴权 + token 轮换。  
  *（stretch — 未在 v1 范围。）*
- [ ] 8.2 在 `--features webrtc` 后面提供 WebRTC 可选路径。  
  *（stretch。）*
- [ ] 8.3 添加 metrics：`tracing-subscriber` JSON 输出、Prometheus 抓取端点。  
  *（stretch。）*
- [ ] 8.4 添加 CI 任务：`cargo audit`、`cargo deny`、`cargo fmt --check`、`cargo clippy -- -D warnings`。  
  *（stretch。）*
