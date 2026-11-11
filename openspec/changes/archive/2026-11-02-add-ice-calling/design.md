## Context

本程序定位是验证程序. 现有 SIP 通话链路只覆盖局域网注册-呼叫形态, 跨网 P2P 的 ICE 打洞能力没有任何通路可以观察. 本 change 加一条独立的 ICE 音视频通话验证通路: ICE 核心 (FFI / agent / blob) 与 GStreamer 媒体 (nicesink / nicesrc 管线) 全部放 `ipcam-gst`, sigserver 为 `media_talk_rust` 的第二个 bin, call-ui 加设置页卡片与后台控制器. 绑定选型已定: libnice 直控手写 FFI (不走 webrtcbin, 太重; 不走停更的 libnice-sys / nice-gst-meet-sys crate; 纯 Rust 备选 librice 因 crates.io 版编不过且 API 处 0.x 快速变动期被否, 调研存档于设计 spec).

关键概念: SIP 管「这通电话的礼数」, ICE 管「RTP 物理上怎么穿 NAT」, 两层互补不互替. 候选三类: host (内网 IP) / srflx (STUN 发现的公网映射) / relay (TURN 中继, 本 change 不配). 信令只做候选交换, 连通性检查是两端直连互发探测包, 不过服务器.

## Goals / Non-Goals

**Goals:**

- 音视频双向通话, 两端各跑一个本程序实例, 媒体通路必须过 ICE (LAN 也一样全流程照跑).
- 双通道信令: 局域网 WS sigserver 自动交换; 跨网退化为人肉导出 / 导入 blob, 两通道共用同一份序列化代码.
- 打洞形态可观测: ICE 状态、选中候选对、收发字节统计上 UI 状态行.
- 现有 SIP 通话链路一行不动, ICE 是独立验证模式.

**Non-Goals:**

- WebRTC 协议族: 不做 DTLS-SRTP / libsrtp / SDP offer-answer 语义 (两端跑同一程序, codec / pt 直接约定, 无需协商).
- TURN: 测试期不配, 双方对称 NAT 时 P2P 打不通属已知限制, 留到 coturn 阶段.
- SIP 改造: 候选不嵌 SIP SDP (RFC 8842 式), 信令与 SIP 完全解耦.
- trickle ICE: 手动场景无法增量投递, 统一非 trickle (gather 完成一次性出全量 blob).
- 浏览器支持 / 手机 App: 产品形态阶段的事.

## Decisions

### 决策 1: 无新 crate, ICE 核心全部放 `ipcam-gst`, sigserver 为 `media_talk_rust` 第二 bin

CLAUDE.md §9: 修改 workspace 根 `members` 列表需用户显式授权, 默认无权 —— 即默认不能新增 crate. CLAUDE.md §4: gstreamer API 只进 `ipcam-gst`. 在此边界内, FFI (sys) + agent + blob + 媒体管线同属一个验证特性, 没有独立发布 / 独立编译的需求, 放同一 crate 共享既有 gstreamer 依赖与错误类型, 最省.

sigserver (~100 行) 同理不做独立 crate: 作为 `media_talk_rust` 第二 bin 共享 workspace 的 tokio / tokio-tungstenite, 零新增结构; 它不依赖 GStreamer, Mac / Windows / Linux 都能 `cargo run`.

**备选**: 新增 `ipcam-ice` crate 与 `sigserver` 独立 crate. 否决 —— 违反 §9 授权边界 (需用户显式触发); crate 粒度的编译隔离对这个验证特性没有实际收益, 只会多出 members / 依赖管理成本.

### 决策 2: blob = libnice 原生 SDP 文本, 不做自定义行格式

blob 生成与入库用 libnice 自带的一对函数: `nice_agent_generate_local_sdp` (出) / `nice_agent_parse_remote_sdp` (入). 设计 spec 早期版本的自定义私约行格式 (`ice-ufrag:` / `ice-pwd:` / `candidate:` 手写行) 被否.

理由: `nice_agent_parse_remote_sdp` 的入参是 SDP 文本, 若用自定义行格式, 入库时仍需手工把候选逐个填回 `NiceCandidate` 结构体并调 `nice_agent_set_remote_candidates` —— 这要求手写 FFI 精确复刻 `NiceCandidate` 的 C 结构体内存布局, 存在跨 libnice 版本的布局漂移风险. 走原生 SDP 文本后, 结构体只作不透明指针, 序列化 / 解析全部由 libnice 自管, FFI 面最小化. 代价: blob 是人读不友好的 SDP 全文, 手动搬运场景靠界面整体导出 / 导入, 不做人工编辑.

**备选**: 自定义行格式 + 手写 `NiceCandidate` 布局 + 逐候选 set. 否决 —— 布局风险落在网络关键路径上, 收益仅为 blob 可读性.

### 决策 3: 双 component 设计 (1 = video, 2 = audio)

单个 stream 下开两个 component: component 1 走视频, component 2 走音频, rtcp-mux 约定 (不单独开 RTCP component).

