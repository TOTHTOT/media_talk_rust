## 1. ipcam-core 新增 NAL 统计类型

- [x] 1.1 在 `crates/ipcam-core/src/lib.rs` 新增 `NalStats` 结构（access_units、idr_count、sps、pps、nal_by_type、bytes_total、time_to_first_idr、elapsed）。
- [x] 1.2 在 `crates/ipcam-core/src/lib.rs` 新增 `pub fn classify_h264_nal(data: &[u8]) -> Option<u8>`，读 `data[4] & 0x1F`；data 长度 < 5 返回 `None`。
- [x] 1.3 为 `NalStats` 派生 `Debug, Clone, Default, Serialize, Deserialize`；为 `classify_h264_nal` 写 2 条单测（典型 IDR / SPS NAL）。

## 2. ipcam-rtsp 暴露轻量 NAL 回调（不破坏现有 API）

- [x] 2.1 评估是否需要在 `RtspClient` 上加 `play_loop_nalus`；若 `EncodedPacket` 已携带所有 NAL 元数据，则跳过本步，仅在 probe 调用处用 `on_video` 闭包 + `classify_h264_nal` 实现。
- [x] 2.2 若新增入口：保证 `play_loop` 现有签名（`FnMut(EncodedPacket) -> CoreResult<()>`）不变；新方法标记 `#[allow(dead_code)]` 仅在 probe 路径被用。

## 3. media_talk probe 子命令骨架

- [ ] 3.1 在 `crates/media_talk/src/main.rs` 的 `Cli` enum 中新增 `Probe { rtsp_url: Vec<String>, username: Option<String>, password: Option<String>, duration: u64, json: bool }`。
- [ ] 3.2 在 `crates/media_talk/src/ipc/probe.rs` 新建文件，定义 `pub async fn run(urls, username, password, duration, json) -> i32`，主循环：逐 URL 调 `RtspClient::connect()` + `play_loop`，到时 `teardown()`。
- [ ] 3.3 在 `crates/media_talk/src/ipc/mod.rs`（若存在）注册 `pub mod probe;`；在 main 的 match 中分发 `Cli::Probe(args) => std::process::ExitCode::from(probe::run(...).await as u8)`。

## 4. NAL 统计聚合

- [ ] 4.1 在 `probe.rs` 实现 `fn accumulate(stats: &mut NalStats, pkt: &EncodedPacket, started: Instant)`：调用 `classify_h264_nal` 拿 NAL 类型，累加 `nal_by_type[nt]`、`bytes_total`、按 `is_keyframe` 与 `marker` 维护 `access_units` / `idr_count` / `time_to_first_idr`。
- [ ] 4.2 SPS / PPS 解析：NAL type == 7 / 8 时把 `&pkt.data[5..]`（去掉起始码 4 字节 + 头字节 1 字节）保存到 `stats.sps` / `stats.pps`，**只**保存第一次。
- [ ] 4.3 写 `fn stats_into_report(stats: NalStats, url: &str, exit_reason: &str) -> ProbeReport` 把 `NalStats` 序列化成 JSON 字段（含 `bitrate_kbps = bytes_total * 8 / elapsed.as_secs()`）。

## 5. 输出格式

- [ ] 5.1 人类格式：分块输出 `=== URL ===` / `连接结果` / `视频信息` / `NAL 分布（表格）` / `退出原因`；NAL 分布按 type 升序列出 `1: 145 / 5: 3 / 7: 1 / 8: 1`；末尾给一句 `OK / WARN / FAIL`。
- [ ] 5.2 JSON 格式：单行 `serde_json::to_string(&report)?` 写到 stdout；错误信息（鉴权失败、连接失败）走 stderr，JSON 报告里 `ok = false` + `error_kind` 字段。
- [ ] 5.3 退出码映射：`Ok(0)`、`NoIdr(1)`、`Auth(2)`、`Rtsp(3)`、`Args(4)`，按设计 §3 优先级合并多 URL 结果。

## 6. 测试

- [x] 6.1 在 `crates/ipcam-rtsp/tests/rtsp_e2e.rs` 现有 fake server 基础上扩一个 `probe_smoke` 集成测试：把 fake server 的 IDR/SPS/PPS 包喂给 `accumulate` 纯函数，断言 `idr_count >= 1`、`sps.is_some() == true`、`pps.is_some() == true`、`nal_by_type[7] == 1`。
- [x] 6.2 `cargo test --workspace` 全部通过；新增 1 个针对 `accumulate` 的单元测试覆盖空流、纯 SPS+无 IDR、混合 IDR+P 三种输入。
- [x] 6.3 文档：更新 `crates/media_talk/src/ipc/probe.rs` 顶部注释，说明「probe 是 RTSP 链路的最快验证工具，不验证 fMP4 / 硬件解码 / Web 显示」。

## 7. 真机端到端（用户执行）

- [x] 7.1 `cargo build -p media_talk` 后跑 `target/debug/media_talk.exe probe --rtsp-url 'rtsp://admin:changeme@192.168.1.13:8554/ch01' --duration 5`，贴回 stdout 给开发者对照 JSON 字段验证鉴权、SDP 协商、IDR 计数、time_to_first_idr。
- [x] 7.2 用错密码跑一次，确认退出码 = 2 且错误信息不泄露密码。
- [x] 7.3 用 `--json` 跑一次，确认输出可被 PowerShell `ConvertFrom-Json` 解析。
