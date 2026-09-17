# media_talk 文档

- [architecture.md](architecture.md) — workspace 整体架构：crate 分工、数据流、依赖与开发约定
- [streaming-engine.md](streaming-engine.md) — `ipcam-gst` 流媒体引擎深挖：管线拓扑、链接纪律、重连、RawTaps 原始帧出口
- [shutdown.md](shutdown.md) — 优雅关停：Ctrl+C / SIGTERM 信号层、关停顺序、子系统接入规范
- [pitfalls.md](pitfalls.md) — 踩坑笔记：MSE→WebRTC 转向、GStreamer/SIP 实战坑、环境与 CI

仓库级的快速上手（编译、交叉编译、部署、CLI）见根目录 [README.md](../README.md)。
