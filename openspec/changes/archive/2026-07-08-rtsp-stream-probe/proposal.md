## Why

在 `ip-camera-media-streaming` 第一条纵向链路（发现 → RTSP → 硬件解码 → Web fMP4）跑通之前，开发者需要一个**不依赖浏览器、不走 fMP4 切片**的最小验证手段：在已知摄像头账号密码的前提下，单独验证「RTSP 能否完成协商 + 鉴权 + 真正拉到 NAL 数据」。当前工程在 `web-display` 端有完整 UI，但要确认 LIVE555 / 海康 / 大华类设备能不能过 `rtsp-runtime` 的 digest/basic 鉴权、能不能稳定吐出 IDR，往往要起服务 → 开浏览器 → 等 15 秒 → 翻日志，迭代成本高。本 change 新增 `media_talk probe` 子命令：给定 RTSP URL + 凭据，播放 N 秒，从 RTP 包中提取并按 NAL 单元统计，输出人类可读 + JSON 两种格式的诊断报告，让拉流问题能在几秒内复现并定位到「401 / 鉴权失败 / SDP 协商失败 / 长时间无 IDR / 帧率异常」这一层。

## What Changes

- 新增 `media_talk` CLI 子命令 `probe`，参数：`--rtsp-url <URL>`（必填，可重复多次）、`--username <U>`、`--password <P>`、`--duration <秒>`（默认 5）、`--json`（结构化输出）。
- `probe` 复用 `ipcam-rtsp` crate（已基于 `rtsp-runtime 0.2` 重写），不在 RTSP 层引入新依赖。
- 在 `ipcam-rtsp` 中暴露轻量回调钩子：`RtspClient::play_loop` 已存在 `on_video` / `on_audio`，新增可选的「**逐 NAL 单元回调**」（Annex-B 已分片的 NAL，含类型/长度/是否 IDR/时间戳），`probe` 用它在内存里累计统计，不重新解包。
- 新增 `ipcam-core` 内的 `NalStats` 结构：`{ access_units, idr_count, sps, pps, nal_by_type: BTreeMap<u8, u64>, bytes_total, first_idr_at }`。
- `probe` 流程：URL 解析 → 拼 `RtspConfig` + 凭据 → `connect()`（捕获鉴权/SDP 错误）→ `play_loop` 在 `--duration` 时间内累计 NAL → 打印摘要（人类格式）或 JSON → 进程退出码：成功 0，鉴权失败 2，拉到 0 IDR 1，其它 RTSP 错误 3。
- **不**修改 `web-display`、`media_talk serve`、`ipcam-rtsp` 已有的公共 API 表面；只在 `ipcam-rtsp` 内部为 `RtspClient` 增加一个**新的可选**入口方法（不破坏现有 `play_loop(on_video, on_audio)` 调用）。
- **不**改 fMP4 muxer、不动硬件解码、不动 ONVIF 发现；这些是各自 change 的范围。

## Capabilities

### New Capabilities

- `rtsp-stream-probe`: 一个本地 CLI 诊断工具，连接 RTSP 源、按 NAL 单元统计并输出诊断报告。它回答「这台摄像头在带凭据的情况下能不能真正拉到流、流长什么样」这个问题，是 web 显示链路之前的最快验证手段。

### Modified Capabilities

_（仓库当前没有 `openspec/specs/` 下的旧 capabilities，无修改项）_

## Impact

- 新增/改动文件：
  - `crates/media_talk/src/main.rs` — 新增 `Probe` 子命令分支。
  - `crates/media_talk/src/ipc/probe.rs` — 新增 `probe` 子命令实现（含 NAL 统计聚合、人类格式与 JSON 格式化、退出码）。
  - `crates/ipcam-rtsp/src/lib.rs` — 在 `RtspClient` 上新增**可选**的 `play_loop_nalus` 方法（或等价的 `with_nal_callback` 入口），签名上不破坏现有 `play_loop` 调用。复用现有 `H264Depacketizer`。
  - `crates/ipcam-core/src/lib.rs` 或新文件 — 新增 `NalStats` 类型。
- 依赖：复用现有 `ipcam-rtsp`、`ipcam-core`、`clap`、`serde`、`serde_json`、`tracing`；**不**新增 crates。
- 行为变化：仅对 `media_talk probe` 子命令可见，不影响 `serve` 行为。
- 风险点：
  1. 长时间 `play_loop` 内回调开销要可控：NAL 回调里只做轻量累加（计数器 + 长度加法），不分配大对象、不阻塞。
  2. `probe` 默认 5 秒拉不到 IDR 退出码 = 1，但**部分摄像头只输出 P 帧直到下一个 IDR**——这本身可能是摄像头 GOP 极长（比如 30s）的现象，**不是**故障。报告里要把「`time_to_first_idr`」「`access_unit_count`」分开呈现，让人类能区分。
  3. 多 URL 串联探测时，每个 URL 独立计时、互不影响；任一 URL 鉴权失败要继续测下一个，全部跑完再汇总。
- 验证手段：
  - 用 `ipcam-rtsp` 现有 fake RTSP server（`tests/rtsp_e2e.rs`）扩一个 `probe_smoke` 集成测试，断言 NAL 统计里 `idr_count >= 1`、`sps.is_some() == true`、`pps.is_some() == true`。
  - 真机端到端：用户给定的 `rtsp://admin:changeme@192.168.1.13:8554/ch01` 跑 `probe`，对比 Python VLC 工具的输出。
