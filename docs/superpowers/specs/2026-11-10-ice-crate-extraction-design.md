# ICE crate 抽取 + 无头连通性测试工具设计

日期: 2026-11-10
状态: 已与用户对齐 (抽 ipcam-ice 独立轻 crate / 工具走 sigserver 信令 /
      只测连通性 + 数据往返, 不测媒体面 / 工具仅保证 probe↔probe 互通)

## 背景与动机

ICE 功能 (libnice 直控) 目前落在 `crates/ipcam-gst/src/ice/` (sys/agent/blob)
+ `crates/ipcam-gst/src/ice_media.rs`, 2026-11-02 功能已实现、尚未提交。

依赖核实结论:

- `ice/` 三部分对外依赖只有 glib 家族 + tokio (mpsc/oneshot/watch) + std,
  **零 UI 依赖、零 gstreamer 媒体依赖** (现在借道 `gstreamer::glib` 只为拿 glib
  类型, 可直接依赖 glib crate)
- `ice_media.rs` 重度依赖 gstreamer (nicesink/nicesrc 管线), 但自身**不调任何
  nice_* FFI** — 只把 IceAgent 持有的 NiceAgent 指针注入元件 GObject 属性
- `IceMediaConfig.agent: &IceAgent` 本就外露在 ipcam-gst 公共 API 上
- sigserver 协议: 首帧 `{"type":"join","room":...}`, 回 `{"type":"joined","slot":0|1}`,
  之后 `{"type":...,"payload":...}` 原样中继给同房另一槽, 满员回 error

动机: 存在**不背 gstreamer 依赖栈**单独使用/测试 ICE 的场景 (两台机器无头
验证打洞)。消费方今天只有 call-ui (本来就全程依赖 ipcam-gst), sigserver 是纯
WS 中继不需要 ICE, 所以这是为"第二个消费者"提前铺路的结构性投资。

## 需求

- 新建 `crates/ipcam-ice` 轻 crate, 迁入 ICE 控制面 (sys/agent/blob +
  libnice 链接 build.rs), 正式依赖 = glib 家族 + tokio + tracing (+ thiserror),
  不含 gstreamer
- 无头测试工具 `ice_probe`: 两台机器各跑一个实例, 经 sigserver 交换 blob,
  打洞连通后双向发 payload 验证数据面, 打印选中路径候选类型
  (host/srflx/prflx/relay), 退出码 0/非 0 表示成败

## 非目标 (YAGNI)

- 不测媒体面: nicesink/nicesrc 管线留在 ipcam-gst, GUI 双实例冒烟已覆盖该链路
- 不和 GUI 程序互拨: ice_probe 只保证 probe↔probe 互通 (kind 命名顺手对齐
  call-ui 惯例, 但互操作不承诺、不测试)
- 不做 TURN 配置: `--stun` 旗标接现有 `IceConfig.stun`, TURN 仍是 coturn 阶段的事
- 不动 sigserver (协议已够用: joined 带 slot 可做确定性角色分配)
- 不做 trickle ICE: 沿用非 trickle, gather 完成一次性出全量 blob

## 设计

### crate 结构

```
crates/ipcam-ice/
├── Cargo.toml      # glib(直依赖, 版本对齐 Cargo.lock 里 gstreamer-rs 锁定的 glib)
│                   # + tokio + tracing + thiserror, 全部 workspace = true
├── build.rs        # 原样随迁 (Windows 下 pkg-config 问 nice.lib 目录加 link-search)
└── src/
    ├── lib.rs      # 原 ice/mod.rs (pub mod agent/blob, pub(crate) mod sys,
    │               # pub use agent::{IceAgent, IceConfig, IceError, IceState})
    ├── sys.rs      # 手写 FFI, 原样搬迁
    ├── agent.rs    # 安全封装 + 环回测试随迁; 新增 selected_pair_type() API
    └── blob.rs     # SDP 编解码, 原样搬迁
```

- glib 借道改直依赖是顺手优化: gstreamer-rs 对 glib 是精确锁版, 在 workspace
  声明同版本 glib, cargo 统一解析; 实现时以 Cargo.lock 现有版本为准核对
- `g_networking_init` (Windows 必需) 原来经哪个 crate 调用, 直依赖后保持等价

### ipcam-gst 改动

- 删 `src/ice/` 整个目录和 build.rs (该文件只有 libnice 一段); **搬完后
  ipcam-gst 不再链接 libnice** (gst-nice 插件运行时才需要, 板端部署不变),
  比现在更干净
