# ICE crate 抽取 + ice_probe 无头测试工具 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 ICE 控制面 (sys/agent/blob) 从 ipcam-gst 抽成独立轻 crate `ipcam-ice`, 并加无头双机连通性测试工具 `ice_probe` (走 sigserver 交换 blob)。

**Architecture:** copy-first 迁移保持每步树绿: 先建新 crate 并验证独立可测, 再切 ipcam-gst / call-ui 的依赖与 import, 然后补数据面 pub API + selected-pair FFI, 最后写 example 工具。媒体面 `ice_media.rs` 留在 ipcam-gst (重度 gstreamer), 仅类型来源换 crate。

**Tech Stack:** libnice 0.1.23 手写 FFI, glib 0.22 / gio 0.22 直依赖 (不再借道 gstreamer), tokio, tokio-tungstenite 0.30, serde/serde_json, tracing。

**设计文档:** `docs/superpowers/specs/2026-11-10-ice-crate-extraction-design.md` (已对齐)。

## Global Constraints

- **全程不 `git commit`** — 本批改动与 2026-11-02 ICE 功能一起等用户口令一次性提交。每个任务完成标志 = 其门禁命令 exit 0, 不是 commit。
- **Windows 门禁/运行前置** (所有 cargo 命令前):
  ```bash
  export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
  export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
  ```
- **零新外部版本**: 依赖全部来自 [workspace.dependencies] 已钉版本; 唯一新增 `glib = "0.22"` 与已钉 `gio = "0.22"` 同族 (Cargo.lock 现有 0.22.9)。
- 搬迁类任务**不改逻辑**, 仅改 import / 路径 / 可见性; 注释与文档中文, 风格随现有文件。
- NiceCandidateType 枚举值已对照本机头文件 `candidate.h`: HOST=0 / SERVER_REFLEXIVE=1 / PEER_REFLEXIVE=2 / RELAYED=3; `type` 是 `struct _NiceCandidate` 首个字段。
- sigserver 线协议 (已对照 `crates/media_talk_rust/src/bin/sigserver.rs`): 首帧必须 `{"type":"join","room":R}`; 回 `{"type":"joined","slot":0|1}`; 之后文本帧原样中继给同房另一槽; 错误回 `{"type":"error","detail":D}`; 满员拒连。默认监听 `0.0.0.0:9000`。
- `IceAgent::new` 固定建 **2 component** (media 面既定行为), 工具与环回测试同法只用 component 1, 本计划不改它。

---

### Task 1: ipcam-ice crate 落地 (copy-first)

**Files:**
- Create: `crates/ipcam-ice/Cargo.toml`
- Create: `crates/ipcam-ice/build.rs` (复制自 `crates/ipcam-gst/build.rs`, 原样)
- Create: `crates/ipcam-ice/src/lib.rs` (内容 = 现 `crates/ipcam-gst/src/ice/mod.rs` 调整)
- Create: `crates/ipcam-ice/src/sys.rs` (复制自 `crates/ipcam-gst/src/ice/sys.rs`, 1 处 import 改)
- Create: `crates/ipcam-ice/src/agent.rs` (复制自 `crates/ipcam-gst/src/ice/agent.rs`, 3 处 import 改)
- Create: `crates/ipcam-ice/src/blob.rs` (复制自 `crates/ipcam-gst/src/ice/blob.rs`, 1 处 doc-link 改)
- Modify: `Cargo.toml` (workspace: members + glib)

**Interfaces:**
- Produces: crate `ipcam-ice`, 公共 API 与现 `ipcam_gst::ice` 完全同名同签名:
  `ipcam_ice::{IceAgent, IceConfig, IceError, IceState}`, `ipcam_ice::blob::{IceBlob, CandidateLine, CandKind, BlobError}`
- `IceConfig { pub stun: Option<String>, pub controlling: bool }`
- `IceAgent::new(cfg) -> Result<Self, IceError>`; `async local_blob(&self) -> Result<IceBlob, IceError>`;
  `async add_remote(&self, &IceBlob) -> Result<(), IceError>`; `state_rx(&self) -> watch::Receiver<IceState>`;
  `raw_agent(&self) -> *mut c_void`; `close(self)`

- [ ] **Step 1: workspace Cargo.toml 注册成员和 glib 依赖**

`Cargo.toml` members 列表 (3-14 行) 的 `"crates/ipcam-gst",` 之后加一行 `"crates/ipcam-ice",`; [workspace.dependencies] 里 `gio = "0.22"` 那行 (63-64 行, 含其上方注释) 之后加:

