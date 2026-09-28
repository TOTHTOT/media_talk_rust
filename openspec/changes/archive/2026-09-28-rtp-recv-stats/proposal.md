## Why

`rtp_recv` 目前没有任何进度反馈——管线启动后完全黑盒，外部无法判断是否真的在收 RTP 包。相比之下 `rtp_send` 已有 stats 日志定期打印 `video_pkts / audio_pkts`，联调时可以第一时间回答"有没有数据发出去了"。

接收端同样需要这个能力：回答"有没有数据进来了"。目前 answer 示例里视频包数为 0 但没有任何告警，无法判断是网络问题、端口不匹配还是编解码问题。

## What Changes

- 在 `rtp_recv/receiver.rs` 的 `RtpReceiver` 结构里增加 `RecvStats` 计数器和定时日志
- 接收链路各安装一个 packet probe（`udpsrc` 的 src pad 上），每收到一个完整 RTP 包计数器 +1
- 定时日志输出格式与 `rtp_send` 保持一致：`rtp receiver stats video_pkts=X audio_pkts=Y`
- 仅在 `RtpReceiver` 内部记录，通过 `packet_counts()` 方法暴露给调用方

## Capabilities

### New Capabilities

- `rtp-recv-stats`: RTP 接收进度统计能力，参考 `rtp_send` 的 probe 计数模式，在 depay src pad 上安装 probe 统计收包数，并定时打印日志

### Modified Capabilities

- 无（不影响现有 spec 行为，仅是实现层面的功能补充）

## Impact

- `crates/ipcam-gst/src/rtp_recv/receiver.rs`：主要改动文件
- `crates/ipcam-gst/src/rtp_recv/config.rs`：如需统计配置化可小幅调整，否则不变
- 调用方（如 `answer.rs`）：可通过 `receiver.packet_counts()` 获取当前收包数，不需要任何 API 变更
