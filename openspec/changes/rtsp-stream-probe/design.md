## Context

`ip-camera-media-streaming` change 已经把「**RTSP 拉流 + 鉴权 + RTP 解包 + fMP4 切片 + Web 显示**」的第一条纵向链路搭起来了，但端到端验证成本高：要起 `media_talk serve`、开浏览器、点设备、等 15 秒、翻日志、查 fMP4 init 段为什么没出来。当问题出在「**RTSP 鉴权 / SDP 协商 / 长时间没 IDR**」这一层时，web UI 反馈的只是「黑屏 / 报错」，要定位是哪一段失败仍要去翻 trace。

工程现状：
- `ipcam-rtsp` 已经基于 `rtsp-runtime 0.2` 重写，公共 API 是 `RtspClient::connect() → RtspSessionInfo`，再 `play_loop(on_video, on_audio)` 把 `EncodedPacket`（H.264 Annex-B NAL 单元）推给调用方。
- `media_talk` 现在只有 `serve` 一个子命令。
- 单元 + 集成测试用 fake RTSP server 覆盖了 happy path 与 Digest 401 重试，但**没有真机端到端**的快速复现工具。

本 change 新增 `media_talk probe` 子命令，让「RTSP 能不能拉、凭据对不对、流长什么样」这一问题在 5 秒内可被回答。

## Goals / Non-Goals

**Goals:**

- 提供一个独立的、**不依赖 web 浏览器**的 CLI 入口，验证「RTSP → RTP → NAL」这一段是否正常。
- 输出**结构化诊断信息**：NAL 类型分布、IDR 计数、首帧 IDR 延迟、SPS/PPS 内容、字节总数、码率估算；可选 `--json` 便于 CI/脚本消费。
- 给每个 URL 清晰的退出码（成功 / 鉴权失败 / 拉不到 IDR / 其它 RTSP 错误），让 shell 脚本可判定。
- 复用 `ipcam-rtsp` 现有抽象，**不**新增任何 RTSP 相关依赖，**不**重复实现 RTP 解包。

**Non-Goals:**

- 不做硬件解码验证（那是 `hardware-decode` change 的范围）。
- 不做 fMP4 切片验证（那是 `web-display` change 的范围）。
- 不做 ONVIF 发现验证（那是 `ipcam-discovery` change 的范围）。
- 不录制到磁盘、不做码流分析（PSNR / 帧内容比对）。
- 不实现音频分析（probe 只统计视频 NAL；`on_audio` 回调继续被占位，不参与退出码判定）。
- 不支持 RTSP over UDP（本工程其它地方都用 TCP 互操作，probe 沿用）。

## Decisions

### Decision 1: 在 `RtspClient` 上新增 `play_loop_nalus` 而不是改 `play_loop` 签名

`RtspClient::play_loop<V, A>` 当前的签名是 `FnMut(EncodedPacket) -> CoreResult<()>`。NAL 单元经过 `H264Depacketizer` 解出后立刻推给回调；NAL 的具体类型（5=IDR / 7=SPS / 8=PPS / 1=non-IDR slice 等）和长度信息**已经在** `EncodedPacket` 里：`codec == H264`、`data` 含 Annex-B 起始码、`is_keyframe` 已基于 NAL 类型计算。所以 `probe` **不需要**新增入口，可以直接传一个会原地累计 `NalStats` 的闭包到 `on_video`。

但这样会把统计逻辑塞进 `probe.rs`，调用方要重复 `data[4] & 0x1F` 的模式。**折中**：在 `ipcam-core` 里新增一个**轻量**辅助 `fn classify_h264_nal(data: &[u8]) -> Option<u8>`，probe 用它读 NAL 类型；SPS/PPS 解析（提取 profile/level/width/height）只识别 `0x00000001` + 头几个字节，不调用 fMP4 muxer。**RtspClient 公共 API 不动**，实现改动为 0。

### Decision 2: `NalStats` 放在 `ipcam-core` 而不是 `ipcam-rtsp`

NAL 统计是「**应用层对 RTP 输出的观察**」，与具体 RTSP 库无关；后续 `web-display` 做录制时也可能复用。把类型放在 `ipcam-core` 让两层都不依赖 `ipcam-rtsp` 的私有类型。结构：

```rust
pub struct NalStats {
    pub access_units: u64,        // marker=1 触发的帧数
    pub idr_count: u64,           // NAL type 5 出现次数（per access unit 还是 per NAL：选 per NAL，与现有 web-display 行为一致）
    pub sps: Option<Vec<u8>>,     // 最新一条 SPS（NAL type 7）的 RBSP（不含起始码）
    pub pps: Option<Vec<u8>>,     // 最新一条 PPS（NAL type 8）的 RBSP
    pub nal_by_type: BTreeMap<u8, u64>,  // NAL type → 计数
    pub bytes_total: u64,
    pub time_to_first_idr: Option<Duration>, // 第一次见到 IDR 的相对耗时
    pub elapsed: Duration,        // 实际拉流时长
}
```