```toml
# ipcam-ice: 直依赖 (同 glib 0.22 生态; 不再借道 gstreamer 拿 glib)
glib = "0.22"
```

- [ ] **Step 2: 写 crates/ipcam-ice/Cargo.toml**

```toml
[package]
name = "ipcam-ice"
version = "0.1.0"
edition.workspace = true
license.workspace = true
rust-version.workspace = true
description = "libnice ICE agent 安全封装 (控制面 + 数据面, 独立于 gstreamer 媒体栈)"

[dependencies]
glib = { workspace = true }
gio = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
thiserror = { workspace = true }

[dev-dependencies]
# examples/ice_probe.rs 用 (正式依赖保持轻: glib+gio+tokio+tracing)
tokio-tungstenite = { workspace = true }
futures-util = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tracing-subscriber = { workspace = true }
```

- [ ] **Step 3: 复制 build.rs, 新建 lib.rs**

`cp crates/ipcam-gst/build.rs crates/ipcam-ice/build.rs` (内容原样: Windows 下 pkg-config 问 nice 的 libdir 加 link-search; 该文件只有这一段)。

新建 `crates/ipcam-ice/src/lib.rs`, 内容 = 现 `ice/mod.rs` 的模块文档 + 声明, 首行文档句改为 crate 定位:

```rust
//! libnice ICE agent 的安全封装: 手写 FFI (sys) + 安全包装 (agent/blob).
//! 独立于 gstreamer 媒体栈 (仅依赖 glib/gio), 供 UI 与无头测试工具复用.
/// 安全 NiceAgent 包装: glib 线程纪律 + 信令 blob 交换 + 状态订阅.
pub mod agent;
/// ICE blob: 原生 SDP 文本包装与候选行解析 (供 UI 状态展示).
pub mod blob;
pub(crate) mod sys;

pub use agent::{IceAgent, IceConfig, IceError, IceState};
```

- [ ] **Step 4: 复制 sys.rs / agent.rs / blob.rs 并改 import**

`cp` 三个文件后各改一处:

`src/sys.rs`: `use gstreamer::glib;` → `use glib;`

`src/agent.rs`:
```rust
use gstreamer::glib;
use gstreamer::glib::prelude::ObjectExt as _;
use gstreamer::glib::translate::ToGlibPtr as _;
```
→
```rust
use glib;
use glib::prelude::ObjectExt as _;
use glib::translate::ToGlibPtr as _;
```
(文件其余不动; `glib::gobject_ffi` / `glib::translate::from_glib_*` / `gio::ffi::g_networking_init` 均为直依赖下的等价路径。)

`src/blob.rs`: 文档注释内链接 [`crate::ice::IceAgent::add_remote`] → [`crate::IceAgent::add_remote`] (21 行附近, 全文唯一一处)。

- [ ] **Step 5: 门禁 — 新 crate 独立编译 + 环回测试通过**

```bash
export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
cargo test -p ipcam-ice
```

Expected: 编译通过; `blob::tests` 3 个用例 + `agent::tests::two_agents_connect_loopback` 全过 (本机有 libnice 时环回真实跑通, 缺库时按既有惯例静默跳过)。此时 ipcam-gst 尚未动, 工作树保持绿。

---

### Task 2: ipcam-gst 切换到 ipcam-ice

**Files:**
- Modify: `crates/ipcam-gst/Cargo.toml` (加依赖)
- Modify: `crates/ipcam-gst/src/lib.rs:20` (删 `pub mod ice;`)
- Modify: `crates/ipcam-gst/src/ice_media.rs:41,579` (两处 use)
- Delete: `crates/ipcam-gst/build.rs` (整个文件, 只有 libnice 一段)
- Delete: `crates/ipcam-gst/src/ice/` (整个目录)

**Interfaces:**
- Consumes: Task 1 的 `ipcam_ice::{IceAgent, IceConfig, IceState}` (ice_media.rs 的 `IceMediaConfig.agent: &IceAgent` 公共签名不变, 仅类型来源换 crate)
- Produces: ipcam-gst 不再含 ICE 控制面代码、不再链接 libnice (gst-nice 插件运行时才需要); 公共 API 其余不变

- [ ] **Step 1: Cargo.toml 加依赖, 删 build.rs**

`crates/ipcam-gst/Cargo.toml` [dependencies] 的 `ipcam-core = { path = "../ipcam-core" }` 之后加:

```toml
ipcam-ice = { path = "../ipcam-ice" }
```

