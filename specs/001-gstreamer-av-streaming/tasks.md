---

description: "Task list for GStreamer 音视频拉流 implementation"
---

# Tasks: GStreamer 音视频拉流

**Input**: Design documents from `/specs/001-gstreamer-av-streaming/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/ipcam-gst-api.md, quickstart.md

**Tests**: 包含单元测试任务（plan.md 的 Testing 字段明确要求：配置校验、AU→NAL 切分、时间戳换算、状态机迁移）。

**Organization**: 按用户故事组织，US1 可独立交付为 MVP。

## Format: `[ID] [P?] [Story] Description`

- **[P]**: 不同文件、无未完成依赖，可并行
- **[Story]**: [US1]/[US2]/[US3] 对应 spec.md 的用户故事
- 路径基于 workspace 根目录 `crates/`

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: 新 crate 骨架与构建环境

- [X] T001 创建 `crates/ipcam-gst/` crate 骨架（Cargo.toml + src/lib.rs），并在 workspace `Cargo.toml` 的 members 中注册
- [X] T002 在 `crates/ipcam-gst/Cargo.toml` 加入依赖：`gstreamer = "0.24"`、`gstreamer-app = "0.24"`，以及 workspace 的 `ipcam-core`、`bytes`、`tracing`、`parking_lot`（版本决策见 research.md R1）。**实现备注**：gstreamer 依赖为 optional，feature `gst` 默认关闭（本机无 GStreamer/pkg-config）
- [ ] T003 [P] 验证 Windows 开发机构建：安装 GStreamer MSVC runtime+devel、PATH 置顶 GStreamer 自带 pkg-config 后 `cargo check -p ipcam-gst` 通过（步骤见 quickstart.md §2）
- [X] T004 [P] 在 `crates/web-display/Cargo.toml` 以可选依赖接入 `ipcam-gst`（feature `gst`），未启用时 `spawn_streaming` 记 error 并经 state_tx 发 `Ended`，保证无 GStreamer 环境仍可 `cargo test -p web-display`。**Deviation**：feature 默认**关闭**（原文默认开启），因本机无 GStreamer 构建环境；待 T003 环境就绪后可翻转为默认开启

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: 三个用户故事共用的类型与纯函数，全部可脱离 GStreamer 运行时单测

**⚠️ CRITICAL**: 本阶段完成前不得开始任何用户故事

- [X] T005 [P] 实现 `GstStreamConfig` / `AudioOutput` / `ReconnectPolicy` 及 `validate()`（rtsp:// 前缀、latency_ms ∈ [0,5000]、max_delay ≥ initial_delay）于 `crates/ipcam-gst/src/config.rs`，错误走 `GstStreamError::InvalidConfig`（契约：contracts/ipcam-gst-api.md）
- [X] T006 [P] 实现 `AudioPacket` 结构体（codec/data/pts_us/rate/channels 字段语义见 data-model.md）于 `crates/ipcam-gst/src/packet.rs`
- [X] T007 [P] 实现 AU→Annex-B 单 NAL 切分 `split_au_into_nals(&[u8]) -> Vec<Bytes>`（支持 3/4 字节起始码混合、忽略空段）于 `crates/ipcam-gst/src/packet.rs`
- [X] T008 [P] 实现时间戳换算 `pts_ns_to_rtp_ts90k(u64) -> u32` 与 `pts_ns_to_us(u64) -> i64`，及 H264/H265 关键帧判定（H264 nal_type==5；H265 nal_type ∈ {19,20}，注意 H265 nal_type 取位方式不同）于 `crates/ipcam-gst/src/packet.rs`
- [X] T009 [P] 实现 `GstStreamError` 错误枚举（InvalidConfig/Init/Connect/Stream，Connect 信息保留 401/Unauthorized 字样兼容 probe 分类）于 `crates/ipcam-gst/src/lib.rs`
- [X] T010 实现 `StreamState` 状态机与 `StreamStats`、`GstStreamHandle` 骨架（state()/stats()/stop()，stop 幂等）于 `crates/ipcam-gst/src/stats.rs`
- [X] T011 [P] 单元测试：config 校验正反例、AU 切分（含多 NAL/无起始码/空输入）、时间戳换算、关键帧判定，于 `crates/ipcam-gst/src/` 各模块 `#[cfg(test)]`

