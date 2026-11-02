# ICE 跨网通话验证设计 (libnice 直控 + 双通道信令)

日期: 2026-10-30
状态: 已与用户对齐 (libnice 直控不走 webrtcbin / WS 脚本信令 + 手动 blob 兜底 /
      SIP 链路完全不动 / 测试期只配 STUN 不配 TURN)

## 需求

本程序定位是**验证程序**: 用真 ICE 打通跨网 P2P 音视频通话, 把打洞形态
(成功/失败/候选类型) 看清楚, 为产品化 (公网 SIP + STUN/TURN) 蹚路:

- 音视频双向通话, 两端各跑一个本程序实例 (开发板 + 电脑)
- 媒体通路必须过 ICE (LAN 测试也一样, host 候选会被自动选中, 全流程照跑)
- 信令: 家里测试用 Mac 上跑的微型 WS 信令服务器自动交换; 跨网 (Mac 不可达)
  时退化为人肉搬运候选 blob
- 现有 SIP 通话链路**一行不动**, ICE 通话是独立验证模式

## 非目标 (YAGNI)

- WebRTC 协议族: 不要 DTLS-SRTP / libsrtp / SDP offer-answer 语义
  (webrtcbin 方案被否, 太重; 两端跑同一程序, codec/pt 直接约定, 无需协商)
- TURN: 测试期不配。双方对称 NAT 时 P2P 打不通属已知限制, 留到 coturn 阶段
- SIP 改造: 候选不嵌 SIP SDP (RFC 8842 式), 信令与 SIP 完全解耦
- trickle ICE: 手动场景下没法做增量投递, 统一非 trickle (gather 完成一次性出全量)
- 浏览器支持 / 手机 App: 那是产品形态阶段的事

## 关键概念定稿 (对话中校准过的理解, 存档备查)

- SIP 管"这通电话的礼数" (找谁/响铃/挂断), ICE 管"RTP 物理上怎么穿 NAT",
  两层互补不互替
- ICE 信令 = 候选交换, 寄生在信令通道里; ICE 连通性检查 = 两端直连互发
  STUN 探测包, 不过服务器
- 候选三类: host (内网IP) / srflx (STUN 发现的公网映射) / relay (TURN 中继
  地址, 优先级最低, P2P 全灭才被提名)
- STUN 答"我从外面看是什么地址", 免费公网一堆 (google/cloudflare); TURN 是
  收发室代转, 吃服务器带宽, 能不用就不用
- 候选有时效性 (NAT 映射几分钟过期), 呼叫时现收集, 不注册时预存

## 总体架构

```
┌─────────── 实例 A (电脑/板子) ───────────┐   信令    ┌─────────── 实例 B ───────────┐
│  ice/ 模块 (Rust, 新)                    │  通道     │  ice/ 模块                    │
│  ├─ FFI 包 libnice (NiceAgent, GObject) │ ◀══════▶ │  ├─ FFI 包 libnice           │
│  │   挂 glib main context 独立线程       │  WS 或    │  │   (同左)                     │
│  ├─ gather (STUN 可配)                  │  人肉     │  ├─ gather                    │
│  ├─ blob 序列化/解析 (ufrag/pwd/cand)   │  搬运     │  ├─ blob 序列化/解析          │
│  └─ 状态机: gathering→checking→         │          │  └─ 状态机 (同左)             │
│      connected/failed, selected-pair    │          │                               │
│        ▼ 共享同一个 NiceAgent           │          │        ▼                     │
│  GStreamer 媒体管线 (复用现有)          │          │  GStreamer 媒体管线           │
│  发送: videosrc/audiosrc → encode →     │ ══ ICE ══ │  发送: (同左)                 │
│        payloader → nicesink ────────────┼═选定通路═▶ │        payloader → nicesink │
│  接收: nicesrc → depay → jitterbuffer → │ ◀════════ │  接收: (同左) → 显示/放音     │
│        decode → 现有 video-frame/ALSA   │  双向对称  │                               │
└──────────────────────────────────────────┘          └───────────────────────────────┘
                 ▲                                                          ▲
                 └──────── ws://Mac局域网IP:端口 (sigserver 微型转发) ──────┘
                    跨网兜底: 界面导出/导入 blob 文本, 微信人肉搬运
```

**关键取舍: libnice 直控而非 webrtcbin**。ICE 内核用 libnice (C, GObject),
媒体走它的 GStreamer 官方元件 nicesink/nicesrc——管线只换收发端, 洞打完后
数据自动在选定通路上流动, 这层零自研。代价是手写最小 FFI (~15 个符号,
libnice 0.1.x API 稳定) + glib main context 线程。

绑定选型调研 (2026-10-30 ~ 11-02, crates.io/GitHub/本机实测):

- **libnice-sys 0.4.0 / libnice 0.3.0**: 2021-01 停更, 虽 C API 冻结大概率
  仍可编译, 网络关键路径不押注无人维护的 crate; 其生成结果可作抄符号参考
- **nice-gst-meet-sys 0.3.0** (2023-06): gst-meet 实战绑定, 同样停更,
  且为其场景定制
