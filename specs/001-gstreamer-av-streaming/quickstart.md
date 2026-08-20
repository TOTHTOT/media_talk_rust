# Quickstart 验证指南: GStreamer 音视频拉流

**Date**: 2026-08-20 | **Feature**: specs/001-gstreamer-av-streaming

端到端验证本特性是否可用。前置要求、构建、运行、验收点分别对应规格 SC-001~SC-005。

## 1. 目标板（rk356x）前置检查

```bash
# 需要 core + good（rtspsrc/rtp depay）+ base；音频播放需 alsa 插件
gst-inspect-1.0 rtspsrc && gst-inspect-1.0 rtph264depay && gst-inspect-1.0 alawdec
# 任一缺失时（Debian 系固件）：
apt install gstreamer1.0-plugins-good gstreamer1.0-plugins-base gstreamer1.0-alsa
# AAC 扬声器播放（--audio-out + 摄像机为 AAC）另需 libav 插件（avdec_aac）：
apt install gstreamer1.0-libav
```

缺元件时程序会以 `GstStreamError::Init` 失败并指明元件名（见 contracts/ipcam-gst-api.md）。

## 2. 构建

**Windows 开发机本地调试**：安装 GStreamer 官方 MSVC runtime + devel 两个 MSI，把 `C:\gstreamer\1.0\msvc_x86_64\bin` 放 PATH 最前（自带 pkg-config.exe 必须优先），然后 `cargo build -p media_talk`。

**交叉编译（WSL2/Docker 内）**：

```bash
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR=/path/to/rk356x-sysroot
export PKG_CONFIG_PATH=$PKG_CONFIG_SYSROOT_DIR/usr/lib/aarch64-linux-gnu/pkgconfig
cargo build --release --target aarch64-unknown-linux-gnu -p media_talk
```

sysroot 需含 `libgstreamer1.0-dev` 与 `libgstreamer-plugins-base1.0-dev`（可直接用目标板 rootfs）。

## 3. 验收场景

### S1：实时画面（SC-001）

```bash
media_talk serve --rtsp-url "rtsp://admin:changeme@192.168.1.46:8554/ch01"
```

浏览器打开 `http://<板子IP>:8080`，创建会话后 **10 秒内出现连续画面**。日志应见 `state=Playing`。

### S2：音频（SC-003）

```bash
media_talk serve --rtsp-url "rtsp://..." --audio-out hw:0,0
```

目标板接扬声器，摄像机端制造声音，**拍手测试 10 次无可见声画错位（<200ms）**。无音频轨的摄像机：视频正常，日志注明"无音频轨"。

### S3：断流恢复（SC-004）

播放中断开摄像机网线 60 秒后恢复：**30 秒内画面自动回来**，日志可见 `state=Reconnecting` → `state=Playing` 与递增的 `reconnects` 计数。

### S4：双路并发（SC-005）

```bash
media_talk serve --rtsp-url "rtsp://...摄像机A..." --rtsp-url "rtsp://...摄像机B..."
```

两路同时打开播放页，均满足 S1；单路断网不影响另一路。

### S5：认证失败可见

故意给错误密码启动：日志出现认证失败（401 字样），该路标记失败，进程不退出。

### S6：稳定性（SC-002）

单路 1080p 连续播放 30 分钟，`top` 观察内存无持续增长。

## 4. 回归确认

- `media_talk probe --rtsp-url ...` 仍正常工作（ipcam-rtsp 的探测路径保留）。
- `cargo test --workspace` 全绿；`cargo clippy --workspace --all-targets` 无警告。