**Checkpoint**: `cargo test -p ipcam-gst` 全绿（本阶段测试不依赖 GStreamer 运行库）

---

## Phase 3: User Story 1 - 网页实时看到摄像机画面 (Priority: P1) 🎯 MVP

**Goal**: rtspsrc 拉流 H.264/H.265 视频，经 appsink 回调输出 `EncodedPacket`，web-display 现有 fMP4→WebSocket→MSE 链路出画面

**Independent Test**: quickstart.md S1——`media_talk serve --rtsp-url "rtsp://admin:changeme@192.168.1.46:8554/ch01"`，浏览器 10 秒内出连续画面

### Implementation for User Story 1

- [X] T012 [US1] 实现管线构建与 pad-added 动态分流框架（rtspsrc：`location`、`latency`、`drop-on-latency`、`protocols=tcp`、`user-id`/`user-pw` 属性注入；按 pad caps 的 `media`/`encoding-name` 分发，兼容静态 payload 无 encoding-name 的情况；多视频轨只取第一路，后续视频 pad 记 `warn!` 并忽略）于 `crates/ipcam-gst/src/pipeline.rs`
- [X] T013 [US1] 实现视频支路 rtph264depay ! h264parse ! appsink（caps `video/x-h264,stream-format=byte-stream,alignment=au`），appsink `new_sample` 回调中切 NAL（T007）、填 EncodedPacket（T008）、末 NAL 置 marker=true，于 `crates/ipcam-gst/src/pipeline.rs`
- [X] T014 [P] [US1] 实现 H.265 支路 rtph265depay ! h265parse（`video/x-h265,stream-format=byte-stream,alignment=au`），回调 `VideoCodec::H265` 帧，于 `crates/ipcam-gst/src/pipeline.rs`（网页播放下游暂丢弃，见 T016）
- [X] T015 [US1] 实现 `ipcam_gst::start(cfg, on_video, on_audio)` 入口：validate → gst::init（失败映射 `GstStreamError::Init` 并含缺失元件名）→ 构建管线 → PLAYING → 状态迁移 Connecting→Playing；**每次 start 创建独立管线实例，句柄间无共享可变状态**（FR-007 并发隔离约束），于 `crates/ipcam-gst/src/lib.rs`
- [X] T016 [US1] 替换 `crates/web-display/src/stream.rs` 的 RtspClient 拉流段（:76-105）为 `ipcam_gst::start`：`resolve_stream_uri` 结果原样作 location，凭据透传；`on_video` 复用现有 `ingest_packet` 闭包零改动，`on_audio` 传 no-op 闭包（US2 再接）；非 H264 帧记 `warn!` 丢弃（H265 待 muxer 扩展）；state_tx 结束通知行为不变（无 gst feature 时记 error 并发 Ended）。**备注：`spawn_streaming` 签名在 T023 被扩展（新增 `audio_out` 参数），"签名不变"约束以 T023 为准**
- [X] T017 [US1] 迁移 `probe` 命令到 ipcam-gst：媒体收集改用 `ipcam_gst::start` + mpsc channel 收集 `EncodedPacket` 喂 `NalStats`（`accumulate_nal` 纯函数原样复用）；`RtspClient::connect` 的 SDP/编码探测**保留不动**；错误分类保留 401 判定（bus 错误文本字符串匹配）；无 gst feature 时 probe 打印明确错误并返回非零退出码，于 `crates/media_talk/src/ipc/probe.rs`
- [ ] T018 [US1] 删除 `crates/ipcam-rtsp/src/lib.rs` 的 `play_loop` 与 `crates/ipcam-rtsp/src/rtp.rs`（保留 connect/teardown/sdp/RtspError 供 probe 的 connect 阶段），同步删除 `crates/ipcam-rtsp/tests/rtsp_e2e.rs` 中 play_loop 用例并修正引用。**前置：T016 与 T017 均完成并验证**。**备注：本阶段刻意跳过——待 T003 构建环境就绪、quickstart S1/S5 实机验证通过后再执行**
- [X] T019 [US1] 认证失败路径：bus ERROR 文本含 401/Unauthorized/Not Authorized 时以 `Connect` 语义写入 `last_error` 并 transition(Failed)，stream.rs 记日志并发 `SessionState::Ended`，probe 侧按字符串匹配分类为 auth（exit 2）。**备注：代码路径已实现，错误密码实测（quickstart.md S5）待 T003 环境后验证**

