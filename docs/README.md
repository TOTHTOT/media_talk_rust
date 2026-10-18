# media_talk_rust 文档

- [architecture.md](architecture.md) — workspace 整体架构：crate 分工、数据流、依赖与开发约定
- [streaming-engine.md](streaming-engine.md) — `ipcam-gst` 流媒体引擎深挖：管线拓扑、链接纪律、重连、RawTaps 原始帧出口
- [call-ui.md](call-ui.md) — `call-ui` 通话 GUI 通信框架：UI↔worker 四条通道、状态机、通知管线、分层纪律
- [shutdown.md](shutdown.md) — 优雅关停：Ctrl+C / SIGTERM 信号层、关停顺序、子系统接入规范
- [pitfalls.md](pitfalls.md) — 踩坑笔记：MSE→WebRTC 转向、GStreamer/SIP 实战坑、环境与 CI
- [cross-build.md](cross-build.md) — 交叉编译：泰山派 (RK3566) 的 Docker 构建流程、国内镜像配置、板端部署

仓库级的快速上手（编译、交叉编译、部署、CLI）见根目录 [README.md](../README.md)。
