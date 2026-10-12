# call-ui 通话 GUI 设计

日期: 2026-10-12
状态: 已确认 (用户批准)

## 目标

为完整音视频通话方案补一个 GUI: 能在 Windows (开发调试) 和
RK3566 泰山派 (Armbian 无桌面, KMS/DRM 直显) 上运行, 支持 SIP
电话拨打与接听。框架选型: **Slint** (Rust 原生, LinuxKMS 后端
无需 X11/Wayland, Windows 走 winit 桌面后端)。

## 组件划分

### crates/call-ui (新库 crate)

| 模块 | 职责 |
| --- | --- |
| `ui/*.slint` | 界面声明, build.rs 用 slint-build 编译 |
| `app.rs` | UI 状态机: Idle / Outgoing / Incoming / InCall, 由 SIP 事件驱动切换 |
| `bridge.rs` | GUI↔后台通道封装: 命令 (Dial/Accept/Reject/Hangup) 进 tokio; 事件 (IncomingCall/Confirmed/Terminated/Failed) 经 `slint::invoke_from_event_loop` 回 UI |
| `media.rs` | 通话建立/变更时起停 `RtpSender` 与 `RtpReceiver` |
| `video.rs` | 接收端 appsink 回调, 帧转 `slint::Image` 推到 UI |

### media_talk_rust

只加薄子命令:

```
media_talk_rust gui --server 192.168.11.17:5062 --username 1000 \
    --password xxx [--media-file assets/oceans.mp4]
```

## 线程模型

- 主线程: Slint 事件循环。板子上 `SLINT_BACKEND=linuxkms-*`
  (软件渲染 DRM dumb buffer 或 FemtoVG GL ES), Windows 用 winit
- 后台线程: tokio runtime, 跑 ipcam-sip 的注册 + 来电监听 + 拨出。
  信令逻辑从 examples/call.rs / answer.rs 收进 ipcam-sip 库复用
- GStreamer 管道沿用 rtp_send / rtp_recv 现有自管线程, 不进 Slint 线程

## UI 三态 (极简)

- **Idle**: 号码输入框 + 拨打按钮
- **Outgoing**: "呼叫中…" + 取消; **Incoming**: 对端号码 + 接听/拒接
- **InCall**: 视频铺满 + 悬浮挂断按钮
- 默认窗口 800x480 (对齐目标屏), Windows 可缩放

## 媒体

- 发送: 默认 mic + camera (ingest 现有源抽象); `--media-file`
  指定 mp4 时改从文件采集 (无麦克风/摄像头设备的替代源)
- 接收: 音频走 rtp_recv 现有 playback 链直接播放; 视频走
  appsink 拉帧进 Slint 纹理 (两端同一套代码; kmssink plane
  叠加作为后续性能优化项, 不在本期)
- re-INVITE 沿用已实现的 on_update 路由: 对端地址/编码变化时
  重启对应媒体

## 错误处理

- SIP 注册失败 / 呼叫失败 (4xx/5xx/超时) → UI 错误条提示, 回 Idle
- 通话中对端 BYE / 网络中断 → 释放媒体, 回 Idle

## 测试

- bridge / app 状态机为纯逻辑, 不启动 Slint 即可单测
- 门禁: fmt + clippy + test 三件套 (含 sw-decode feature)
- 集成验证: Windows 手测 + 板子部署手测

## 不做 (YAGNI)

- 拨号键盘 / 通话记录 / 设置页 (后续迭代)
- kmssink DRM plane 叠加 (性能优化项)
- AEC (已明确暂缓)
- H.265 (后续编码兼容项)
