# call-ui 健康度优化 + ipcam-sip 收口设计

日期: 2026-10-13
状态: 已批准
背景: repowise health 显示 call-ui 模块偏弱 — `session.rs` 7.0/10 (唯一 fair),
`run`/`incoming_inner` 被标记 large/complex/hotspot, `media.rs`+`session.rs`+`answer.rs`
三处 pick 逻辑逐字重复 (dry_violation), 另有终审遗留的 follow-up 技术债。

## 目标

- repowise: session.rs ≥8.5, call-ui 模块无 fair 档文件
- 三份编码挑选拷贝收口为 ipcam-sip 一处
- 顺带还清终审 follow-up ①②③④ (注册失败原因透出 / pick 收口 / build_av_answer
  m-line 裁剪 / receiver.rs 死调用)

## 非目标

- 不重划 call-ui 文件边界 (session.rs 不拆成多文件)
- 不改 SIP 互操作的主路径行为 (对端全音视频 offer 时 answer 逐字节不变)
- AEC、H265 发送支持等新功能

## 1. ipcam-sip 新增 negotiate 模块

`crates/ipcam-sip/src/negotiate.rs`:

```rust
pub fn pick_audio(offer: &OfferMedias, pred: impl Fn(&str) -> Option<AudioCodec>)
    -> Option<(u8, AudioCodec)>;
pub fn pick_video(offer: &OfferMedias, pred: impl Fn(&str) -> Option<VideoCodec>)
    -> Option<(u8, VideoCodec, Option<String>)>;
pub fn build_answer(
    offer: &OfferMedias, local_ip: IpAddr, audio_port: u16, video_port: u16,
    audio_pred: impl Fn(&str) -> Option<AudioCodec>,
    video_pred: impl Fn(&str) -> Option<VideoCodec>,
) -> (Option<(u8, AudioCodec)>, Option<(u8, VideoCodec, Option<String>)>, SessionDescription);
```

- 谓词由调用方传 (call-ui 传 `ipcam_gst::sendable_audio_codec` / VideoCodec 白名单
  闭包), **ipcam-sip 不反向依赖 ipcam-gst**, 分层不破
- `build_answer` 按 offer 裁剪 m-line (修 build_av_answer 既有 bug):
  - offer 没有的媒体路 → answer 不带该 m-line
  - offer 有但谓词全不匹配 → 该路 `m=` 端口置 0 (RFC 3264 拒路惯例, 明确告知拒收)
  - 两路都选中 → 输出与现 `build_av_answer` 逐字节一致 (互操作不变)
- `build_av_answer` 保留, 改为 `build_answer` 的薄封装 (内部调用, 行为不变)
- `sdp.rs` 的 `OfferCodec`/`OfferMedias` 等类型不动, negotiate 复用

## 2. process_register 透出永久失败原因

`crates/ipcam-sip/src/client.rs`:

- `Err(e)` (classify_response 判定的 401/403/404 永久失败) 分支: warn 后记住原因并
  break; 因从未注册成功, 跳过 best-effort 注销, 最终 `return Err(e)`
- 正常注销路径 (cancel 触发) 行为不变, 仍返回 Ok
- call-ui 的 `UiEvent::RegisterFailed` 因此能显示真实原因 (如 "403 Forbidden"),
  不再是 "注册循环退出: Ok(())"
- client.rs 现有测试 "永久失败返回 Ok" 改为断言 Err 且消息含状态码

## 3. call-ui 重构 (函数级拆分, 不动文件边界)

### session.rs (632 行 → 目标 <450)

- `run` → 抽 `setup()` (配置/client/通道/闭包建立) 与 `shutdown()` (补 BYE +
  client.stop + 等注销) 两段, 主 select 循环保留原处
- `incoming_inner` → 抽:
  - `wait_user_decision()` — ringing + 三选一 select (接听/拒接/超时/对端取消)
  - `accept_with_answer()` — parse offer + 488 兜底 + dialog.accept
  - `incoming_call_loop()` — 通话中 select (BYE/挂断/re-INVITE)
- `handle_cmd` 的 Dial 臂 → 抽 `prepare_dial()` (占线检查/URI 解析/占位 socket)
- 删本文件的 `pick_audio`/`pick_video`/`negotiate_answer` (~60 行), 改用
  `ipcam_sip::negotiate::{build_answer, ...}`

### media.rs

- 删自己的 pick 拷贝, renegotiate 的指纹比对改用共享 pick (指纹含义不变:
  pt+codec+fmtp)

### lib.rs

- `run` 里四个近乎相同的回调注册块 → 抽 `wire_callbacks()` 辅助函数,
  回调差异 (事件→命令映射) 用参数化闭包表达

### examples/answer.rs

- 切到共享 negotiate, 删第三份拷贝

### ipcam-gst receiver.rs

- 删两个死调用: `disable_sink_async(&appsink)` (appsink 非 Bin, no-op) 与
  tap 分支无效的 `emit-signals` 设置

## 4. 测试与验证

- `negotiate` 新单测:
  - pick 按 offer 偏好序 (多候选挑第一个支持的)
  - fmtp 回显进 answer
  - audio-only offer → answer 无 video m-line
  - 某路全不支持 → 该路端口 0
  - 全音视频 offer → 与旧 build_av_answer 输出一致 (回归钉死)
- process_register: 永久失败测试改断言 Err(含状态码)
- 门禁: fmt + clippy -D warnings + 全 workspace 测试, 全绿
- repowise health 复测: session.rs ≥8.5, call-ui 无 fair 档
- 真机回归 (用户侧): 门口机/Linphone 互拨一轮, 确认 build_answer 行为变化
  不影响主路径

## 风险

| 风险 | 缓解 |
|---|---|
| build_answer 行为变更影响互操作 | 主路径 (全音视频) 输出钉测试逐字节比对; 仅 audio-only/全不支持场景行为变化, 且原行为本就是 bug |
| 拆分引入回归 | 纯函数级移动不改逻辑; 既有 78+ 测试全绿兜底 |
| 谓词化 pick 与 sendable 白名单漂移 | 谓词由 call-ui 单点提供, 白名单仍在 ipcam-gst 一处 |

## 实现期偏差 (已裁决)

1. **`build_av_answer` 保持独立实现未改成薄封装**: 原计划让旧 answer 构造函数薄封装共享协商逻辑; 实现期发现薄封装会让主路径的逐字节回归测试自指失效 (测试比对的对象与被测实现同源), 故保留独立实现, 由逐字节钉测试兜底两条路径一致。
2. **`shutdown` 的 reg 参数带显式生命周期 `<'a>`**: 设计稿签名 `Pin<Box<dyn Future<Output = Result<()>> + Send>>` 隐含 'static, 而 `process_register()` 借用 client, 编译不过; 实际签名为 `shutdown<'a>(client: &'a SipClient, reg: Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>, ...)`。
3. **`prepare_dial` 返回值用 `DialPrep` type alias**: 设计稿的内联四元组返回签名触发 `clippy::type_complexity` (门禁 -D warnings); 拆出 `type DialPrep = (rsip::Uri, (u16, u16), (UdpSocket, UdpSocket), mpsc::Receiver<CallCtl>)` 别名替代, 未加 #[allow]。