`git rm crates/ipcam-gst/build.rs` (用 git rm 让删除进暂存, 与当前全部暂存的工作区状态一致; 若 git rm 因暂存怪癖失败, 直接删文件即可)。

- [ ] **Step 2: lib.rs 删 ice 模块声明**

`crates/ipcam-gst/src/lib.rs` 删第 20 行 `pub mod ice;`。grep 确认无 `pub use ice::` 类 re-export (前面已核对: 无)。

- [ ] **Step 3: ice_media.rs 换 import**

第 41 行 `use crate::ice::IceAgent;` → `use ipcam_ice::IceAgent;`

第 579 行 (测试模块内) `use crate::ice::{IceConfig, IceState};` → `use ipcam_ice::{IceConfig, IceState};`

顺带 grep `crates/ipcam-gst/src` 里剩余的 `crate::ice` 引用应为 0:
```bash
grep -rn "crate::ice" crates/ipcam-gst/src --include="*.rs"
```
Expected: 无输出。

- [ ] **Step 4: 删旧 ice 目录**

`git rm -r crates/ipcam-gst/src/ice` (同 Step 1 的 git rm 备注)。

- [ ] **Step 5: 门禁**

```bash
export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
cargo test -p ipcam-gst
```

Expected: 全套过 (含 ice_media 测试 — 其内部建双 IceAgent 环回, 验证了跨 crate 类型的链接与行为等价)。

---

### Task 3: call-ui 切换 import

**Files:**
- Modify: `crates/call-ui/Cargo.toml` (加依赖)
- Modify: `crates/call-ui/src/ice.rs:38-39,114` (三处)

**Interfaces:**
- Consumes: Task 1 的 `ipcam_ice::{IceAgent, IceConfig, IceError, IceState, blob::IceBlob}` (名字签名不变, call-ui 逻辑零改)
- Produces: 全仓最后一个 `ipcam_gst::ice` 引用点清零

- [ ] **Step 1: Cargo.toml 加依赖**

`crates/call-ui/Cargo.toml` [dependencies] 里 `ipcam-gst = { path = "../ipcam-gst" }` 之后加:

```toml
ipcam-ice = { path = "../ipcam-ice" }
```

- [ ] **Step 2: 改三处 use**

`crates/call-ui/src/ice.rs`:

```rust
use ipcam_gst::ice::blob::IceBlob;
use ipcam_gst::ice::{IceAgent, IceConfig, IceState};
```
→
```rust
use ipcam_ice::blob::IceBlob;
use ipcam_ice::{IceAgent, IceConfig, IceState};
```

第 114 行 `Agent(#[from] ipcam_gst::ice::IceError),` → `Agent(#[from] ipcam_ice::IceError),`

- [ ] **Step 3: 全盘清零核对 + 门禁**

```bash
grep -rn "ipcam_gst::ice\|ipcam-gst.*::ice" crates/ --include="*.rs"
export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
cargo clippy -p call-ui --all-targets -- -D warnings
```

Expected: grep 无输出; clippy 0 警告 (UI 全量测试在 Task 7 终检跑)。

---

### Task 4: 数据面 API 公开化 (send / attach_recv)

**Files:**
- Modify: `crates/ipcam-ice/src/agent.rs` (方法可见性 + 改名 + 文档注释; `recv_trampoline` 移出 cfg(test); mpsc import 常态化)

**Interfaces:**
- Produces (Task 6 依赖这两个签名):
  - `pub async fn send(&self, data: &[u8]) -> Result<(), IceError>` — component 1 发原始字节, connected 之后可用
  - `pub async fn attach_recv<F>(&self, f: F) -> Result<(), IceError> where F: Fn(&[u8]) + Send + 'static` — component 1 挂接收回调 (回调在独立线程触发, 永不占 glib 线程)
- 行为与现有 cfg(test) 实现完全一致, 仅翻转可见性 + 改名。

- [ ] **Step 1: 写失败测试调用 (改名后的公开名)**

`crates/ipcam-ice/src/agent.rs` 的 `mod tests` 里 `two_agents_connect_loopback` 中, 把 `a.send_test(b"ping")` 改 `a.send(b"ping")`, 两处 `attach_test_recv` 改 `attach_recv` —— 此时方法还是 cfg(test) 私有名, 编译即失败 (新方法名不存在)。

- [ ] **Step 2: 跑测试确认编译失败**

```bash
export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
cargo test -p ipcam-ice --no-run
```

Expected: 编译错误 "no method named `send` / `attach_recv`"。

- [ ] **Step 3: 提升可见性并改名**