- **Windows 链接实测可用**: gstreamer msvc 捆绑包含 `nice.lib` +
  `nice-10.dll`, 无需另装 libnice 开发包; 板端/CI 走 libnice-dev

**纯 Rust 备选 librice 深度调研 (弃用, 存档)**:
- 作者 Matthew Waters = GStreamer WebRTC 栈/webrtcbin/gst-plugins-rs 作者;
  WebKit 已采用其 sans-IO 内核做 WebRTC 候选收集; NLnet 资助; repo 持续活跃
- **致命伤 (2026-11-02 本机实证)**: crates.io 版作为普通 cargo 依赖
  **编不过**——`rice-c` build.rs 经 system-deps 探测系统级 `rice-proto`
  C 库, 实测 panic: `pkg-config --libs rice-proto` not found。其 CI 每 job
  装 cargo-c + pkg-config 且基于完整工作区 checkout 才能绿, 消费方需
  vendor 工作区 + cargo-c, 比 libnice 系统依赖更重
- `librice` 0.4.3 (2026-05) 若可构建则 API 尚可: `component.send()/recv()`
  数据面 + `to_sdp_string()`, 但作者自述 "ICE 层效率与 API 体验还需更多
  工作", 0.x 快速变动 (0.2→0.4 五个月)
- **rice-proto** (sans-IO 内核) 是唯一纯 cargo 可编译路径: 零 C 工具链,
  但要自写 IO 驱动 (~300 行, poll/poll_transmit/handle_packet/定时器) +
  appsink↔agent 媒体桥 (~150 行), 且驱动代码绑在变动期 API 上需长期跟改
- webrtc-rs 的 webrtc-ice 同理媒体桥成本, 活跃度不如 librice
- 结论: 验证期 libnice; 产品期若 gst-plugins-rs 完成切换再评估 rice 系

依据: nicesink/nicesrc 媒体零桥接 + 项目已有 rime-sys 手写 FFI 成功先例
(~30 函数, librime 1.5), 手写 libnice FFI 风险低

## 组件设计

### ice/ 模块 (核心新增, 估 300-500 行)

```
IceAgentHandle
  ├─ new(stun_server: Option<String>)      // 默认 stun.l.google.com:19302
  ├─ gather() -> IceBlob                   // 阻塞/回调至 gathering-done
  ├─ set_remote(blob: &IceBlob)            // 对端候选+凭据, 触发连通性检查
  ├─ agent() -> *mut NiceAgent             // 交给 nicesink/nicesrc 的 agent 属性
  ├─ state() -> IceState                   // new/gathering/checking/connected/failed
  └─ selected_pair() -> Option<(CandInfo, CandInfo)>  // 状态行展示用
```

- FFI: glib/nice 符号手工声明 (项目已链 glib-sys/gstreamer-sys, 无新重依赖);
  `nice_agent_new` + stream/component 各 1 (单流单组件 + RTCP-mux 约定)
- glib main context: 独立线程跑 `g_main_loop_run`, ICE 信号 (candidate-
  gathering-done / new-selected-pair / component-state-changed) 经 channel 桥回 tokio
- blob 格式 (两端程序私约, 不走 SDP 解析器):
  ```
  ice-ufrag:<s>
  ice-pwd:<s>
  candidate:<foundation> 1 UDP <priority> <ip> <port> typ host|srflx [raddr .. rport ..]
  ```
- TURN 扩展位: `set_relay_info()` 留空实现, 产品化期接 coturn

### 信令通道 (双通道, 同一 ICE 核心)

```
trait Signaling {
    async fn send_offer(&self, blob: IceBlob);
    async fn recv_answer(&self) -> IceBlob;   // 角色 A: 谁先 gather 谁先 offer
}
├─ WsChannel   → ws://<sigserver>:<port>/<room>, 收/发 blob JSON
└─ ManualChannel → 界面弹框导出本地 blob / 粘贴对端 blob
```

- WS 协议 (JSON): `{"type":"offer"|"answer"|"hangup", "payload":"<blob>"}`,
  sigserver 按 room 两两配对转发, 第三人进房直接拒绝 (验证场景无需排队)
- 角色: 非 trickle 下双方都要先完成 gather 才能出 blob, offer/answer 仅区分
  谁先发; 两个通道同一份序列化代码

### sigserver (独立 bin, ~100 行)

- workspace 新最小 crate: tokio + tokio-tungstenite, **不依赖 GStreamer**,
  Mac/Windows/Linux 都能 `cargo run` 跑
- 房间制 WS 转发: join/offer/answer/hangup 原样互转; 只这一个职责
- 产品化时它退役 (位置让给 kamialio/coturn 同构替换)

### 媒体通路

- 发送: 复用现有 video source 抽象 (file/camera/test) → H.264 编码 →
  rtph264pay → nicesink; 音频 Opus 48k (pt 111, webrtc 惯例) → rtpopuspay → nicesink
- 接收: nicesrc → depay → jitterbuffer → decode → 现有 video-frame 显示 /
  ALSA 放音, 复用对讲链路已调通的全部配置