**Checkpoint**: US1 独立可验收——网页出画面；`probe` 命令迁移后功能回归正常；`cargo test --workspace` 全绿

---

## Phase 4: User Story 2 - 同步听到摄像机现场声音 (Priority: P2)

**Goal**: 音频轨 depay 后经回调暴露编码帧，并在启用时于管线内解码送目标板 ALSA 播放

**Independent Test**: quickstart.md S2——含音频轨的摄像机出声且与画面同步（拍手测试，<200ms）；无音频轨的摄像机视频正常且日志注明

### Implementation for User Story 2

- [X] T020 [P] [US2] 实现音频支路分流：按 encoding-name/payload 选 rtppcmadepay / rtppcmudepay / rtpmp4adepay ! appsink，回调构造 `AudioPacket`（G.711 固定 rate=8000/channels=1；AAC 从 sample caps 读，读不到用 0 并 warn）调 `on_audio` + `note_frame`，于 `crates/ipcam-gst/src/pipeline.rs`
- [X] T021 [P] [US2] 实现 `AudioOutput::Alsa` 播放支路：depay 后接 tee，一路到 appsink（回调），另一路 `alawdec`/`mulawdec`/`aacparse ! avdec_aac` ! `audioconvert ! audioresample ! alsasink`（device 可配；avdec_aac 属 gst-libav，README/quickstart 已注明需 `gstreamer1.0-libav`），于 `crates/ipcam-gst/src/pipeline.rs`
- [X] T022 [US2] 无音频轨处理：进入 Playing 且无音频 pad 时一条 `info!`（"stream has no audio track"），不算错误，于 `crates/ipcam-gst/src/pipeline.rs`（watch_bus，每轮管线重建各记一次）
- [X] T023 [US2] CLI 接线：`media_talk serve` 增加 `--audio-out <ALSA_DEVICE>`（`crates/media_talk/src/main.rs`、`crates/media_talk/src/ipc/serve.rs`），经 `WebDisplay::start` → `Inner.audio_out` → `spawn_streaming` → `GstStreamConfig.audio_output` 透传（默认 Disabled 仅回调）；非 gst 构建下参数链正常编译，仅使用时记 warn

**Checkpoint**: US1+US2 同时可用；音频关闭时行为与 US1 完成态一致

---

## Phase 5: User Story 3 - 断流自动恢复与状态可观测 (Priority: P3)

**Goal**: bus ERROR/EOS/RTSPSrcTimeout 触发退避重建；状态与统计全程可观测

**Independent Test**: quickstart.md S3——断网 60s 恢复后 30s 内画面自动回来，日志可见 Reconnecting→Playing 与 reconnects 计数

### Implementation for User Story 3

- [X] T024 [US3] 实现 bus 监听：ERROR/EOS/RTSPSrcTimeout element message → set_state(Null) → 按 `ReconnectPolicy` 指数退避（1s 起、上限 30s、默认无限）整条管线重建；状态迁移 Playing→Reconnecting→Connecting→Playing，重试耗尽 →Failed，401 语义 ERROR 直接 Failed 不重试；退避等待用 condvar 可被 stop() 打断，于 `crates/ipcam-gst/src/pipeline.rs`（session_loop + watch_bus + `stats.rs::wait_or_stop`）
- [X] T025 [P] [US3] 实现统计采集与周期日志：frames_video/frames_audio/bytes/reconnects/last_error，每次状态迁移一条 `info!`（stats.rs transition），Playing 期间每 10s 一条统计（spawn_stats_ticker，随 stop 退出），于 `crates/ipcam-gst/src/pipeline.rs` + `crates/ipcam-gst/src/stats.rs`
- [X] T026 [US3] 单元测试：Reconnecting 迁移表用例（合法环 + Reconnecting→Playing 非法）、`ReconnectPolicy::exhausted` max_attempts 边界、`wait_or_stop` 到时返回 true / 被 stop 打断返回 false，于 `crates/ipcam-gst/src/stats.rs` + `crates/ipcam-gst/src/config.rs` 的 `#[cfg(test)]`