在 `agent.rs` 中:

1. 第 19-20 行 `#[cfg(test)] use tokio::sync::mpsc;` 删掉 `#[cfg(test)]` 行属性, 变为无条件 import (attach_recv 的 trampoline 通道正式化后需要)。

2. `send_test` → `send`: 删 `#[cfg(test)]`, 改 `pub(crate)` → `pub`, 文档注释改为:

```rust
    /// 经 component 1 发原始字节 (connected 之后).
    ///
    /// 无 ICE 数据面专用错误变体, 发送失败借用 [`IceError::RemoteRejected`]
    /// 表示 (契约枚举的历史遗留, 与 cfg(test) 时期一致)。
    pub async fn send(&self, data: &[u8]) -> Result<(), IceError> {
```

3. `attach_test_recv` → `attach_recv`: 同样翻为 `pub`, 文档注释改为:

```rust
    /// component 1 挂数据接收回调.
    ///
    /// glib trampoline 把数据 marshal 进 mpsc, 由独立线程调 `f`, 用户回调
    /// 永远不占用 glib 线程。注意 (既有纪律, 升格为 pub 后更显式): 每次调用
    /// 会把回调通道 Box 泄漏到进程退出才回收 — 一次性测试工具/单测场景
    /// 无感知, 长驻进程请勿反复调用。ice_probe 与环回测试各调一次。
    pub async fn attach_recv<F>(&self, f: F) -> Result<(), IceError>
    where
        F: Fn(&[u8]) + Send + 'static,
    {
```

4. `recv_trampoline` (原 467-482 行, `#[cfg(test)] unsafe extern "C" fn`) 删 `#[cfg(test)]`, 保持模块级私有; 其 SAFETY 注释保留。

方法体一行不动。

- [ ] **Step 4: 门禁**

```bash
export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
cargo test -p ipcam-ice
```

Expected: 编译通过, 环回测试真实跑通 (send/attach_recv 行为与改名前一致 — 测试代码本身没换逻辑就是最好的等价证明)。

---

### Task 5: selected-pair FFI (TDD)

**Files:**
- Modify: `crates/ipcam-ice/src/sys.rs` (NiceCandidate 不透明类型 + get_selected_pair + candidate_type)
- Modify: `crates/ipcam-ice/src/agent.rs` (`SelectedKind` 枚举 + `selected_pair` 方法; 环回测试加断言)

**Interfaces:**
- Produces (Task 6 依赖):
  - `pub enum SelectedKind { Host, Srflx, Prflx, Relay }` (Debug/Clone/Copy/PartialEq/Eq)
  - `pub async fn selected_pair(&self, component: u32) -> Option<(SelectedKind, SelectedKind)>` — component 连通后返回 (本地, 远端) 选中候选类型; 未连通/查询失败返回 None
- sys.rs 布局依据 (已核对本机 `nice/candidate.h`): NiceCandidateType 枚举 HOST=0/SRFLX=1/PRFLX=2/RELAY=3; `type` 是 `struct _NiceCandidate` 首字段。

- [ ] **Step 1: 先写失败测试**

`crates/ipcam-ice/src/agent.rs` 的 `mod tests` 里, `two_agents_connect_loopback` 的 payload 断言之后、close 之前加:

```rust
        // 选中路径查询: 本机环回两侧都是 host 候选 (Task 5 新增 API 的 TDD 断言)
        let pair_a = a.selected_pair(1).await;
        let pair_b = b.selected_pair(1).await;
        assert_eq!(
            pair_a,
            Some((SelectedKind::Host, SelectedKind::Host)),
            "A 侧选中路径应为 host/host"
        );
        assert_eq!(
            pair_b,
            Some((SelectedKind::Host, SelectedKind::Host)),
            "B 侧选中路径应为 host/host"
        );
        println!("loopback: selected pair a={pair_a:?} b={pair_b:?}");
```

- [ ] **Step 2: 跑测试确认失败**

```bash
export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
cargo test -p ipcam-ice --no-run
```

Expected: 编译错误 "no method named `selected_pair`" / "cannot find type `SelectedKind`"。

- [ ] **Step 3: sys.rs 加 FFI**

`crates/ipcam-ice/src/sys.rs` 的 `extern "C"` 块内、`nice_agent_attach_recv` 之后加:

```rust
    pub(crate) fn nice_agent_get_selected_pair(
        agent: *mut NiceAgent,
        stream: c_uint,
        component: c_uint,
        local: *mut *mut NiceCandidate,
        remote: *mut *mut NiceCandidate,
    ) -> c_int;
```

