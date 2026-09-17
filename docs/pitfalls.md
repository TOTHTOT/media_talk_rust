# 踩坑笔记

按子系统记录开发中真实踩过的坑：现象 → 根因 → 解法。每条都对应一次实际的
调试或重构（git 历史可查），不是事后总结的正确废话。

## 流媒体链路：fMP4/MSE 为什么是死路

**现象**：最初方案是 GStreamer 解复用后打成 fMP4 分片，WebSocket 推给浏览器，
前端用 MSE（MediaSource Extensions）喂 `<video>`。问题连环爆：

- `InvalidStateError: Failed to execute 'addSourceBuffer' on 'MediaSource'` ——
  在 `readyState` 不是 `open` 时调了 `addSourceBuffer`（web/play.js）
- 修好状态机后又出 `SourceBuffer error`，画面概率性花屏
- 延迟稳定一秒多，摄像头就在旁边也降不下去

**根因**：MSE 的延迟大头在浏览器侧——`SourceBuffer` 有内部缓冲，且 fMP4 分片
本身以"片段"为单位交付，天然攒延迟。花屏则来自 fMP4 muxer 细节没完全对齐
ISO 14496 与浏览器 MSE 的私有要求（init segment、timestampOffset）。

**解法**：整条链路切换为 WebRTC（`webrtcsink`，commit `3d4f9b2`）。WebRTC 是
为实时通话设计的：RTP 直推、jitter buffer 极小、浏览器硬解零缓冲，延迟降到
肉眼无感。**教训：浏览器里"低延迟直播"只有 WebRTC 一条路，MSE/FLV 都是
伪低延迟方案。**

## GStreamer

### webrtcsink 信令配置的两套属性别搞混

`webrtcsink` 连"别人起的信令服务器"要走 `signaller` 子对象的 `uri` 属性：

```rust
let signaller = ws.property::<gst::glib::Object>("signaller");
signaller.set_property("uri", format!("ws://{host}:{port}"));
```

而 `signalling-server-host`/`signalling-server-port` 是"让 webrtcsink 自己起
服务器"的模式用的。本项目是进程内共享服务器
（`ensure_signalling_server()`，幂等，ws://0.0.0.0:8443），多个 webrtcsink
实例连同一个地址不会重复开端口。浏览器侧靠 `meta,name=<stream_name>` 匹配
producer。

### pad-added：rtspsrc 每路媒体打一次回调

`rtspsrc` 是动态元件，SDP 里有几路媒体（音频/视频）就会触发几次
`pad-added`。在回调里按 caps 的 `media=(string)video/audio` 分流到各自的
depay → parse 链。**相机没有音轨时就只会来一次 video pad**——日志里
"stream has no audio track" 是正常分支不是错误。

### 视频回调按整 AU 交付，别自己拼 NAL

早期版本把一个个 NAL 单元抛给上层，下游要自己维护"哪些 NAL 属于同一帧"
的重组状态机。后来改为在 `h264parse` 输出侧按 access unit（AU，一帧的完整
NAL 集合）一次交付，删掉了整个重组状态机（commit `b97cf8c`）。**教训：
GStreamer 的 parse 元件已经帮你组好帧了，别在下游重复造轮子。**

### tee 分流：主链路不加 queue，支路必须 leaky

GUI/分析模块要原始帧时，`parse` 后接 `tee` 分路（commit `213c87d`）：

- 一路**直推 webrtcsink**，不加 queue，保持低延迟
- 一路 `queue(leaky) → decode → videoconvert → appsink` 交给回调（RawTaps）

支路的 `queue` 必须设 leaky（满了丢帧而不是反压），否则 GUI 消费慢会反过来
拖住整条管线——tee 的下游任意一路阻塞，所有路都阻塞。

### 浏览器 WebRTC 音频只认 Opus

相机出来的音频（G.711/AAC）不能直接进 webrtcsink，浏览器 WebRTC 音频强制
Opus。中间要过一次 `opusenc` 转码（commit `7296d60`）。

### Windows 开发环境

- 每条 cargo 命令前都要：
  `export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"` +
  `export PKG_CONFIG_PATH=".../lib/pkgconfig"`
- 启动日志里 `giolibproxy.dll 找不到指定的模块` 是 GStreamer 官方安装包的
  已知无害警告，不影响功能，忽略
- 早期 gst 是可选 feature（`#[cfg(feature = "gst")]`），后来改成无条件依赖
  并删掉所有 cfg 门槛（commit `a2b1048`）——功能开关带来两套编译路径，
  维护成本大于收益

## SIP（ipcam-sip）

### 注销 = 再发一次 REGISTER 且 Expires: 0

SIP 没有独立的注销消息（RFC 3261 §10.2.2）。关停时 best-effort 发
Expires:0 让服务器立刻删绑定，否则等自然过期期间，来电还会往死地址转发。

### cancel 之后包还发得出去，但收不到响应

`CancellationToken` 取消后 rsipstack 的收包循环就停了，但 UDP socket 发送
路径没有 cancel 检查——注销包发得出去，只是等不到 200 OK。所以注销要套
`timeout(2s)` 兜底，不能把关停流程卡死。

### 已知未修：注册竞态

stop 若抢在首个 REGISTER 的 200 OK 被处理之前，`registered` 还是 false，
注销整段被跳过 → 服务器上绑定残留到自然过期。低危（窗口只有几十毫秒），
已记录待修。

### rsipstack 不采纳 200 OK 里 Contact 的 expires

续期间隔不能直接用 `registration.expires()`：我们把有效期放在 Expires 头
而非 Contact 参数，rsipstack 读 Contact 永远落空到默认值 50s。显式配了
duration 就以它为准（lib.rs `register_cycle` 注释）。

### 鉴权类失败不要重试

401/403/404 重试无意义直接永久失败——401 的 digest challenge 已由
rsipstack 内部处理过，走到这还是 401 就是凭据错。其余（超时/5xx）走
1s→30s 指数退避。

### SDP 不手拼，类型化 + round-trip 测试

`sdp-rs` 的 `SessionDescription` 实现 `Display`/`FromStr`：构造完
`to_string()` 发出去，收到 answer `try_from` 读回来。注意 `times` 字段是
`Vec1`（非空 Vec）——SDP 强制至少一条 `t=` 行，构造时漏了序列化出来就是
非法报文。防御手段是 round-trip 单测：构造 → 序列化 → 必须能解析回来。

## 优雅关停

全进程共享一棵 `CancellationToken` 树（commit `d0b67c9`）：Ctrl+C/SIGTERM
触发根 token，各子系统（注册循环、流管线、web 服务）共用 token 收尾，二次
信号强退。细节见 [shutdown.md](shutdown.md)。

## 环境 / CI

- CI runner 要装 `libgstreamer1.0-dev` + `plugins-base` + `pkg-config`
  （commit `df57ef4`），否则 gst-sys 编译直接挂
- `alsa` 0.9 有 API 漂移，升级要单独验证（commit `fe9ef85`）
- `nix` 启用 fs feature 后 ioctl 调用要包 unsafe 块（commit `d77acc9`）

## 待办

- 音频 tap 实机验证（168 相机无音轨，需要一台带音频的相机）
- serve.rs 启动日志里 local_ip 的端口硬编码 `:8080`，与实际默认 bind 8081
  不符，未修
- SIP 注册竞态（见上）