### Decision 3: 退出码用 POSIX 风格

| 退出码 | 含义 |
|---|---|
| 0 | 成功（拉流 ≥ 1 IDR）|
| 1 | 拉流 N 秒内**没有** IDR（GOP 异常或摄像头配置为只发 P 帧）|
| 2 | RTSP 鉴权失败（401）|
| 3 | 其它 RTSP 错误（连接失败、SDP 解析失败、SETUP/PLAY 状态码非 2xx）|
| 4 | URL 解析 / 参数错误 |

shell 脚本可以直接 `if probe ... ; then ...` 判断。多 URL 模式：任一 URL 成功 → 0；任一 401 → 2；任一无 IDR → 1；多个 URL 同时报错时取**优先级最高**的：参数错误(4) > 鉴权(2) > 无 IDR(1) > 其它(3) > 成功(0)。

### Decision 4: 不为 `probe` 写 fake-server 单元测试，只写 fake-server 集成测试

`ipcam-rtsp` 现有 `tests/rtsp_e2e.rs` 已经有完整 fake RTSP server。`probe` 的统计聚合在 `ipc/probe.rs` 里纯函数化（`stats_from_packets` 接收 `&[EncodedPacket]`），可以用 fake server 触发若干 NAL 后断言 `NalStats` 字段。真机验证交给用户在终端跑 `media_talk probe ...` —— 那是**用户**的端到端测试，不是单元测试。

### Decision 5: 时间窗口用 `--duration`（默认 5 秒），单次探测定时退出

不实现「见到 N 个 IDR 后自动退出」，因为要支持的场景是「**5 秒内能不能拉到东西**」，不是「等到稳定」。`--duration 0` 表示「拉到一个完整 AU 就退出」（便于脚本快速验证）。超时控制：在 `RtspClient` 内部加 `tokio::time::timeout(duration, play_loop)`，超时后正常走 `teardown()` 路径、汇总统计。

### Decision 6: JSON schema 稳定

固定字段集，**不**做 serde flatten / 嵌套对象膨胀。Nal 分布用 `BTreeMap<u8, u64>`（按 type 升序），二进制字段（SPS / PPS）用十六进制字符串，便于直接 copy 到 `sprop-parameter-sets=` 测试。**v1 锁字段**：将来要加字段只能 ADDED，不能改语义；类型变更走 major version。

```json
{
  "url": "rtsp://admin:***@192.168.1.13:8554/ch01",
  "ok": true,
  "elapsed_ms": 5012,
  "video_codec": "H264",
  "audio_codec": "Unknown",
  "access_units": 150,
  "idr_count": 3,
  "time_to_first_idr_ms": 412,
  "sps_hex": "6742C01EDA00A047FEC8",
  "pps_hex": "68CE3880",
  "nal_by_type": {"1": 145, "5": 3, "7": 1, "8": 1},
  "bytes_total": 612345,
  "bitrate_kbps": 977,
  "exit_reason": "duration_reached"
}
```

## Risks / Trade-offs

- **NAL 回调开销** → 回调里只做 BTreeMap 累加 + 字节加法 + 几个 `if`；每帧约 100ns 量级，对 30fps 摄像头消耗 < 1μs/s。
- **长时间跑会累积日志** → probe 默认开 `RUST_LOG=info`；不开启 `debug`；结束后 `teardown()`。
- **SPS/PPS 解析可能误判** → 用最严格的 `0x00000001` + NAL type ∈ {7,8} 判定，RBSP 切到下一个 `0x00000001` 之前；不解析 profile/level（避免假阳性）。
- **退出码 1（无 IDR）容易被误判为失败** → 报告里加 `exit_reason: "duration_reached_without_idr"` 字段；人类格式用 `⚠ no IDR in 5s — check camera GOP` 文案；用户读消息比读退出码更准。
- **多 URL 探测耗时线性叠加** → 文档说明「`--duration 5` × N 个 URL = 最长 5N 秒」；不做并发，因为单进程内 RTSP 鉴权错误要挨个报告给用户。
- **`probe` 子命令在 Windows 下断网时 wait 时间长** → `connect_timeout` 已经走 `RtspConfig::connect_timeout`（默认 10s），失败时进程最长 10s 内退出。

## Open Questions

- 是否需要在 probe 里加一个 `--list-formats` 选项，让用户在不知道摄像头支持什么 codec / 分辨率时先用 SDP 摸一遍？——v1 不做，留给后续。
- `ipcam-alsa` 与硬件解码链路要不要也接 probe？——不在本 change 范围；硬件解码的端到端验证更适合 `mpp-bindgen` 自带的 unit test。
- 当摄像头返回的 SDP 包含多个 video track（H.264 主 + H.265 子），probe 默认只统计第一个吗？——v1 简化为：probe 接 `info.tracks.len()` 决定要不要循环 SETUP/PLAY 多 track（沿用 `RtspClient::connect` 现有 SETUP 循环，不引入新行为）。