- 编码约定 (两端同程序, 不协商): H.264 pt 96 (现有 sdp.rs 惯例), Opus pt 111;
  单 NAL/slice 模式沿用 sdp.rs 既有经验 (门口机不认 FU-A 的坑在板间不存在,
  但验证期保持一致性少踩坑)
- RTCP: rtcp-mux 约定 (单 component), 验证期不做 sender report 分析
- 收发统计: nicesink 前/nicesrc 后挂 pad probe 计字节, 喂给 UI 状态行
- 板子 CPU 预算: 验证期 640×480@15 软编可接受

### UI (设置页新增"ICE 通话验证"卡片)

- sigserver 地址/端口/房间名输入框 (存 SQLite kv, 重启保留)
- [呼叫]: WsChannel → gather → 发 offer → 等 answer → connected
- [手动模式] 开关: 改走 ManualChannel, [导出 blob]/[粘贴 blob] 弹框
- 通话中状态行: ICE state、selected pair (本地/远端地址 + host/srflx/relay
  类型标签)、收发字节/包数、挂断按钮

### 平台/依赖

- Windows 开发机: gstnice.dll 已验证在 (0.1.23, rank none 不影响按名 make)
- 板端: `apt install gstreamer1.0-nice` (armbian noble), 部署文档同步;
  板上先 `gst-inspect-1.0 nicesink` 确认再上
- 新依赖: tokio-tungstenite (sigserver 与 WsChannel 共用), libnice 系统库
- CI: rust.yml linux job 加 `libnice-dev`; Windows 本机自带无需动;
  rk3566 交叉构建容器加 `libnice-dev`

## 测试计划

1. **单元**: blob 序列化/解析 round-trip (含 srflx raddr/rport 行); 候选行
   解析边界 (缺字段/多行)
2. **LAN 自动 (主场景)**: 电脑 + 板子 (或电脑 + 电脑) 连 Mac sigserver,
   一键呼叫, host 候选选中, 视频互通, 状态行统计正常
3. **跨网 (可选)**: 一端切手机热点, 手动 blob 搬运, srflx 候选选中,
   观察打洞成功形态; 双方对称 NAT 的预期失败也要记录实际表现
4. **板端**: gst-inspect 确认 nicesink; rime 经验之外新的板端依赖项记入
   cross-build.md 与 systemd unit 注释

## 风险与限制

| 风险 | 应对 |
|---|---|
| GObject 线程纪律 (最大技术风险): libnice API 大多非线程安全, 信号回调只在 glib 线程发生 | 编码规则落成任务: 所有 nice_* 调用 marshal 回单一 glib 线程; 回调只做塞 channel; LAN 测不出跨线程 bug, 评审时专门盯 |
| 版本分裂排障噪音: 开发机 libnice 0.1.23+gst 1.28 vs 板端 apt 0.1.22+gst 1.24 | 板上排障先核对 gst-inspect 版本再怀疑代码; 差异记入部署文档 |
| 明文 RTP 无加密, 媒体面进不了产品 | 验证期接受; 产品期换 webrtcbin 或加 srtp 元件 (ICE 模块/blob/信令设计不受影响) |
| nicesink/nicesrc 极少被直接使用 (rank none) | 已两端 gst-inspect 确认存在; 出问题退 FFI send/attach_recv 直桥 |
| 非 trickle 建连秒级延迟 | 验证期接受; 产品期 trickle 或预收集 |
| 资源回收易漏 (agent/glib 线程/管线), 典型"二次呼叫挂死" | teardown 走固定顺序并加单测/日志; 挂断场景列入测试计划 |
| 双方对称 NAT 无 TURN 打不通 | 已知限制; 测试拓扑至少一端避开对称 NAT; 产品期 coturn |
| 板子软编 CPU | 验证场景 640×480@15; 上硬件编码器是产品期话题 |
| 免费 STUN 不可靠 | 验证期可接受; 产品期自建 coturn (STUN 成本极低) |
| 板端 gstnice 插件未实证 (armbian noble 理论有, 泰山派镜像可能裁剪) | 计划第一步板上 gst-inspect; 缺则 apt 装 gstreamer1.0-nice |

## 实施顺序建议

风险前置: 先做 FFI 冒烟原型 (仅 FFI 层 + LAN 双实例打洞 demo, 不接媒体
管线, 估 1 天), 通了再推进媒体与 UI。不痛快可及时换 rice-proto 内核,
沉没成本一天。

## 产品化路线 (阶段 1, 本文档不实施)

架构同构替换: sigserver 位置 → 公网 kamailio (设备注册, 候选仍走信令消息体,
服务器不解析 ICE); 免费 STUN → 公网 coturn (+TURN 兜底); 手机端 linphone
SDK; 板端 SIP 栈按需加 TCP/TLS 传输。届时 ICE 模块零改动或近零改动。

## 未决小问题 (实施计划期定)

- ice/ 模块归属: 新 crate `ipcam-ice` 还是并入 `ipcam-gst` (取决于媒体耦合面)
- UI 入口形态: 设置页卡片 vs 独立调试页
- ManualChannel 弹框复用现有人工输入组件