extern 块之前加不透明类型与类型常量 (候选对象归 agent 所有, 只读首字段, 永不构造/释放):

```rust
/// NiceCandidate 不透明句柄: 只经 get_selected_pair 拿到指针并读首字段,
/// 布局私有于 libnice (candidate.h struct _NiceCandidate), 本 crate 不复制.
#[repr(C)]
pub(crate) struct NiceCandidate {
    _priv: [u8; 0],
}

// NiceCandidateType 枚举值 (candidate.h: HOST=0, SERVER_REFLEXIVE=1,
// PEER_REFLEXIVE=2, RELAYED=3).
pub(crate) const CAND_HOST: c_uint = 0;
pub(crate) const CAND_SRFLX: c_uint = 1;
pub(crate) const CAND_PRFLX: c_uint = 2;
pub(crate) const CAND_RELAY: c_uint = 3;

/// 读 NiceCandidate 首字段 `type` — 布局依据: candidate.h 中
/// `struct _NiceCandidate { NiceCandidateType type; ... }` type 为第一成员.
///
/// # Safety
/// `c` 必须指向 libnice 返回的合法 NiceCandidate (本 crate 内仅
/// get_selected_pair 出参, 已判空).
pub(crate) unsafe fn candidate_type(c: *mut NiceCandidate) -> c_uint {
    // SAFETY: 见函数文档; 首字段投影不依赖后续布局.
    unsafe { *c.cast::<c_uint>() }
}
```

- [ ] **Step 4: agent.rs 加 SelectedKind 与 selected_pair**

`IceState` 枚举之后加:

```rust
/// 选中路径上的候选类型 (ICE 测试展示用; 映射 candidate.h 的
/// NiceCandidateType, SDP 展示用的 blob::CandKind 不含 prflx 故单列).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectedKind {
    Host,
    Srflx,
    Prflx,
    Relay,
}

impl SelectedKind {
    fn from_raw(raw: c_uint) -> Option<Self> {
        match raw {
            sys::CAND_HOST => Some(Self::Host),
            sys::CAND_SRFLX => Some(Self::Srflx),
            sys::CAND_PRFLX => Some(Self::Prflx),
            sys::CAND_RELAY => Some(Self::Relay),
            _ => None,
        }
    }
}
```

(`use std::ffi::{CString, c_void};` 那行扩展为 `use std::ffi::{CString, c_uint, c_void};`)

`state_rx` 方法之后加:

```rust
    /// 查询 component 当前选中路径的候选类型 (本地, 远端).
    ///
    /// component 连通 ([`IceState::Connected`]) 后可用; 未连通 / 库未选中 /
    /// 类型值越界时返回 None。走 invoke 纪律, 可在任意线程调用。
    pub async fn selected_pair(&self, component: u32) -> Option<(SelectedKind, SelectedKind)> {
        let raw = self
            .invoke(move |agent, stream| {
                let mut local: *mut sys::NiceCandidate = std::ptr::null_mut();
                let mut remote: *mut sys::NiceCandidate = std::ptr::null_mut();
                // SAFETY: ctx 线程; agent 存活; 出参由库写入, 候选归 agent 所有
                // (本闭包不释放), 调用前后均判空.
                let ok = unsafe {
                    sys::nice_agent_get_selected_pair(
                        agent,
                        stream,
                        component,
                        &mut local,
                        &mut remote,
                    )
                };
                if ok == 1 && !local.is_null() && !remote.is_null() {
                    // SAFETY: 两指针均已判空且为库返回的合法候选.
                    Some((unsafe { sys::candidate_type(local) }, unsafe {
                        sys::candidate_type(remote)
                    }))
                } else {
                    None
                }
            })
            .await
            .ok()??;
        Some((SelectedKind::from_raw(raw.0)?, SelectedKind::from_raw(raw.1)?))
    }
```

lib.rs 的 `pub use` 补 `SelectedKind`:

```rust
pub use agent::{IceAgent, IceConfig, IceError, IceState, SelectedKind};
```

- [ ] **Step 5: 门禁**

```bash
export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
cargo test -p ipcam-ice
```

Expected: 全过; 环回测试新断言在真实 libnice 下输出 selected pair (本机为 host/host); 缺库跳过的环境不受新断言影响 (提前 return)。

---

### Task 6: ice_probe 无头测试工具 (example)

**Files:**
- Create: `crates/ipcam-ice/examples/ice_probe.rs` (全文如下)