理由: libnice 的一个 component 只挂一个 receive callback (`nice_agent_attach_recv`), GStreamer 侧每个 nicesrc attach 一次; 若在单 component 上挂视频 / 音频两个 nicesrc, 后 attach 的会覆盖先 attach 的, 数据面必然错乱. 双 component 后收发互不干扰, 媒体管线按 component 号路由.

**备选**: 单 component 多路流. 否决 —— attach_recv 互覆是机制级限制, 绕不开.

### 决策 4: glib 线程纪律 —— 一切 `nice_*` 调用经自建 GMainContext invoke 回 glib 线程

`IceAgent` 自建一个 `GMainContext` + 独立线程跑 `g_main_loop_run`. 纪律两条:

1. 所有 `nice_*` 调用 (new / gather / set remote / get state 等) 一律 `MainContext::invoke` 投递到该 glib 线程执行, Rust 侧经 oneshot / watch channel 拿结果, 不允许跨线程直调.
2. libnice 信号回调 (candidate-gathering-done / new-selected-pair / component-state-changed) 内不做任何 Rust 业务, 只把事件塞进 channel, 由 tokio 侧消费.

理由: `NiceAgent` 是 GObject, 非线程安全, 所有调用必须在它所属的 main context 线程; 回调里塞 channel 才能保证 tokio 侧的 async 语义与线程安全边界清晰.

**备选**: 直接调 + 加锁. 否决 —— GObject 的线程亲和不是锁能表达的, 违反纪律会以随机死锁 / 数据错乱回报.

### 决策 5: FFI 符号清单对照本机 libnice 0.1.23 头文件手写

FFI 声明 (~15 个符号) 逐个对照本机 libnice 0.1.23 头文件核实, 不照抄停更 crate (libnice-sys 0.4.0 / nice-gst-meet-sys 0.3.0, 仅作抄符号参考). 已核实枚举值 (计划 Task 1): `NICE_COMPATIBILITY_RFC5245 = 0`, `NICE_COMPONENT_STATE_GATHERING = 1` / `CONNECTING = 2` / `CONNECTED = 3` / `READY = 4` / `FAILED = 5`.

理由: C API 自 0.1.x 起冻结, 手写声明面小且可审; 网络关键路径不押注无人维护的生成绑定. 项目已有 rime-sys 手写 FFI 成功先例 (~30 函数, librime 1.5), 方法论可复制. Windows 链接已实测: gstreamer msvc 捆绑 `nice.lib` + `nice-10.dll`, 无需另装开发包; 板端 / CI 走 `libnice-dev`.

**备选**: 依赖 libnice-sys. 否决 —— 2021-01 停更, 生成结果不可控, 与其审它的生成代码不如自己写 15 行声明.

## Risks / Trade-offs

- [手写 FFI 与系统库版本漂移] → 符号清单锁定 0.1.x 头文件; Task 3 双 agent 环回冒烟风险前置, 打洞核心不通则后续任务不启动.
- [glib 线程与 tokio 跨线程死锁 / 竞态] → 决策 4 的线程纪律; 回调只塞 channel, 单写者单读者.
- [手动 blob 人肉搬运出错] → blob 为完整 SDP 文本整体导出 / 导入, 不做增量与人工编辑; 解析失败返回错误, 不进入状态机.
- [无 TURN, 双方对称 NAT 打不通] → 已知限制, 本身就是验证目标的一部分 (看清打洞形态); 产品化期再接 coturn.
- [gstnice 元件 rank 为 none] → 不影响按名 `make` 元件; 板端部署前先 `gst-inspect-1.0 nicesink` 验证 (已写入 service 注释提示).
- [sigserver 是验证期临时物] → 产品化时退役, 位置由 kamailio / coturn 同构替换; 代码隔离在独立 bin, 删除成本低.

## Migration Plan

1. Task 1: FFI + build.rs 链接 (纯新增, 不改既有行为).
2. Task 2: blob 序列化 / 解析 (TDD).
3. Task 3: `IceAgent` + 双 agent 环回冒烟 —— 风险前置, 不通则停.
4. Task 4: sigserver bin (独立, 不部署即无影响).
5. Task 5: `ice_media.rs` 媒体管线.
6. Task 6 / 7: call-ui 控制器 + 设置页卡片.
7. Task 8: CI / 交叉构建 / 文档.
8. 回归: LAN 双实例经 sigserver 呼叫通; 手动 blob 跨网搬运通; 状态行与字节统计正确.

回滚: 全部新增文件与接线, 不触碰既有链路; call-ui 卡片无入口即无行为变化; sigserver 不部署即无影响.

## Open Questions

- kv 键 (`ice_sigserver` 默认 `ws://127.0.0.1:9000` / `ice_room` 默认 `test` / `ice_stun`) 验证期存 SQLite kv, 产品化是否迁入配置中心留待后续阶段.
- 板端 640x480@15 软编 CPU 占用是否需为验证期降码率, 实测后再定.
