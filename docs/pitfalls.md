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

## SIP 视频通话媒体面 (rtp_send)

这一节来自让门口机出画面的完整排查 (commits `e667aa4` → `4652edd`),
症状演进: 黑屏无日志 → 有声音没画面 → 出画面. 排查方法本身就是收获:
**本地 loopback 集成测试 + python 脚本抓 UDP 解析 RTP 包**, 不等设备就能
验证码流形状.

### bus.iter() 提前结束 ≠ 流结束

**现象**: 发送管线启动后几十毫秒内被神秘拆掉, stats 永远 0 包, 时好时坏.

**根因**: bus 线程写 `for msg in bus.iter()`, 指望只在 EOS/Error 时退出.
但 `Iter::next` = `gst_bus_timed_pop`, **bus 进入 flushing 状态同样返回
None** — 而管线启动做状态切换时 bus 会短暂 flushing. 线程误以为是流结束,
反手 `set_state(Null)` 把刚起起来的管线杀了 (commit `e667aa4`).

**解法**: bus 线程只看 EOS/Error 打日志, 不碰管线状态; 收尾统一由
`RtpSender::stop()`/Drop 负责. **教训: GStreamer 里任何 "读到结束" 的
循环, 退出条件都要区分 "消息说结束" 和 "迭代器本身结束".**

### SPS/PPS 必须按秒级周期重发

**现象**: 信令全通, 设备端连视频解码器都不初始化 (日志里没有
`VideoDecoder::open2`).

**根因**: 设备回完 200 OK 才开收包 socket, 而我们接通瞬间就开始发, 开头
那串 SPS/PPS 基本必丢. 嵌入式解码器收不到参数集永远起不来. 抓包确认
SPS/PPS 全程只发了一次.

**解法**: `rtph264pay config-interval=1` — payloader 缓存见过的参数集按
秒级随 RTP 重发, 与 IDR 位置无关 (commit `988553f`). 注意是设在
**payloader** 上, h264parse 的 config-interval 只在 IDR 边界插入, 稀疏
IDR 的片源救不了场.

### 嵌入式设备不认 FU-A 分片

**根因**: 对照 Linphone 成功通话的设备日志, 关键差异是我们的 offer 写了
`packetization-mode=1` 且实际发 FU-A 分片包, Linphone 不写 (mode 0,
单 NAL). 设备的 RTP 接收器不认 FU-A, 大 NAL 全丢.

**解法** (commit `ac06371`):

- offer 的 fmtp 不写 `packetization-mode` (回落 mode 0)
- 片源重编码 `-x264-params slice-max-size=1300`, 每个 NAL 小于 MTU,
  从源上消除分片需求
- 顺带把测试片源从 960x400 降到 352x288 (CIF): 设备 answer 里有
  私有扩展 `a=ex_fmtp:96 2CIF=1`, 超出 2CIF 的分辨率解不动

### h264parse 转 annexb 会偷偷插 AUD

**现象**: 抓包发现线上每帧多一个 AUD (NAL type 9), 而源文件里根本没有.

**根因**: mp4 (avcC) → byte-stream 转换时 h264parse 给每个 AU 插 AUD.
部分嵌入式设备的解析器不认 AUD.

**解法**: 去掉强制 byte-stream 的 capsfilter, avc 直接喂 rtph264pay
(它本来就支持 avc 输入), 不转换就不插 AUD (commit `ac06371`).

### offer 必须包含对端偏好的编码 — 协商一致性才是黑屏的根

**现象**: 视频码流形状全部修对之后 (分支 3, commit `ac06371`), 设备
依然必黑屏; 只改音频协商的分支 4 (commit `4652edd`) 必出画面; 手机
Linphone 拨打每次都成. 分支 3 和分支 4 线上差异只有音频.

**排除实验**: 在分支 4 基础上故意发错包 — 发送 pt 写成 answer+1
(pt 1), PCMU 映射成 PCMA 编码 — 画面照出. 证明设备**不校验收到的
音频包**, "发错包打爆对端音频线程"的方向整体排除.

**根因** (双向实验确认): 这台设备是非标实现 — 无论 offer 给
什么, answer 永远选 PCMU (pt 0). answer 里出现 offer 没有的 pt 本身就
违反 RFC 3264, 设备不在乎. 真正的影响在设备内部:

- offer 含 PCMU: answer(0) 与 offer 一致, 设备音频通路一次初始化成功,
  媒体会话稳定, 视频解码器正常启动
- offer 不含 PCMU (分支 3; 反向验证: 把 offer 改回全 PCMA 必黑, 加回
  PCMU 必亮): answer(0) 与 offer(8) 自相矛盾, 设备音频初始化陷入
  reset 循环 (设备日志 `AudioModule::reset` / `alsa open err` 刷屏),
  全志这套栈的媒体处理是会话级的, 音频线程卡死连带视频解码器起不来
  — 码流形状完全正确也黑屏

即: **让设备活下来的是 offer 的协商一致性, 不是发送内容的正确性.**
分支 4 真正修对的是 offer 加了 PCMU; 按 answer 选编码/pt 是顺手做对的
合规部分 (它决定音质, 不决定视频亮不亮).

**解法**: offer 音频 fmt 列 `0 8` (PCMU 在前, 对齐设备原生
偏好), 发送端按 answer 协商结果选 `alawenc/rtppcmapay` 或
`mulawenc/rtppcmupay`, pt 用 answer 里的值 (commit `4652edd`).

**教训**:

- 调嵌入式对端的黑屏, 别只盯视频链 — 对端媒体是会话级状态机, 一路
  媒体的协商错配能阻塞所有路
- 定位靠"每次只改一个变量"的 A/B 实验, 不靠日志猜测 — 本轮排查中
  两个看似合理的理论 (SPS/PPS 位置不足论, 发送 pt 触发 ortp reset 论)
  都先被写进文档又被实验推翻, 本节是第三个版本

### 联调优先级: 先证明数据离开我们

`rtp_send` 在 payloader src pad 挂 probe 计数, 每 2s 打
`rtp sender stats video_pkts=xx audio_pkts=xx`. 联调先确认数字在涨
(数据离开本端), 再去怀疑 PBX 转发和对端解码 — 顺序反了会在错误的
环节浪费一天.

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