**Interfaces:**
- Consumes: Task 1/4/5 的 `ipcam_ice::{IceAgent, IceConfig, IceState, SelectedKind}`, `ipcam_ice::blob::IceBlob`, `send`, `attach_recv`, `selected_pair`, `local_blob`, `add_remote`, `state_rx`, `close`; 外部 sigserver (`ws://HOST:9000`, 协议见 Global Constraints)。
- Produces: 可执行 example `ice_probe`; 双机同房各跑一个, 退出码 0 = 连通且数据面互通。

- [ ] **Step 1: 写 examples/ice_probe.rs 全文**

```rust
//! ice_probe: 无头 ICE 连通性测试工具.
//!
//! 两台机器各跑一个实例 (同一房名), 经 sigserver 交换 blob, 打洞连通后
//! 双向各发一帧 payload 验证数据面, 打印选中路径候选类型.
//!
//! 用法:
//!   ice_probe --sig ws://HOST:PORT [--room NAME] [--stun stun://...] [--timeout SECS]
//!
//! 先 join 者为 slot 0 (offerer/controlling), 后 join 者为 slot 1。
//! 退出码: 0 = 连通且数据面互通; 1 = 任一步失败; 2 = 参数错误。

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use ipcam_ice::blob::IceBlob;
use ipcam_ice::{IceAgent, IceConfig, IceState, SelectedKind};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{error, info, warn};

/// sigserver 线协议 (字段对齐 media_talk_rust/src/bin/sigserver.rs 的 Frame;
/// probe 只与同版本 probe 互通, 宽字段 + default 保证未知帧不炸).
#[derive(Debug, Serialize, Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    room: String,
    #[serde(default)]
    payload: String,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    slot: u8,
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsTx = futures_util::stream::SplitSink<Ws, Message>;
type WsRx = futures_util::stream::SplitStream<Ws>;

struct Args {
    sig: String,
    room: String,
    stun: Option<String>,
    timeout: Duration,
}

fn parse_args() -> Result<Args, String> {
    let mut sig: Option<String> = None;
    let mut room = "ice-probe".to_string();
    let mut stun: Option<String> = None;
    let mut secs: u64 = 30;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sig" => sig = Some(it.next().ok_or("--sig 缺少参数值")?),
            "--room" => room = it.next().ok_or("--room 缺少参数值")?,
            "--stun" => stun = Some(it.next().ok_or("--stun 缺少参数值")?),
            "--timeout" => {
                secs = it
                    .next()
                    .ok_or("--timeout 缺少参数值")?
                    .parse()
                    .map_err(|_| "--timeout 需要正整数秒数".to_string())?;
            }
            other => return Err(format!("未知参数: {other}")),
        }
    }
    let sig = sig.ok_or("缺少 --sig ws://HOST:PORT")?;
    if !sig.starts_with("ws://") && !sig.starts_with("wss://") {
        return Err(format!("--sig 需为 ws:// 或 wss:// 地址: {sig}"));
    }
    Ok(Args {
        sig,
        room,
        stun,
        timeout: Duration::from_secs(secs),
    })
}

/// 收一帧文本 Frame; 忽略非文本/无法解析的帧直到 deadline.
/// (必须是 loop 而非递归 — async fn 自递归需要 boxing, E0733)
async fn recv_frame(rx: &mut WsRx, deadline: Instant) -> Result<Frame, i32> {
    loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        let msg = match timeout(remain, rx.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => {
                error!("WS 连接错误: {e}");
                return Err(1);
            }
            Ok(None) => {
                error!("WS 连接被服务端关闭");
                return Err(1);
            }
            Err(_) => {
                error!("等待对端超时");
                return Err(1);
            }
        };
        match msg.into_text() {
            Ok(text) => match serde_json::from_str::<Frame>(&text) {
                Ok(f) => return Ok(f),
                Err(e) => warn!("忽略无法解析的帧: {e}"),
            },
            Err(_) => warn!("忽略非文本帧"),
        }
    }
}

async fn send_frame(tx: &mut WsTx, kind: &str, payload: &str) -> Result<(), i32> {
    let text = serde_json::json!({ "type": kind, "payload": payload }).to_string();
    tx.send(Message::text(text))
        .await
        .map_err(|e| {
            error!("WS 发送失败: {e}");
            1
        })
}

/// 等到一帧 type=blob 并返回 payload; error 帧直接失败退出.
async fn recv_blob(rx: &mut WsRx, deadline: Instant) -> Result<String, i32> {
    loop {
        let f = recv_frame(rx, deadline).await?;
        match f.kind.as_str() {
            "blob" => return Ok(f.payload),
            "error" => {
                error!("服务端/对端错误: {}", f.detail);
                return Err(1);
            }
            other => warn!(kind = other, "blob 交换中忽略无关帧"),
        }
    }
}

async fn run(args: Args) -> i32 {
    let deadline = Instant::now() + args.timeout;

    // 1. 连 sigserver 并 join 房间
    let ws = match timeout(args.timeout, tokio_tungstenite::connect_async(&args.sig)).await {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => {
            error!(sig = %args.sig, "连接 sigserver 失败: {e}");
            return 1;
        }
        Err(_) => {
            error!(sig = %args.sig, "连接 sigserver 超时");
            return 1;
        }
    };
    let (mut tx, mut rx) = ws.split();
    let join = serde_json::json!({ "type": "join", "room": args.room }).to_string();
    if let Err(e) = tx.send(Message::text(join)).await {
        error!("发送 join 失败: {e}");
        return 1;
    }
    let slot = loop {
        match recv_frame(&mut rx, deadline).await {
            Ok(f) => match f.kind.as_str() {
                "joined" => break f.slot,
                "error" => {
                    error!("join 被拒: {}", f.detail);
                    return 1;
                }
                other => warn!(kind = other, "join 阶段忽略无关帧"),
            },
            Err(code) => return code,
        }
    };
    info!(room = %args.room, slot, "已加入房间");

    // 2. 建 agent; slot 0 = offerer = controlling
    let agent = match IceAgent::new(IceConfig {
        stun: args.stun.clone(),
        controlling: slot == 0,
    }) {
        Ok(a) => a,
        Err(e) => {
            error!("IceAgent 创建失败 (libnice 缺失?): {e}");
            return 1;
        }
    };
    info!(controlling = slot == 0, stun = ?args.stun, "ICE agent 就绪");

    // attach 必须在等 Connected 之前 (同环回测试: attach 才把 component 的
    // socket I/O 路由到被迭代的 ctx, 否则连通性检查回包无人收).
    let (data_tx, mut data_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    if let Err(e) = agent.attach_recv(move |d| {
        let _ = data_tx.try_send(d.to_vec());
    }).await
    {
        error!("attach_recv 失败: {e}");
        agent.close();
        return 1;
    }

    // 3. blob 交换: offerer 先发包, answerer 先收包 (同 call-ui 握手序)
    if slot == 0 {
        let blob = match agent.local_blob().await {
            Ok(b) => b,
            Err(e) => {
                error!("生成本地 blob 失败: {e}");
                agent.close();
                return 1;
            }
        };
        info!(bytes = blob.as_str().len(), "本地 blob 已生成, 发给对端");
        if let Err(code) = send_frame(&mut tx, "blob", blob.as_str()).await {
            agent.close();
            return code;
        }
        let peer = match recv_blob(&mut rx, deadline).await {
            Ok(p) => p,
            Err(code) => {
                agent.close();
                return code;
            }
        };
        if let Err(e) = agent.add_remote(&IceBlob::from_sdp(peer)).await {
            error!("喂入对端 blob 被拒: {e}");
            agent.close();
            return 1;
        }
    } else {
        let peer = match recv_blob(&mut rx, deadline).await {
            Ok(p) => p,
            Err(code) => {
                agent.close();
                return code;
            }
        };
        let blob = match agent.local_blob().await {
            Ok(b) => b,
            Err(e) => {
                error!("生成本地 blob 失败: {e}");
                agent.close();
                return 1;
            }
        };
        if let Err(e) = agent.add_remote(&IceBlob::from_sdp(peer)).await {
            error!("喂入对端 blob 被拒: {e}");
            agent.close();
            return 1;
        }
        if let Err(code) = send_frame(&mut tx, "blob", blob.as_str()).await {
            agent.close();
            return code;
        }
    }

    // 4. 等 Connected
    let t0 = Instant::now();
    let mut state_rx = agent.state_rx();
    loop {
        match *state_rx.borrow() {
            IceState::Connected => break,
            IceState::Failed => {
                error!("ICE 打洞失败 (Failed)");
                agent.close();
                return 1;
            }
            _ => {}
        }
        tokio::select! {
            _ = state_rx.changed() => {}
            _ = tokio::time::sleep_until(deadline) => {
                error!("等待 Connected 超时");
                agent.close();
                return 1;
            }
        }
    }
    info!(elapsed_ms = t0.elapsed().as_millis() as u64, "ICE Connected");

    // 5. 数据面: 各发一帧 payload, 各收对端一帧
    const PING: &[u8] = b"ice-probe-ping";
    if let Err(e) = agent.send(PING).await {
        error!("数据面发送失败: {e}");
        agent.close();
        return 1;
    }
    match timeout(Duration::from_secs(10), data_rx.recv()).await {
        Ok(Some(got)) if got == PING => {
            info!(bytes = got.len(), "收到对端 payload, 数据面互通");
        }
        Ok(Some(got)) => {
            error!(
                "payload 内容不符: {:?}",
                String::from_utf8_lossy(&got)
            );
            agent.close();
            return 1;
        }
        Ok(None) => {
            error!("数据通道已关闭");
            agent.close();
            return 1;
        }
        Err(_) => {
            error!("等对端 payload 超时 (10s), 数据面不通");
            agent.close();
            return 1;
        }
    }

    // 6. 选中路径候选类型 + 结果行
    let pair = agent.selected_pair(1).await;
    let name = |k: SelectedKind| match k {
        SelectedKind::Host => "host",
        SelectedKind::Srflx => "srflx",
        SelectedKind::Prflx => "prflx",
        SelectedKind::Relay => "relay",
    };
    match pair {
        Some((local, remote)) => println!(
            "ICE OK local={} remote={} slot={}",
            name(local),
            name(remote),
            slot
        ),
        None => println!("ICE OK local=unknown remote=unknown slot={slot}"),
    }
    info!(?pair, "selected pair");
    agent.close();
    0
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    let code = match parse_args() {
        Ok(args) => run(args).await,
        Err(e) => {
            error!("参数错误: {e}");
            2
        }
    };
    std::process::exit(code);
}
```

