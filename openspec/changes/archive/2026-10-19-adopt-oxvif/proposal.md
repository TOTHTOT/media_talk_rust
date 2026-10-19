# adopt-oxvif — 把 ipcam-discovery 内部实现替换为 oxvif crate

## Why

`ipcam-discovery` 目前是手写的 ONVIF 客户端:UDP 多播 WS-Discovery (`lib.rs`)、`reqwest` + `quick-xml` 拼 SOAP envelopes (`device_mgmt.rs`)、WS-Security / Digest 鉴权 (`ws_security.rs`、`soap.rs`)。`repowise` 静态分析多次把这个 crate 标为最差分(5.55/10)的核心原因是 `parse_probe_message` 大脑方法 + 重造 SOAP 客户端。

发布于 [`oxvif`](https://github.com/smiti1642/oxvif) 的 `0.12.0` (MIT, 2026-04-02 发布, 29 个版本,单人维护 `smiti1642`) 提供了完整 ONVIF Profile S/T/G 实现 + 内建 MockTransport,直接复用即消除 ~700 行手写代码。

风险已与用户决议一致:**不走 spike,直接上生产**。原因:实验网段 `192.168.1.0/24` 已有 12 台测试摄像机,spike 与直接生产等价。回滚走 git revert + 重 deploy,板端无 env flag 切换。

## What Changes

- **`crates/ipcam-discovery/Cargo.toml`** 加 `oxvif = "=0.12.0"`(严格 pin,默认 features)到 `[dependencies]`
- **`crates/ipcam-discovery/src/lib.rs`** `ws_discovery_probe` 改为调 `oxvif::discovery::probe()`,私有类型映射 `oxvif::DiscoveredDevice` → `ipcam_core::DiscoveredDevice` 留内部
- **`crates/ipcam-discovery/src/device_mgmt.rs`** `DeviceManagementClient::new` 内部持 `oxvif::OnvifSession` builder;`list_profiles` / `get_stream_uri` / `get_capabilities` 转调 `OnvifSession` 对应方法
- **`crates/ipcam-discovery/src/soap.rs`** 与 **`crates/ipcam-discovery/src/ws_security.rs`** **BREAKING**(内部删除):oxvif 内部已实现 SOAP + WS-Security + Digest
- **`crates/ipcam-discovery/src/lib.rs` `ProbeMatch`** **BREAKING**:被私有映射替代;现有测试覆盖此 struct 的需要适配
- **`crates/media_talk/src/main.rs`** 默认 backend 改用 oxvif 路径,加 `MEDIA_TALK_DISCOVERY_BACKEND=legacy` env flag 切回旧实现(旧代码在 `ipcam-discovery/src/_legacy/` 路径保留作为 fallback,`make legacy` feature flag 默认开但任意时刻可以 inline 删)
- **删除 `crates/media_talk/src/ipc/oxvif_spike{,_mock}.rs`** 与 `crates/media_talk/src/ipc/oxvif-spike` feature gate:spike 路线作废
- **删除 `openspec/changes/oxvif-spike/`**(若有遗留 artifact):spike change 从未 archive,直接废弃其目录
- **删除 `docs/oxvif-spike-result.md`** 与 **`scripts/spike-against-cam.sh`**:spike 文档不再相关
- **删除 `ProbeOxvif` CLI 子命令**:oxvif 已是默认实现,无独立子命令需要
- **删除 `.github/workflows/rust.yml` 的 `oxvif-spike` job**:默认 build 已经包含 oxvif

公开 API 不变:`probe_all_with_config(DiscoveryConfig)`、`DeviceManagementClient::new` / `list_profiles` / `get_stream_uri` / `get_capabilities` 全部保留签名;`crates/media_talk/src/ipc/discover.rs` 与 `serve.rs` 调用方零修改。

## Capabilities

### New Capabilities

- `oxvif-discovery-backend`: 板端运行时可通过 `MEDIA_TALK_DISCOVERY_BACKEND` env 选择 oxvif 主路径(`oxvif`,默认)或回退到 `ipcam-discovery` 内置的旧实现(`legacy`,以 `_legacy/` 子模块形式保留)。本 capability 不引入新需求,而是把"主路径实现细节"作为可观测的回滚能力。

### Modified Capabilities

- `ipcam-discovery`:本 change 替换实现,**公开 API 契约不变**(WS-Discovery 仍发 UDP 多播到 `239.255.255.250:3702`,Device Management 仍按 ONVIF spec 返回 Profiles / StreamUri)。**delta spec 文件列出哪些 REQUIREMENT 在新实现下可能产生细微差异**(具体见 `specs/ipcam-discovery/spec.md`):
  - MO #2 (自定义 UDP 多播绑定每个接口的精确语义)→ 替换为 `oxvif::discovery::probe()` 的实现语义(由 oxvif README 表述)
  - MO #4 (重连逻辑:失败后 5s 内透明重 `SETUP`+`PLAY`)→ 改为依赖 oxvif 内部 reconnect 实现 — delta spec 标注差异
  - 所有其它 requirement(W3-DISCOVERY,S0-PROFILES,S1-STREAM_URI,S2-CLOCK 等)保持原措辞,作实施细节变更处理

## Impact

- **新增 dep**: `oxvif 0.12.0`(`crates/ipcam-discovery/Cargo.toml`),连带 minor bump:`quick-xml 0.36 → 0.41`、`reqwest 0.12 → 0.13`、`if-addrs 0.10 → 0.15`、`sha1 0.10 → 0.11`(共 28 transitive deps)。`http` / `hyper` / `tokio` 已存在,**没有版本冲突**
- **删除代码**: `crates/ipcam-discovery/src/soap.rs` + `crates/ipcam-discovery/src/ws_security.rs`(共 ~280 行)以及 `lib.rs::ProbeMatch` (struct + 4 个 helper 函数,~120 行)
- **API 表面**: 公开 `pub` 项目保留。所有 `media_talk` 已有 `discover` / `serve` / `probe` 子命令调用方零修改
- **测试**: 53 baseline(`cargo test --workspace --features sw-decode`)要求 100% 通过。现有 `parse_probe_matches_with_s` 等 mock 风格的 parse 测试可能因 `ProbeMatch` 私有化需要重新指向新的内部映射函数
- **CI**: 5 个 job 中现有的 `oxvif-spike` job 删除(默认 cargo 编译链已包含 oxvif)。其它 4 个 job 保持不变
- **生产环境风险**:
  - oxvif 单人维护(2026-04 首发,bus factor = 1)。`=0.12.0` 严格 pin,任何 0.12.x / 0.13.x 都需开新 change 评估
  - 真机(192.168.1.x 12 台摄像机,11 valid + 1 invalid creds)的兼容性未经验证直接上板;若 oxvif multicast 行为差异导致数量减少或 192.168.1.19 误标为 Valid,**只能通过 env flag 回滚**(无需重启板端进程外的任何变更)
  - media_talk.service 模板不需要变更(`Environment=RUST_LOG=info` 已存在,运维加 `MEDIA_TALK_DISCOVERY_BACKEND=legacy` 一行即可回滚)
- **回滚策略**:
  - 板端: `sed -i 's/^Environment=RUST_LOG=info$/Environment=RUST_LOG=info\nEnvironment=MEDIA_TALK_DISCOVERY_BACKEND=legacy/' /etc/systemd/system/media_talk.service && systemctl daemon-reload && systemctl restart media_talk`
  - 源码: `git revert <adopt-oxvif-commit>` + 重 deploy,需 ~5min CI

## 待用户 review 的实现约束

| 约束 | 现状 | 备注 |
| --- | --- | --- |
| `oxvif` 严格 pin | `=0.12.0`(我已写入 proposal) | 任何 0.13.x 升级开新 change |
| `MEDIA_TALK_DISCOVERY_BACKEND` env flag | 默认 `oxvif`,可选 `legacy` | 用户已确认要这个 flag |
| 旧实现保留 | `_legacy/` 子模块形式 + `legacy` feature flag 默认开 | 用户未明确是否保留;**默认保留**便于回滚 |
| Mock 测试保留 | 删 spike 阶段的 3 个 mock 测试 | 用户已确认 mock 测试"没什么用" |

如果以上任何一条你想调整,告诉我改 proposal / design / specs / tasks 哪一份。