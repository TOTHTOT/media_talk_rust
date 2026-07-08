# ipcam-rtsp Specification

## Purpose
TBD - created by archiving change ip-camera-media-streaming. Update Purpose after archive.
## Requirements
### Requirement: 基于 TCP 的 RTSP 客户端
系统 SHALL 使用 TCP 与摄像头建立 RTSP 控制连接，URI 来源为 ONVIF `GetStreamUri` 的返回。客户端 SHALL 至少实现 `OPTIONS`、`DESCRIBE`、`SETUP`、`PLAY`、`PAUSE`、`TEARDOWN` 六种方法。

#### Scenario: DESCRIBE 返回 SDP
- **WHEN** 客户端对一台标准 ONVIF 摄像头发送 `DESCRIBE`
- **THEN** 服务端 SHALL 返回 `200 OK`，且 `Content-Type` 头部为 `application/sdp`；客户端 SHALL 将 SDP 解析为结构化的 media 描述，至少包含 `payload_type`、`codec`、`clock_rate`、`track_id` 字段。

### Requirement: RTP/RTCP 解复用与拆包
系统 SHALL 在 RTSP 交错 TCP 通道上对 RTP/RTCP 报文进行解复用，并 SHALL 按 track 拆出基本流：H.264/H.265 Annex-B access unit、AAC ADTS 帧。

#### Scenario: 输出 H.264 access unit
- **WHEN** 摄像头通过 TCP 交错的 RTP 推送 H.264
- **THEN** 客户端 SHALL 向 `ipcam-rtsp::VideoSink` 输出以 Annex-B start code 划分的完整 access unit（一个或多个 NALU）。

#### Scenario: 输出 AAC 帧
- **WHEN** 摄像头推送 MPEG4-GENERIC AAC（RFC 3640 / RFC 6416）
- **THEN** 客户端 SHALL 向 `ipcam-rtsp::AudioSink` 输出已封装 ADTS 的音频帧。

### Requirement: 保活与重连
系统 SHALL 通过周期性保活（默认 30 秒一次，可选 `OPTIONS` 探活或 `RTCP BYE` 规避）维持 RTSP 会话；在出现 `RTP socket closed` 或服务端 `Server: timeout` 时，系统 SHALL 在 5 秒内透明地重新执行 `SETUP`+`PLAY`。

#### Scenario: 连接断开恢复
- **WHEN** 底层 TCP socket 被摄像头或 NAT 关闭
- **THEN** RTSP 客户端 SHALL 记录 `reconnecting` 日志并重新握手，恢复帧输出，且 SHALL 不泄漏先前解码器会话分配的 buffer。

### Requirement: 外部时钟与时序
系统 SHALL 暴露流的 RTP timestamp 关系，并 SHALL 为每帧记录 wall-clock 到达时间，以便下游做音视频同步。

#### Scenario: PTS 暴露
- **WHEN** 报文被解复用
- **THEN** 关联的 sink 帧 SHALL 同时携带 `rtp_ts` 与 `arrival_us` 字段，两者 SHALL 均为微秒级精度。