- [ ] **Step 2: 编译门禁 (clippy --all-targets 覆盖 example)**

```bash
export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
cargo clippy -p ipcam-ice --all-targets -- -D warnings
```

Expected: 0 警告 (example 被 --all-targets 编译覆盖)。

- [ ] **Step 3: 本机双实例冒烟 (真实 sigserver + 两进程)**

终端 A:
```bash
export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
cargo run -p media_talk_rust --bin sigserver
```

终端 B (先起, 占 slot 0):
```bash
export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
cargo run -p ipcam-ice --example ice_probe -- --sig ws://127.0.0.1:9000 --room smoke
```

终端 C (后起, slot 1):
```bash
export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
cargo run -p ipcam-ice --example ice_probe -- --sig ws://127.0.0.1:9000 --room smoke
```

Expected: B/C 均打 `ICE Connected`, 随后各打 `收到对端 payload`, 末行
`ICE OK local=host remote=host slot=0/1`, 进程退出码 0。若本机防火墙拦 UDP
回环属环境问题, 与代码无关 — 复跑仍失败则记录现象上报, 不硬改代码。

---

### Task 7: 交叉编译文档 + 全量终检

**Files:**
- Modify: `docs/cross-build.md` (ICE 一节补 ice_probe)
- Modify: `docs/superpowers/specs/2026-11-10-ice-crate-extraction-design.md` (无需改 — 引用核对)

