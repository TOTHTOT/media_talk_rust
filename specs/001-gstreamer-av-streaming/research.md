# Phase 0 Research: GStreamer 音视频拉流

**Date**: 2026-08-20 | **Feature**: specs/001-gstreamer-av-streaming

## R1: gstreamer crate 版本选型

- **Decision**: 使用 `gstreamer = "0.24"` 与 `gstreamer-app = "0.24"`。
- **Rationale**: 项目 `rust-version = 1.85`；`gstreamer 0.25.x` 的 MSRV 是 Rust 1.92（不满足），`0.24.x` MSRV 为 1.83（满足）。绑定向后兼容系统库 ≥ 1.14，rk356x 固件常见的 1.20/1.22 均可运行；`v1_xx` feature 只解锁新 API，本特性不需要。
- **Alternatives considered**: 0.25（API 略新，但 MSRV 1.92 超出现有工具链）；0.23（更老，无收益）。
- **来源**: crates.io API 一手数据（`gstreamer` 0.24.5 MSRV 1.83）；[GStreamer 1.24 release notes](https://gstreamer.freedesktop.org/releases/1.24/)（绑定 0.22 起兼容 ≥1.14 的表述，后续版本延续）。

## R2: 管线拓扑与元件

- **Decision**: 手动组元件（不用 `parse_launch`）：`rtspsrc` → `connect_pad_added` 动态分流 → 视频支路 `rtph264depay ! h264parse`（H.265 为 `rtph265depay ! h265parse`）→ `appsink`（caps `video/x-h264,stream-format=byte-stream,alignment=au`）；音频支路按编码选 `rtppcmadepay` / `rtppcmudepay` / `rtpmp4adepay`。
- **Rationale**: 多轨（视频+音频）必须从 rtspsrc 的动态 pad（`stream_%u`，caps `application/x-rtp`）分流，parse_launch 字符串无法注入 Rust 回调。`h264parse` 必须存在：它把 RTP 拆出的 NAL 聚合成整 AU 并产生 codec_data/时间戳。`byte-stream`（Annex-B）正好匹配下游 `Fmp4Muxer` 的现有输入约定。
- **Alternatives considered**: `parse_launch` 单字符串（多轨分流和回调注入不便，否决）；直接 appsink 接 `application/x-rtp`（拿到的是 RTP 包而非编码帧，否决）。
- **来源**: [rtspsrc 官方文档](https://gstreamer.freedesktop.org/documentation/rtsp/rtspsrc.html)；[gstreamer-rs decodebin.rs 官方示例](https://github.com/GStreamer/gstreamer-rs/blob/main/examples/src/bin/decodebin.rs)（动态 pad 模式）；[rtp 插件元件索引](https://gstreamer.freedesktop.org/documentation/rtp/index.html)（depay 元件归属 gst-plugins-good）。

## R3: appsink 取帧与时间戳

- **Decision**: `AppSinkCallbacks` + `new_sample` 回调（设 `emit-signals=true`）；`sample.buffer().map_readable()` 取字节；`buffer.pts()/dts()`（纳秒 `ClockTime`）换算 90kHz 填入 `EncodedPacket.rtp_ts`，`arrival_us` 用 `ipcam_core::now_micros()`。h264parse `alignment=au` 输出整 AU 一个 buffer，按 Annex-B 起始码切成单 NAL 逐个回调，最后一个 NAL 置 `marker=true`。
- **Rationale**: 下游 `stream.rs::ingest_packet` 与 `Fmp4Muxer` 的输入契约（Annex-B 单 NAL、marker 定 AU 边界、rtp_ts 不被消费只作展示）原样保留，替换时 web-display 侧零改动。
- **来源**: [gstreamer-app docs.rs](https://docs.rs/gstreamer-app/latest/gstreamer_app/) + [appsink.rs 官方示例](https://github.com/GStreamer/gstreamer-rs/blob/main/examples/src/bin/appsink.rs)。

## R4: 认证与低延迟参数

- **Decision**: 凭据走 rtspsrc 的 `user-id` / `user-pw` 属性（优先）或 URI userinfo；`latency=200`（ms，默认 2000 是直播大坑）；`drop-on-latency=true`；`do-retransmission` 保持默认 true；`protocols=tcp`（与自研客户端的 TCP interleaved 行为一致，避免 UDP 丢包排查复杂度）。
- **Rationale**: Basic/Digest 由 rtspsrc 内部自动协商，无需配置；TCP interleaved 与现有实现行为对齐，且穿越性更好。
- **来源**: [rtspsrc 官方属性文档](https://gstreamer.freedesktop.org/documentation/rtsp/rtspsrc.html)（官方确认）。

## R5: 断线重连

- **Decision**: bus 监听 `ERROR` / `EOS` / `RTSPSrcTimeout` element message → `set_state(Null)` → 销毁并按指数退避（默认 1s 起、上限 30s、无限重试）重建整条管线。
- **Rationale**: GStreamer 无内建通用自动重连（`udp-reconnect` 只覆盖 UDP 传输下控制连接断开）；社区一致模式是 ERROR 时整条重建。注意网络断开常只发 ERROR 不发 EOS，重连触发必须挂在 ERROR 上。
- **来源**: [GStreamer Discourse: RTSP 断线重连](https://discourse.gstreamer.org/t/rtsp-disconnect-and-reconnect-on-error-during-play/395)（社区经验，模式通用）；`rtspsrc2` 功能不全，官方 release notes 不建议生产使用。

## R6: 音频处理范围（落地形态）

- **Decision**: 本期音频播放走 GStreamer 管线内部支路：`rtppcmadepay ! alawdec ! audioconvert ! alsasink`（PCMU/AAC 同理用 `mulawdec` / `aacparse ! avdec_aac`），直接出目标板扬声器；同时把 depay 后的**编码音频帧**通过回调暴露（`AudioPacket`，为二期双向对讲和网页音频预留）。网页 fMP4 音频轨本期不做。
- **Rationale**: 现有 `ipcam-alsa` 只有采集接口（播放是 stub），网页 `Fmp4Muxer` 只支持 H264 视频轨；在管线内接 `alsasink` 是满足 US2（"网页端或目标板扬声器"）的最小路径。G.711/AAC 软解在 rk356x 上开销可忽略，无需硬解。
- **Alternatives considered**: 扩展 fMP4 muxer 加 AAC 音轨（工作量与浏览器 MSE 兼容性验证都大，列入后续）；扩展 ipcam-alsa 播放 + Rust 侧软解（重复造 GStreamer 已有元件，否决）。
- **来源**: [rtppcmudepay 官方文档](https://gstreamer.freedesktop.org/documentation/rtp/rtppcmudepay.html)（`audio/x-alaw,channels=1,rate=8000` caps）；注意 PCMA 是静态 payload 8，部分 IPC 的 SDP 不写 rtpmap，分流判断需兼容 `payload=8` 无 `encoding-name` 的情况（社区经验）。

## R7: 与现有代码的替换边界

- **Decision**: 新增 crate `ipcam-gst`，对外暴露与 `play_loop` 等价的拉流 API；`web-display/src/stream.rs` 把 `RtspClient::connect/play_loop` 换成 `ipcam-gst`；`ipcam-rtsp` 删除 `play_loop`、`rtp.rs` 的 depacketizer 随之下架，但**保留** `RtspClient::connect/teardown`、`RtspError`、`sdp` 模块——`probe` 命令的 SDP 探测仍依赖它们。
- **Rationale**: 符合 FR-008（替换拉流路径、不留双后端）；probe 的诊断能力（SDP/编码信息 + 401 分类）与拉流无关，重写没有收益。
- **来源**: 代码勘察（agent 报告）：`probe.rs:287-320` 依赖 connect/teardown；`rtp.rs/sdp.rs` 除内部外无其他引用。

## R8: H.265 的交付边界

- **Decision**: `ipcam-gst` 完整接收并解封装 H.265（rtph265depay ! h265parse，回调 `VideoCodec::H265` 帧），满足"接收"要求；但现有 `Fmp4Muxer` 只产出 avcC（H264），H265 帧在 web-display 侧暂被丢弃并记日志。muxer 的 hvcC/hev1 扩展列为本特性最低优先级的独立任务，按浏览器 MSE 兼容性评估后再做。
- **Rationale**: MSE 的 H265 支持在浏览器侧参差不齐（Safari 支持，Chrome 仅较新版本支持 hev1），扩展 muxer 前需要先确认目标浏览器；不阻塞 P1/P2 主线。
- **来源**: 代码勘察（`stream.rs:126` 丢弃非 H264；`mux.rs` avcC 写死）。

## R9: 构建与部署

- **Decision**: Windows 开发机本地调试：安装 GStreamer 官方 MSVC 版 runtime+devel 两个 MSI，`bin` 目录入 PATH，且 GStreamer 自带 `pkg-config.exe` 必须排在 PATH 最前（官方 README 明确警告）。交叉编译 aarch64：走 WSL2/Docker + `aarch64-linux-gnu-gcc`，sysroot 含 `libgstreamer1.0-dev`、`libgstreamer-plugins-base1.0-dev`，设 `PKG_CONFIG_ALLOW_CROSS=1`、`PKG_CONFIG_SYSROOT_DIR`、`PKG_CONFIG_PATH`，`.cargo/config.toml` 配 linker。目标板运行时必须装 `gstreamer1.0-plugins-good`（rtspsrc/rtp depay 所在包）等插件，用 `gst-inspect-1.0 | grep -E 'rtsp|rtp'` 验证。
- **Rationale**: Windows 直接交叉 aarch64 Linux 基本走不通（社区共识），WSL2/Docker 是标准做法；编译期只需 core+base 的 dev 包，但运行时缺 good 插件会让 `ElementFactory::make("rtspsrc")` 直接失败。
- **Alternatives considered**: 静态编译/捆绑 GStreamer（体积与许可证复杂度不值，否决）。
- **来源**: [docs.rs gstreamer README](https://docs.rs/crate/gstreamer/latest)（官方）；[cross-rs #250](https://github.com/cross-rs/cross/issues/250)、[gstreamer-rs #463](https://gitlab.freedesktop.org/gstreamer/gstreamer-rs/-/issues/463)（社区经验）。

## R10: Rockchip 硬解元件（信息性结论）

- **Decision**: 本特性管线不解码，**不使用** `mppvideodec`。记录：它是 Rockchip 第三方插件 `gstreamer-rockchip`（Debian 包 `gstreamer1.0-rockchip1`），不在上游 gst-plugins-bad；rk356x 厂商/社区固件普遍可得。后续如需硬解显示再评估（也可走上游 `v4l2codecs` 无状态路径）。
- **来源**: [Firefly 社区 GStreamer 文档](https://community.t-firefly.com/docs/software/multimedia/GStreamer)、[ODROID-M2 wiki](https://wiki.odroid.com/odroid-m2/application_note/mpp)、1.24 release notes（未收录确认）。
