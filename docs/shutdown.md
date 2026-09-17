# 优雅关停（Ctrl+C / SIGTERM）

全进程统一的关停信号层，实现在 `crates/media_talk/src/ipc/shutdown.rs`。
目标：**一次 Ctrl+C 干净退出**（停流、断 WebRTC、释放端口），**两次 Ctrl+C 立即强退**，
任何卡死的子系统都拖不住关停。

## 三条纪律

1. **只有二进制碰 OS 信号**——信号处理只存在于 `media_talk` 的 `shutdown.rs`；
   库 crate（web-display、ipcam-gst……）一律只接收 `CancellationToken`，
   不直接 `tokio::signal`。
2. **cancel 语义 = 停止接新活 + 快速收尾**，不是立即死。子系统收到 cancel 后
   应停止接受新请求、推进手头工作到一个安全点、然后释放资源。
3. **每个子系统的 run()/serve() 必须在收尾完成后才 resolve**，由调用方
   `await` 它，并带硬 deadline 兜底。

## 信号语义

```text
SIGINT（Ctrl+C，全平台）──┐
                           ├─▶ 第一次：cancel root token，各子系统开始优雅收尾
SIGTERM（仅 Unix，─────────┘    日志：shutdown signal received, graceful stop ...
  systemctl stop 走它）
                                第二次：std::process::exit(2) 强退
                                     日志：second shutdown signal, forcing exit
```

`shutdown::install()` 在 `main` 里调用一次（目前只有 `serve` 子命令安装），
返回的 root token 逐级克隆分发：

```text
main
 └─ ipc::shutdown::install() ── root CancellationToken
     └─ ipc::serve::run(..., shutdown)
         └─ web_display::WebDisplay::start(..., shutdown.clone())
             ├─ axum .with_graceful_shutdown(shutdown.cancelled_owned())
             └─ SessionRegistry（各路 ipcam-gst 会话的句柄）
```

## 关停顺序

```text
Ctrl+C
  │  root token cancel
  ▼
serve::run 从 shutdown.cancelled().await 醒来
  │  "shutdown requested, cleaning up"
  ▼
WebDisplay::shutdown()
  1. registry.stop_all()        # 先停所有摄像头会话：gst pipeline 置 Null，
                                #    浏览器的 WebRTC/WS 连接随之中断
  2. await axum serve 句柄      # graceful：停止接新连接，存量请求处理完
     （硬超时 5s，超时 warn 放弃，不再等）
  ▼
main 返回 ExitCode::SUCCESS
```

**顺序很关键**：先停媒体会话再停 HTTP——webrtc/WS 长连接由 gst 会话供血，
会话不停，axum 的 graceful shutdown 会被长连接拖到超时。

两层兜底保证"关得掉"：

- axum 等待有 **5 秒硬超时**（`SHUTDOWN_TIMEOUT`），长连接赖着不走也拖不死流程
- 第二次信号 **exit(2)** 强退，跳过一切收尾

## 接入新子系统的清单

如果给 `serve` 加新的长生命周期子系统，照这个模式：

1. 函数签名收 `shutdown: CancellationToken`，内部 `shutdown.clone()` 分发
2. 主循环用 `tokio::select!` 同时等"正常结束"和 `shutdown.cancelled()`
3. cancel 分支里做收尾（停会话、flush、释放设备），**收尾完成后才 return**
4. 资源上限可疑的等待一律包 `tokio::time::timeout`
5. 库 crate 内部绝不调 `tokio::signal::*`、`std::process::exit`

`ipcam-gst` 侧的对应物是 `GstStreamHandle::stop()`（幂等、打断重连退避、
管线置 Null），由 `registry.stop_all()` 间接触发，见
[streaming-engine.md](streaming-engine.md#重连与状态机)。

## systemd 集成

`media_talk.service` 以独立用户运行；`systemctl stop media_talk` 默认发
SIGTERM，走的就是上面第一次信号的优雅路径。若服务卡死，systemd 自己的
`TimeoutStopSec`（默认 90s）到点会 SIGKILL，与进程内"第二次信号强退"同理。