- Cargo.toml 加 `ipcam-ice = { path = "../ipcam-ice" }`
- `ice_media.rs` 及其测试模块的 `use crate::ice::...` 改 `use ipcam_ice::...`;
  `IceMediaConfig.agent` 等公共签名不变, 仅类型来源换 crate
- lib.rs 删 `pub mod ice` (及可能的 re-export 行, 实现时核对)

### call-ui 改动

- Cargo.toml 加 ipcam-ice; `src/ice.rs` 的 `use ipcam_gst::ice::{...}` 改
  `use ipcam_ice::{...}`, 逻辑零改
- 实现前全盘 grep `ipcam_gst::ice` / `ice::` 引用点, 确认无遗漏

### ice_probe 工具

位置 `crates/ipcam-ice/examples/ice_probe.rs` — example + dev-dependencies
(tokio full / tokio-tungstenite / serde_json) 不污染 crate 正式依赖;
CI 的 `cargo clippy --all-targets` 自动覆盖其编译。交叉编译
`cargo build --release --example ice_probe --target ...` 与 bin 等价。

CLI:

```
--sig ws://HOST:PORT   # 必填, sigserver 地址
--room NAME            # 默认 "ice-probe"
--stun stun://...      # 可选, 透传 IceConfig.stun
--timeout SECS         # 默认 30
```

流程:

1. 连 WS, 发 `{"type":"join","room":...}`, 等 `joined` 拿 slot; error (含
   room full) 直接非 0 退出
2. **slot 0 = offerer**: `IceAgent::new` → 等 gathering 完成 → 生成本地 blob
   发出;**slot 1 = answerer**: 等对端 blob, parse 后回自己的 blob
   (IceAgent 固定建 2 component, 为 Task 3 既定行为; 工具与环回测试同法
   只用 component 1)
3. 等 `IceState::Connected` (slot 0/1 对称)
4. 双向各发一帧 payload, 断言对方收到 (复用 agent attach/recv 通道模式,
   与现有环回测试同法)
5. 打印选中路径候选类型对 (本地/远端各是 host/srflx/prflx/relay 哪种) +
   连通耗时, 退出码 0
6. 超时/对端断开/状态失败 → 打印原因, 退出码 1

### FFI 小扩展 (纯新增, 不动现有封装)

- sys.rs 加 `nice_agent_get_selected_pair` 声明 + `NiceCandidate` 首字段
  `type` 读取 (枚举对照本机 nice/agent.h: host/srflx/prflx/relay)
- agent.rs 加 `pub fn selected_pair_type(stream, component) -> Option<(候选类型对)>`
- 这是 ICE 测试的核心价值: 必须知道打洞走的是哪类候选

### 数据面 API 公开化 (实施时发现, 设计补充)

环回测试用的 `send_test` / `attach_test_recv` 目前是 `#[cfg(test)] +
pub(crate)`, example 目标编译时不带 cfg(test) 且看不见 crate 内项, 无法复用。
实施时把这两个方法提升为正式 pub API (改名 `send` / `attach_recv`), 数据面
不新增封装逻辑, 仅翻转可见性; recv 回调的 Box 泄漏至进程退出的既有纪律
在文档注释中明示 (一次性测试工具/单测场景可接受)。

### CI / 文档

- rust.yml 已装 libnice-dev (原来给 ipcam-gst, 现在给 ipcam-ice, 同一 job
  无需改); Windows job 的 PKG_CONFIG_PATH 同款
- Dockerfile.rk3566 不变 (板端 UI 程序仍需 gst-nice 运行时插件, libnice-dev
  已装)
- docs/cross-build.md 补一条 ice_probe 板端构建/运行命令

## 测试

- 现有双 agent 本机环回测试随迁 ipcam-ice, `cargo test -p ipcam-ice` 继续跑
- ice_probe 双机性质不进 CI, 编译覆盖靠 --all-targets
- 人工冒烟: Mac 跑 sigserver, 电脑 + 开发板各跑一个 ice_probe 实例对连

## 风险

- glib 直依赖版本对齐: 低风险, 以 Cargo.lock 现有 gstreamer-rs 锁定版本为准
- NiceCandidate 布局读取: 低风险, `type` 是结构体首个字段, 对照本机头文件
  核对; 不加断言不猜布局, 读不到就返回 None 并打 warn

## 提交说明

本次改动叠加在 ICE 功能未提交状态之上, 完成后仍是未提交工作区, 与
2026-11-02 ICE 功能一起等用户口令一次性提交。