**Interfaces:**
- Consumes: Task 1-6 全部产物
- Produces: 可提交状态 (commit 等用户口令)

- [ ] **Step 1: cross-build.md 补 ice_probe 板端命令**

`docs/cross-build.md` 的 `### ICE 通话验证 (libnice)` 一节 (110 行起) 末尾、"feature 开关" 一节前加如下内容 (注意原文件此节的代码块用三个反引号, 追加时保持同级缩进):

    无头连通性测试工具 (双机对连, 不需要 GUI):

    ````text
    # 构建 (example 目标, 与 GUI 同一套交叉编译)
    docker run --rm -v "$PWD":/src -w /src dnake-rk3566-builder \
      cargo build --release --example ice_probe --target aarch64-unknown-linux-gnu
    # 板上与电脑各跑一个实例 (电脑侧用本机 cargo 出本机二进制):
    ./ice_probe --sig ws://电脑IP:9000 --room board-test
    ````

- [ ] **Step 2: 全量三门禁 (fmt / clippy / test)**

```bash
export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH"
export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features sw-decode -- -D warnings
cargo test --workspace --features sw-decode
```

Expected: fmt 无 diff; clippy 0 警告; test 全套 0 failed。

- [ ] **Step 3: 收尾报告 (不 commit)**

向用户报告: 各任务门禁结果、ice_probe 冒烟输出摘录、改动文件清单
(`git status --short`), 并提醒设计与计划两份文档也在待提交列表里, 等口令一次性提交。