**Checkpoint**: 三个故事全部可用；S3/S4 实测通过

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: 跨故事收尾与文档

- [ ] T027 [P] 全量质量门：`cargo test --workspace` 全绿 + `cargo clippy --workspace --all-targets` 无警告
- [X] T028 [P] 更新 `CLAUDE.md`/`README.md`：新 crate 职责、GStreamer 系统依赖、构建方式（Windows 调试 + aarch64 交叉）；quickstart.md 同步补 gstreamer1.0-libav 依赖与 S2 的 `--audio-out` 用法
- [ ] T029 按 `specs/001-gstreamer-av-streaming/quickstart.md` S1~S6 全场景实机验收并记录结果
- [ ] T030 [P] （stretch，最低优先级）评估并实现 `Fmp4Muxer` 的 H.265（hvcC/hev1）支持于 `crates/web-display/src/mux.rs`，先做目标浏览器 MSE 兼容性验证再动工（背景见 research.md R8）

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: 无依赖，立即开始
- **Foundational (Phase 2)**: 依赖 T001/T002；**阻塞所有用户故事**
- **US1 (Phase 3)**: 依赖 Foundational；T016 依赖 T012/T013/T015；T017 依赖 T015；T018 依赖 T016+T017 验证通过
- **US2 (Phase 4)**: 依赖 Foundational + T012（复用分流框架）；与 US1 的 T014/T016 之后衔接最顺
- **US3 (Phase 5)**: 依赖 T015（start 入口与管线生命周期）
- **Polish (Phase 6)**: 依赖 US1~US3 完成（T030 除外，可任意时间单独立项）

### User Story Dependencies

- **US1 (P1)**: 仅依赖 Foundational——独立可交付（MVP）
- **US2 (P2)**: 依赖 US1 的管线框架，但音频支路代码文件独立，可并行开发
- **US3 (P3)**: 横切管线生命周期，最后做

### Parallel Opportunities

- Phase 1: T003 ∥ T004
- Phase 2: T005 ∥ T006 ∥ T007 ∥ T008 ∥ T009（不同模块/文件）
- US1: T013（H264 支路）∥ T014（H265 支路）；T016（web-display）∥ T017（probe）在 T015 之后可并行
- US2: T020 ∥ T021
- US3: T024 与 T025 可部分并行（T025 的统计字段被 T024 消费，先定接口）

---

## Parallel Example: Phase 2 Foundational

```bash
# 纯函数模块互不依赖，可全部并行：
Task: "GstStreamConfig/validate() in crates/ipcam-gst/src/config.rs"
Task: "AudioPacket in crates/ipcam-gst/src/packet.rs"
Task: "AU→NAL 切分 in crates/ipcam-gst/src/packet.rs"
Task: "时间戳换算与关键帧判定 in crates/ipcam-gst/src/packet.rs"
Task: "GstStreamError in crates/ipcam-gst/src/lib.rs"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. 完成 Phase 1 + Phase 2（`cargo test -p ipcam-gst` 全绿）
2. 完成 Phase 3 → 网页出画面即为可演示 MVP
3. **STOP**：按 quickstart.md S1/S5 实机验收后再继续

### Incremental Delivery

1. Setup + Foundational → 地基就绪
2. US1 → 视频 MVP（替换自研拉流的风险在此阶段暴露完毕）
3. US2 → 音频叠加，不破坏 US1
4. US3 → 生产可靠性
5. Polish + H265 stretch

---

## Notes

- [P] = 不同文件、无未完成依赖
- 每个故事完成后按 quickstart.md 对应场景实机验证再推进
- **T018 是破坏性改动（删 play_loop），必须在 T016（网页出画面）与 T017（probe 迁移完成）都验证后再执行提交**——这样替换失败时随时可回退
- 分辨率中途变化本期按 spec 边界处理（检测+记日志，重开播放页恢复），不列任务
- 回调在 GStreamer 流线程执行：保持轻量，禁止阻塞（契约第 5 条）
