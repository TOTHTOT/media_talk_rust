## Context

`rtp_send` 在 `rtp_send/sender.rs` 里实现了 probe 计数 + 定时日志，机制是在 payloader 的 src pad 上挂 `PadProbeType::BUFFER` probe，每发一个包计数器 +1，然后 spawn 一个 tokio 任务每 N 秒打印一次。

`rtp_recv` 目前没有对应机制。需要在 `rtp_recv/receiver.rs` 里实现类似的能力。

## Goals / Non-Goals

**Goals:**
- 在 `RtpReceiver` 内部安装收包 probe，统计 video/audio 包数
- 定时打印日志，格式与 `rtp_send` 一致
- 对调用方暴露 `packet_counts()` 方法查询当前计数器值

**Non-Goals:**
- 不修改 `RtpRecvConfig` 结构（配置层面无变化）
- 不做 bytes 级别统计（先做包数，与 rtp_send 对齐）
- 不改变 mp4 文件写入逻辑

## Decisions

### 1. Probe 挂在哪里

**选择：depay 的 src pad**

| 方案 | 优点 | 缺点 |
|---|---|---|
| `udpsrc` src pad | 更早统计，包含所有 UDP 包 | 包含 RTP header，未 depay，包含重传包 |
| `depay` src pad | 剥掉 RTP header 后，纯媒体数据，更准确 | depay 可能出错，导致漏统计 |

**结论：参考 rtp_send，在 depay src pad 上挂 probe**（与 rtp_send 一致，都是统计有效载荷包）

### 2. 计数器结构

```rust
#[derive(Default)]
pub(super) struct RecvStats {
    pub(super) video_pkts: AtomicU64,
    pub(super) audio_pkts: AtomicU64,
}
```

与 `SendStats` 结构完全对齐，便于理解。

### 3. 定时日志机制

在 `start_rtp_receiver` 里 spawn 一个 tokio task，循环读取计数器 + `info!` 打印，然后 `tokio::time::sleep`。间隔与 `rtp_send` 一致（2 秒）。

注意：tokio task 的生命周期绑定到 `stop_flag`，stop 时任务自然结束，不需要额外清理。

### 4. EOS 后停止统计

`stop()` 调用后，统计任务应立即停止（通过 `stop_flag` 检查），避免在管线 teardown 之后再继续打印。

## Risks / Trade-offs

- **风险**：depay src pad 在管线 teardown 时可能已经被销毁，导致 probe callback 访问已释放的计数器。
  **缓解**：probe 注册在 `build_video_track` / `build_audio_track` 内部，函数返回后 pad 引用仍然有效，直到管线销毁；tokio 统计任务在 stop 后立即退出，不再访问计数器。
