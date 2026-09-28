## 1. 实现 RecvStats 结构

- [x] 1.1 在 `rtp_recv/receiver.rs` 中添加 `RecvStats` 结构体，包含 `video_pkts: AtomicU64` 和 `audio_pkts: AtomicU64`
- [x] 1.2 在 `RtpReceiver` 中添加 `stats: Arc<RecvStats>` 字段

## 2. 安装收包 Probe

- [x] 2.1 在 `build_video_track` 的 depay src pad 上安装 `PadProbeType::BUFFER` probe，收到包时 `stats.video_pkts.fetch_add(1)`
- [x] 2.2 在 `build_audio_track` 的 depay src pad 上安装 `PadProbeType::BUFFER` probe，收到包时 `stats.audio_pkts.fetch_add(1)`

## 3. 定时日志任务

- [x] 3.1 在 `start_rtp_receiver` 中 spawn std thread，每 2 秒读取计数器并打印 `info!("rtp receiver stats video_pkts={} audio_pkts={}", ..., ...)`
- [x] 3.2 日志任务检查 `stop_flag`，stop 时立即退出

## 4. 对外暴露 packet_counts 方法

- [x] 4.1 在 `RtpReceiver` 中添加 `packet_counts(&self) -> (u64, u64)` 方法，返回当前计数器值

## 5. 验证

- [x] 5.1 `cargo fmt --all` 通过
- [x] 5.2 `cargo clippy --workspace --all-targets --features sw-decode -- -D warnings` 通过
- [x] 5.3 `cargo test --workspace --features sw-decode` 通过（所有原有测试不减少）
