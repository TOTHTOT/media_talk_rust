# ICE 跨网通话验证实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给程序加一条独立的 ICE 音视频通话验证通路: libnice 手写 FFI 打洞, 双通道信令 (局域网 WS sigserver 自动 / 跨网手动 blob 搬运), 媒体走 nicesink/nicesrc, SIP 链路不动.

**Architecture:** ICE 核心 (FFI/agent/blob) 与 GStreamer 媒体 (nicesink/nicesrc 管线) 全部放 `ipcam-gst` (新 `ice` 模块 + `ice_media.rs`, 遵守 CLAUDE.md §4 与 §9 无新 crate); sigserver 为 `media_talk_rust` 的第二个 bin (tokio-tungstenite); call-ui 加设置页卡片 + 后台线程控制器. blob 采用 libnice 原生 SDP 文本 (对 spec 自定义行格式的偏差, 理由见 Task 2).

**Tech Stack:** libnice 0.1.x (手写 FFI, 对照本机 0.1.23 头文件), gstreamer 0.25 (已有), tokio + tokio-tungstenite 0.30 (workspace 已有), slint 1.18.

## Global Constraints

- 门禁 (Windows 开发机先 `export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH" && export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"`):
  `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets --features sw-decode -- -D warnings` / `cargo clippy -p call-ui --all-targets -- -D warnings` / `cargo test --workspace --features sw-decode`
- OpenSpec 流程 (CLAUDE.md §7/§8): 先 Task 0 propose; **archive 前与"用户说提交"前一律不 commit 代码**; 文档类提交沿用会话惯例.
- 无新 crate (§9 不改 root members); gstreamer API 只进 ipcam-gst (§4); 新依赖只允许 tokio-tungstenite (call-ui 与 media_talk_rust 各加 workspace 引用, §8 先 `cargo tree | grep` 确认已在树).
- 风格: 中文文本一律英文标点+空格; 只 tracing; 库用 thiserror 不 panic (unwrap 仅测试); `unsafe` 必须 `// SAFETY:` 注释+单测; 测试名 `tests::does_x_under_y`; 参数>3 或返回 Result 的函数用 CLAUDE.md §2 文档模板.
- 编码约定: 全部 `nice_*` 调用与信号回调 marshal 回 agent 自有的 glib 线程 (回调只塞 channel); 视频 component=1, 音频 component=2 (单 component 上多 nicesrc 会互相覆盖 attach_recv); 挂断顺序: pipeline Null → agent close.
- 常量: H264 pt=96 clock 90000; Opus pt=111 clock 48000 ch=2; STUN 默认 `stun://stun.miwifi.com:3478` (kv 全文存储, 不解析); kv key: `ice_sigserver`(默认 `ws://127.0.0.1:9000`), `ice_room`(默认 `test`), `ice_stun`.
- SIP 链路一行不动; 无 TURN; 非 trickle (gather 完成才出 blob).

## 任务接口速览

- Task 2 → `ipcam_gst::ice::blob::{IceBlob, CandidateLine, CandKind}`: `IceBlob(String)`, `as_str()`, `candidates() -> Vec<CandidateLine>`, `CandidateLine { kind: CandKind, address: IpAddr, port: u16, component: u32, foundation: String, priority: u32 }`, `CandKind::{Host,Srflx,Relay}`.
- Task 3 → `ipcam_gst::ice::agent`: `IceConfig { stun: Option<String>, controlling: bool }`, `IceState { Gathering, Connecting, Connected, Failed }`, `IceAgent::new(cfg) -> Result<Self, IceError>`, `async fn local_blob(&self) -> Result<IceBlob, IceError>`, `async fn add_remote(&self, blob: &IceBlob) -> Result<(), IceError>`, `fn state_rx(&self) -> tokio::sync::watch::Receiver<IceState>`, `pub fn raw_agent(&self) -> *mut std::ffi::c_void`, `pub(crate) async fn send_test(&self, data: &[u8]) -> Result<(), IceError>` (冒烟用).
- Task 4 → bin `sigserver`, WS 协议 JSON: `{"type":"join","room":"x"}` → `{"type":"joined","slot":0|1}`; `{"type":"offer"|"answer","payload":"<sdp>"}` 对端转发; `{"type":"hangup"}`; 满员 `{"type":"error","detail":"room full"}`.
- Task 5 → `ipcam_gst::ice_media`: `IceMediaConfig<'a> { agent: &'a IceAgent, video_src: Option<TrackSource>, audio_send: bool, audio_recv: AudioRecv, max_width: u32, taps: RawTaps }`, `AudioRecv::{Disabled, Alsa(String), TapOnly}`, `start_ice_media(cfg) -> Result<IceMediaHandle, GstStreamError>`, `IceMediaHandle { stop(), bytes() -> (u64, u64) }`.
- Task 6/7 → call-ui `UiEvent::IceStatus { state: String, tx_kb: u64, rx_kb: u64 }` + settings 卡片.

---

### Task 0: OpenSpec change scaffold

**Files:**
- Create: `openspec/changes/add-ice-calling/proposal.md`
- Create: `openspec/changes/add-ice-calling/design.md`
- Create: `openspec/changes/add-ice-calling/tasks.md`
- Create: `openspec/changes/add-ice-calling/specs/ice-calling/spec.md`

**Interfaces:** 无代码. 遵循 `openspec/changes/archive/2026-10-22-ingest-terminal-notification/` 的格式 (Why/What Changes/Capabilities/Impact).

- [ ] **Step 1: 写 proposal.md** — Why: 验证 ICE 跨网 P2P 形态; What Changes 按本计划 Task 1-8 罗列 (新增 ice 模块/ice_media/sigserver/call-ui 卡片, 全部非破坏); Capabilities: New `ice-calling`; Impact: ipcam-gst (新 pub API), media_talk_rust (新 bin), call-ui (内部接线), CI/Docker/docs; 明确 "不新增 crate 依赖除 tokio-tungstenite (已在树), 不改 SIP".
- [ ] **Step 2: 写 design.md** — 关键决策: (1) 无新 crate 的理由 (§9); (2) blob=libnice 原生 SDP (避免 NiceCandidate 结构体跨版本布局风险, 出入库一对函数); (3) 双 component 设计 (1=video, 2=audio) 及原因 (单 component 多 nicesrc 互覆 attach_recv); (4) glib 线程纪律 (所有 nice_* 调用经 MainContext::invoke, 信号回调只塞 channel); (5) FFI 符号清单对照本机 0.1.23 头文件.
- [ ] **Step 3: 写 tasks.md** — 直接镜像本计划的 Task 1-8 (一行一条, 不复制步骤).
- [ ] **Step 4: 写 specs/ice-calling/spec.md** — ADDED Requirements: "ICE 会话建立 (gather/blob 交换/连通)" / "媒体经 nicesink/nicesrc 收发 (H264 pt96, Opus pt111)" / "双通道信令 (WS sigserver / 手动 blob)" / "可观测 (状态行 + 字节统计)", 每个 Requirement 带 2-3 个 WHEN/THEN Scenario.
- [ ] **Step 5: 提交文档** (文档类, 会话惯例):

```bash
git add openspec/changes/add-ice-calling
git commit -m "[文档] openspec: add-ice-calling change 提案"
```

---

### Task 1: ipcam-gst `ice/sys.rs` FFI + build.rs 链接

**Files:**
- Create: `crates/ipcam-gst/build.rs`
- Create: `crates/ipcam-gst/src/ice/mod.rs` (内容: `pub(crate) mod sys;` + 后续任务的 `pub mod agent;` `pub mod blob;` 占位先不加)
- Create: `crates/ipcam-gst/src/ice/sys.rs`
- Modify: `crates/ipcam-gst/src/lib.rs:19-23` (模块表加 `pub mod ice;`)

**Interfaces:**
- Produces (Task 3 依赖): `sys::NiceAgent` (opaque), `sys::nice_agent_new/add_stream/gather_candidates/set_remote_credentials/generate_local_sdp/parse_remote_sdp/send/attach_recv/g_object_unref` 的 pub(crate) unsafe 包装, `sys::connect_state_changed(agent_ptr, Box<dyn Fn(u32,u32,u32)+Send>)`, `sys::connect_gathering_done(agent_ptr, Box<dyn Fn() + Send>)`, 常量 `COMPAT_RFC5245: u32 = 0`, 状态枚举裸值 `STATE_GATHERING=1/CONNECTING=2/CONNECTED=3/READY=4/FAILED=5`.

- [ ] **Step 1: build.rs (Windows 链接搜索路径, Linux 零行为)**

```rust
// libnice 链接: Linux 走默认搜索路径 (-lnice, libnice-dev 提供);
// Windows 借 gstreamer msvc 捆绑包的 nice.lib, 用 pkg-config 问出 libdir.
fn main() {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let out = std::process::Command::new("pkg-config")
            .args(["--variable=libdir", "nice"])
            .output();
        if let Ok(out) = out {
            if out.status.success() {
                let dir = String::from_utf8_lossy(&out.stdout);
                println!("cargo:rustc-link-search=native={}", dir.trim());
            }
        }
    }
}
```

Run: `cargo check -p ipcam-gst` → 通过 (此时 build.rs 无消费者, 仅验证不破坏构建).

- [ ] **Step 2: ice/sys.rs 手写 FFI (对照 D:/Soft/gstreamer/1.0/msvc_x86_64/include/nice/agent.h 逐条核实)**

```rust
//! libnice 0.1.x C API 手写绑定.
//! 逐条对照 nice/agent.h (本机 0.1.23), 只含 ICE 验证通路用到的最小面:
//!<https://libnice.freedesktop.org/libnice/NiceAgent.html>

use std::ffi::{c_char, c_int, c_uint, c_void};

use gstreamer::glib;

/// Opaque NiceAgent (GObject 子类). 释放走 `g_object_unref`.
#[repr(C)]
pub(crate) struct NiceAgent {
    _private: [u8; 0],
}

/// NiceCompatibility::NICE_COMPATIBILITY_RFC5245 (agent.h 实测值 0)
pub(crate) const COMPAT_RFC5245: c_uint = 0;
/// NiceComponentState (agent.h 实测)
pub(crate) const STATE_GATHERING: c_uint = 1;
pub(crate) const STATE_CONNECTING: c_uint = 2;
pub(crate) const STATE_CONNECTED: c_uint = 3;
pub(crate) const STATE_READY: c_uint = 4;
pub(crate) const STATE_FAILED: c_uint = 5;

unsafe extern "C" {
    #[link_name = "g_object_unref"]
    pub(crate) fn g_object_unref(object: *mut c_void);

    pub(crate) fn nice_agent_new(
        ctx: *mut glib::ffi::GMainContext,
        compat: c_uint,
    ) -> *mut NiceAgent;
    pub(crate) fn nice_agent_add_stream(agent: *mut NiceAgent, n_components: c_uint) -> c_uint;
    pub(crate) fn nice_agent_gather_candidates(agent: *mut NiceAgent, stream: c_uint) -> c_int;
    pub(crate) fn nice_agent_set_remote_credentials(
        agent: *mut NiceAgent,
        stream: c_uint,
        ufrag: *const c_char,
        pwd: *const c_char,
    ) -> c_int;
    pub(crate) fn nice_agent_generate_local_sdp(agent: *mut NiceAgent) -> *mut c_char;
    pub(crate) fn nice_agent_parse_remote_sdp(
        agent: *mut NiceAgent,
        sdp: *const c_char,
    ) -> c_int;
    pub(crate) fn nice_agent_send(
        agent: *mut NiceAgent,
        stream: c_uint,
        component: c_uint,
        len: c_uint,
        buf: *const c_char,
    ) -> c_int;
    /// NiceAgentRecvFunc: (agent, stream, component, len, buf, user_data)
    pub(crate) fn nice_agent_attach_recv(
        agent: *mut NiceAgent,
        stream: c_uint,
        component: c_uint,
        ctx: *mut glib::ffi::GMainContext,
        func: Option<unsafe extern "C" fn(*mut NiceAgent, c_uint, c_uint, c_uint, *mut c_char, *mut c_void)>,
        data: *mut c_void,
    );
}
```

注意: `unsafe extern "C"` 块是 edition 2024 语法; `g_object_unref` 经 `#[link_name]` 借用 nice 的链接 (gio 已链 glib, 符号可用). glib 路径用 `gstreamer::glib` (免加依赖).

- [ ] **Step 3: 信号连接辅助 (glib::signal::connect_raw 包装)**

```rust
/// 连接 `component-state-changed` 信号. 回调在 agent 的 glib 线程触发,
/// 实现方只能做无锁的 channel 发送 (见 agent.rs 纪律注释).
///
/// # Safety
/// * `agent` 必须是 `nice_agent_new` 返回的存活指针.
pub(crate) unsafe fn connect_state_changed<F>(agent: *mut NiceAgent, f: F)
where
    F: Fn(u32, u32, u32) + Send + 'static,
{
    unsafe extern "C" fn trampoline(
        _agent: *mut NiceAgent,
        stream: c_uint,
        component: c_uint,
        state: c_uint,
        data: *mut c_void,
    ) {
        // SAFETY: data 由本函数 Box::into_raw, 生命周期与信号连接同长, 回调只读.
        let f = unsafe { &*(data as *const F) };
        f(stream, component, state);
    }

    let data = Box::into_raw(Box::new(f)) as *mut c_void;
    // SAFETY: agent 是 GObject; 信号名以 NUL 结尾; trampoline 签名与
    // NiceAgent::component-state-changed 的 (u,u,u) 参数序一致.
    unsafe {
        glib::signal::connect_raw(
            agent.cast(),
            b"component-state-changed\0".as_ptr().cast(),
            Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(),
            >(trampoline as *const ())),
            data,
        );
    }
}

/// 连接 `candidate-gathering-done` 信号 (回调签名 ()), 结构同上.
pub(crate) unsafe fn connect_gathering_done<F>(agent: *mut NiceAgent, f: F)
where
    F: Fn() + Send + 'static,
{
    unsafe extern "C" fn trampoline(_agent: *mut NiceAgent, data: *mut c_void) {
        // SAFETY: 同 connect_state_changed.
        let f = unsafe { &*(data as *const F) };
        f();
    }

    let data = Box::into_raw(Box::new(f)) as *mut c_void;
    // SAFETY: 同上.
    unsafe {
        glib::signal::connect_raw(
            agent.cast(),
            b"candidate-gathering-done\0".as_ptr().cast(),
            Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(),
            >(trampoline as *const ())),
            data,
        );
    }
}
```

- [ ] **Step 4: lib.rs 挂载 + 门禁**

`crates/ipcam-gst/src/lib.rs` 模块区 (`mod gstutil;` 一行处) 加 `pub mod ice;`.

Run: 四门禁全跑 → 通过 (此任务无新测试, 覆盖由 Task 3 环回测试提供; clippy 可能对 `mem::transmute` 报警 — 若报 `clippy::transmute_cast` 类, 用 `unsafe extern "C" fn()` 指针直接强转 `as` 不行则保留 transmute 并加 `// reason: glib::signal::connect_raw 需要类型擦除 fn 指针` 内联注释, 符合 §8).

---

### Task 2: `ice/blob.rs` — SDP blob 与候选行解析 (TDD)

**Files:**
- Create: `crates/ipcam-gst/src/ice/blob.rs`
- Modify: `crates/ipcam-gst/src/ice/mod.rs` (加 `pub mod blob;`)

**Interfaces:**
- Produces (Task 3/6/7 依赖): `pub struct IceBlob(String)` — `new_unchecked(String)`, `pub fn as_str(&self) -> &str`, `pub fn candidates(&self) -> Vec<CandidateLine>`; `pub struct CandidateLine { pub foundation: String, pub component: u32, pub priority: u32, pub address: IpAddr, pub port: u16, pub kind: CandKind }`; `pub enum CandKind { Host, Srflx, Relay }`; `pub enum BlobError { BadCandidateLine(String) }` (thiserror).

设计说明 (deviation from spec): blob 直接承载 libnice `generate_local_sdp`/`parse_remote_sdp` 的原生 SDP 文本, 不做自定义行格式转换 — 避免翻译 NiceCandidate 结构体 (跨版本布局风险), 出入库一对函数直接对上. `candidates()` 只做文本解析用于 UI 状态展示.

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "v=0\r\n\
        m=video 1 RTP/AVP 96\r\n\
        a=ice-ufrag:abcd\r\n\
        a=ice-pwd:efghEFGHefghEFGHefgh\r\n\
        a=candidate:1 1 UDP 2013266431 192.168.1.5 51234 typ host\r\n\
        a=candidate:2 1 UDP 2013266430 118.31.0.10 37891 typ srflx raddr 192.168.1.5 rport 51234\r\n";

    #[test]
    fn parses_candidate_lines() {
        let blob = IceBlob::new_unchecked(SAMPLE.into());
        let cands = blob.candidates();
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].kind, CandKind::Host);
        assert_eq!(cands[0].address, "192.168.1.5".parse::<IpAddr>().unwrap());
        assert_eq!(cands[0].port, 51234);
        assert_eq!(cands[1].kind, CandKind::Srflx);
        assert_eq!(cands[1].foundation, "2");
    }

    #[test]
    fn ignores_unrelated_lines() {
        let blob = IceBlob::new_unchecked("v=0\r\na=ice-pwd:x\r\n".into());
        assert!(blob.candidates().is_empty());
    }

    #[test]
    fn skips_malformed_candidate_line() {
        // 缺 port 的行不产生条目也不 panic.
        let blob = IceBlob::new_unchecked(
            "a=candidate:1 1 UDP 100 10.0.0.1 typ host\r\n".into(),
        );
        assert!(blob.candidates().is_empty());
    }
}
```

Run: `cargo test -p ipcam-gst ice::blob` → FAIL (模块不存在).

- [ ] **Step 2: 实现**

```rust
//! ICE blob: libnice 原生 SDP 文本的薄包装.
//!
//! blob 在对端之间原样搬运 (WS JSON 或剪贴板), 进出库走
//! `nice_agent_generate_local_sdp` / `nice_agent_parse_remote_sdp`,
//! 本模块只做候选行解析供 UI 状态展示.

use std::net::IpAddr;

/// 对端搬运的 ICE 信令单元 (libnice SDP 文本).
#[derive(Debug, Clone)]
pub struct IceBlob(String);

impl IceBlob {
    pub(crate) fn new_unchecked(sdp: String) -> Self {
        Self(sdp)
    }

    /// 原始 SDP 文本 (WS/剪贴板搬运载体).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 解析 `a=candidate:` 行 (RFC 5245 语法) 供状态展示; 坏行跳过不 panic.
    pub fn candidates(&self) -> Vec<CandidateLine> {
        self.0
            .lines()
            .filter_map(|line| CandidateLine::parse(line.trim()).ok())
            .collect()
    }
}

/// 一条解析后的候选 (仅展示用, 不参与 ICE 逻辑).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateLine {
    pub foundation: String,
    pub component: u32,
    pub priority: u32,
    pub address: IpAddr,
    pub port: u16,
    pub kind: CandKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandKind {
    Host,
    Srflx,
    Relay,
}

impl CandidateLine {
    /// 解析单行 `a=candidate:foundation component transport priority ip port typ kind`.
    ///
    /// # Arguments
    ///
    /// * `line`: SDP 属性行, 可带 `a=` 前缀.
    ///
    /// returns: Result<CandidateLine, BlobError>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    fn parse(line: &str) -> Result<Self, BlobError> {
        let line = line.strip_prefix("a=candidate:").unwrap_or(line);
        let f: Vec<&str> = line.split_whitespace().collect();
        // 最小字段: foundation comp transport priority addr port typ kind
        if f.len() < 8 {
            return Err(BlobError::BadCandidateLine(line.into()));
        }
        let parse_kind = |s: &str| match s {
            "host" => Ok(CandKind::Host),
            "srflx" => Ok(CandKind::Srflx),
            "relay" => Ok(CandKind::Relay),
            other => Err(BlobError::BadCandidateLine(format!("typ {other}"))),
        };
        Ok(Self {
            foundation: f[0].into(),
            component: f[1].parse().map_err(|_| BlobError::BadCandidateLine(line.into()))?,
            priority: f[3].parse().map_err(|_| BlobError::BadCandidateLine(line.into()))?,
            address: f[4].parse().map_err(|_| BlobError::BadCandidateLine(line.into()))?,
            port: f[5].parse().map_err(|_| BlobError::BadCandidateLine(line.into()))?,
            kind: parse_kind(f[7])?,
        })
    }
}

/// blob 文本解析错误 (展示路径, 非 ICE 控制路径).
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("bad candidate line: {0}")]
    BadCandidateLine(String),
}
```

注意: `thiserror` 已是 ipcam-gst 依赖 (检查 Cargo.toml, 非则加 workspace 引用). `new_unchecked` 仅 crate 内 (Task 3 agent.rs 调用).

- [ ] **Step 3: 跑测试 + 门禁**

Run: `cargo test -p ipcam-gst ice::blob` → 3 passed; 四门禁.

---

### Task 3: `ice/agent.rs` — IceAgent + 双 agent 环回冒烟测试 (风险前置)

**Files:**
- Create: `crates/ipcam-gst/src/ice/agent.rs`
- Modify: `crates/ipcam-gst/src/ice/mod.rs` (加 `pub mod agent;` 与 re-export)
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: Task 1 `sys::*`, Task 2 `IceBlob::new_unchecked`.
- Produces (Task 5/6 依赖): 见"任务接口速览" Task 3 条目. `IceError` (thiserror): `NullAgent / CreateStream / GatherFailed / RemoteRejected / Timeout(&'static str) / Closed`.

- [ ] **Step 1: 写失败测试 (双 agent 环回, 即 spec 的 FFI 冒烟原型)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 双 agent 本机环回: 无 STUN, host 候选打洞, 数据双向可达.
    /// libnice 缺失 (库未装) 时静默跳过, 与 gst 缺件 guard 同惯例.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_agents_connect_loopback() {
        let Ok(a) = IceAgent::new(IceConfig { stun: None, controlling: true }) else {
            return; // libnice 不可用
        };
        let Ok(b) = IceAgent::new(IceConfig { stun: None, controlling: false }) else {
            return;
        };

        let (a_blob, b_blob) = tokio::join!(a.local_blob(), b.local_blob());
        let (a_blob, b_blob) = (a_blob.unwrap(), b_blob.unwrap());
        tokio::join!(a.add_remote(&b_blob), b.add_remote(&a_blob)).0.unwrap();
        b.add_remote_check().await.unwrap(); // 见下方对称补调说明

        wait_connected(&a).await;
        wait_connected(&b).await;

        // 数据面: A→B 经 nice_agent_send + attach_recv 验证
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        b.attach_test_recv(move |data| { let _ = tx.try_send(data.to_vec()); }).await.unwrap();
        a.send_test(b"ping").await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("recv timeout")
            .expect("channel closed");
        assert_eq!(got, b"ping");
    }

    async fn wait_connected(agent: &IceAgent) {
        let mut rx = agent.state_rx();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if *rx.borrow() == IceState::Connected {
                return;
            }
            tokio::select! {
                _ = rx.changed() => {},
                _ = tokio::time::sleep_until(deadline) => panic!("ICE not connected"),
            }
        }
    }
}
```

注意: `add_remote` 两侧都要调 (`tokio::join!` 已覆盖, 多余的 `add_remote_check` 行删除——实现以 join 为准).

Run: `cargo test -p ipcam-gst ice::agent` → FAIL (类型不存在).

- [ ] **Step 2: 实现 agent.rs**

```rust
//! libnice NiceAgent 的安全包装.
//!
//! 线程纪律 (spec 风险#1 的编码规则): NiceAgent 绑定自建 GMainContext,
//! 独立线程跑 MainLoop; 一切 `nice_*` 调用经 `MainContext::invoke` marshal
//! 回该线程执行; GObject 信号回调同样在该线程触发,  trampoline 内只做
//! 无锁 channel 发送, 不做业务.

use std::ffi::{c_void, CString};
use std::sync::{mpsc as std_mpsc, Arc};
use std::time::Duration;

use gstreamer::glib;
use tokio::sync::{mpsc, oneshot, watch};

use super::blob::IceBlob;
use super::sys;

/// agent 配置.
pub struct IceConfig {
    /// STUN server URI (如 `stun://stun.miwifi.com:3478`), None = 仅 host 候选.
    pub stun: Option<String>,
    /// true = controlling (通常 offer 方), false = controlled.
    pub controlling: bool,
}

/// 组件连通状态 (映射 NiceComponentState, UI 展示用).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceState {
    Gathering,
    Connecting,
    Connected,
    Failed,
}

/// 公共 ICE agent 句柄.
pub struct IceAgent {
    ctx: glib::MainContext,
    agent: *mut sys::NiceAgent,
    stream: u32,
    state_tx: watch::Sender<IceState>,
    _loop_thread: Arc<std::thread::JoinHandle<()>>, // 关停时 drop
}

// SAFETY: agent 指针只经 MainContext::invoke 访问, Send 供跨线程持有.
unsafe impl Send for IceAgent {}
// SAFETY: 内部状态经 channel/watch 同步, Sync 供多线程只读引用.
unsafe impl Sync for IceAgent {}

impl IceAgent {
    /// 创建 agent: 起 glib 线程, 建流 (2 component: 1=video, 2=audio), 挂信号.
    ///
    /// # Arguments
    ///
    /// * `cfg`: STUN URI 与 controlling 角色, 见 `IceConfig`.
    ///
    /// returns: Result<IceAgent, IceError> — 库缺失/建流失败时 Err
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn new(cfg: IceConfig) -> Result<Self, IceError> {
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<u32, IceError>>();
        let (state_tx, _state_rx) = watch::channel(IceState::Gathering);

        let thread = std::thread::Builder::new()
            .name("ice-glib".into())
            .spawn(move || {
                let ctx = glib::MainContext::new();
                let mainloop = glib::MainLoop::new(Some(&ctx), false);
                let state_cb = state_tx.clone();
                let ready2 = ready_tx.clone();
                ctx.with_thread_default(|| {
                    // SAFETY: agent 在 ctx 线程创建并随 ctx 存活, invoke 纪律见模块注释.
                    let agent = unsafe {
                        sys::nice_agent_new(ctx.as_ptr(), sys::COMPAT_RFC5245)
                    };
                    if agent.is_null() {
                        let _ = ready2.send(Err(IceError::NullAgent));
                        return;
                    }
                    let gobj: glib::Object = unsafe { glib::translate::from_glib_none(agent) };
                    gobj.set_property("controlling-mode", cfg.controlling);
                    if let Some(stun) = cfg.stun {
                        gobj.set_property("stun-server", stun);
                    }
                    // SAFETY: 信号回调内只发 channel, 符合线程纪律.
                    unsafe {
                        sys::connect_state_changed(agent, move |_s, _c, raw| {
                            let s = match raw {
                                sys::STATE_GATHERING => Some(IceState::Gathering),
                                sys::STATE_CONNECTING => Some(IceState::Connecting),
                                sys::STATE_CONNECTED | sys::STATE_READY => Some(IceState::Connected),
                                sys::STATE_FAILED => Some(IceState::Failed),
                                _ => None,
                            };
                            if let Some(s) = s {
                                let _ = state_cb.send_replace(s);
                            }
                        });
                    }
                    // SAFETY: add_stream 在 ctx 线程内调用.
                    let stream = unsafe { sys::nice_agent_add_stream(agent, 2) };
                    if stream == 0 {
                        let _ = ready2.send(Err(IceError::CreateStream));
                        return;
                    }
                    let _ = ready2.send(Ok(stream));
                    mainloop.run();
                });
            })
            .map_err(|_| IceError::NullAgent)?;

        let stream = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| IceError::Timeout("agent init"))?
            .inspect_err(|_| drop(state_tx))??;
        Ok(Self {
            ctx: /* 同 ctx: 建线程外再建一个引用 */,
            agent: /* 线程内指针传出 via channel */,
            stream,
            state_tx,
            _loop_thread: Arc::new(thread),
        })
    }
    // ... 其余方法见下
}
```

问题: `ctx` 与 `agent` 在线程闭包内创建, 需经 channel 传出 (`ready` 改为传 `(u32, MainContext, *mut NiceAgent)`). 实现时调整: `ready_tx.send(Ok((stream, ctx.clone(), agent)))` (`MainContext: Clone`). `ctx.as_ptr()` 用 `glib::translate::ToGlibPtr::to_glib_none(&ctx).0` 若 as_ptr 不存在.

- [ ] **Step 3: 其余方法 (invoke 模式)**

```rust
    /// 收集候选并产出本地 blob (阻塞至 gathering-done, 上限 10s).
    pub async fn local_blob(&self) -> Result<IceBlob, IceError> {
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let done_tx = std::sync::Mutex::new(Some(done_tx));
        // SAFETY: connect 在 invoke 内执行, agent 存活.
        self.invoke(move |agent, _stream| unsafe {
            sys::connect_gathering_done(agent, move || {
                if let Some(tx) = done_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            });
        })
        .await;
        self.invoke(|agent, stream| unsafe {
            if sys::nice_agent_gather_candidates(agent, stream) == 0 {
                // 立即失败 (无候选地址), 错误经返回值携带 — 实现用 channel 回传 bool
            }
        })
        .await;
        tokio::time::timeout(Duration::from_secs(10), done_rx)
            .await
            .map_err(|_| IceError::Timeout("gather"))??;
        let sdp = self
            .invoke(|agent, _| unsafe {
                sys::nice_agent_generate_local_sdp(agent)
            })
            .await;
        if sdp.is_null() {
            return Err(IceError::GatherFailed);
        }
        // SAFETY: 所有权从 C 转移, from_glib_full 会 g_free.
        Ok(IceBlob::new_unchecked(unsafe {
            glib::translate::from_glib_full(sdp)
        }))
    }

    /// 喂入对端 blob (credentials + candidates), 触发连通性检查.
    pub async fn add_remote(&self, blob: &IceBlob) -> Result<(), IceError> {
        let sdp = CString::new(blob.as_str()).map_err(|_| IceError::RemoteRejected)?;
        let ok = self
            .invoke(move |agent, _| unsafe { sys::nice_agent_parse_remote_sdp(agent, sdp.as_ptr()) })
            .await;
        if ok < 0 {
            return Err(IceError::RemoteRejected);
        }
        Ok(())
    }

    /// 状态订阅.
    pub fn state_rx(&self) -> watch::Receiver<IceState> {
        self.state_tx.subscribe()
    }

    /// 原始 NiceAgent 指针, 仅供 ice_media 设置 nicesink/nicesrc `agent` 属性.
    ///
    /// # Safety
    /// 调用方 MUST 保证 agent 存活于所有使用者之后 (IceMediaHandle 先于 IceAgent drop).
    pub fn raw_agent(&self) -> *mut c_void {
        self.agent.cast()
    }

    /// 冒烟测试数据面: 经 component 1 发原始字节.
    pub(crate) async fn send_test(&self, data: &[u8]) -> Result<(), IceError> {
        let len = u32::try_from(data.len()).map_err(|_| IceError::RemoteRejected)?;
        let sent = self
            .invoke(move |agent, stream| unsafe {
                sys::nice_agent_send(agent, stream, 1, len, data.as_ptr().cast())
            })
            .await;
        if sent < 0 {
            return Err(IceError::RemoteRejected);
        }
        Ok(())
    }

    /// 冒烟测试数据面: attach_recv, 数据经回调进 channel (component 1).
    pub(crate) async fn attach_test_recv<F>(&self, f: F) -> Result<(), IceError>
    where
        F: Fn(&[u8]) + Send + 'static,
    {
        let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);
        std::thread::spawn(move || {
            // glib trampoline 已把回调 marshal 到普通线程安全 channel, 此处只是搬运
            while let Ok(d) = data_rx.blocking_recv() {
                f(&d);
            }
        });
        self.invoke(move |agent, stream| unsafe {
            let ctx_ptr = std::ptr::null_mut(); // NULL = agent 的默认 ctx
            sys::nice_agent_attach_recv(
                agent,
                stream,
                1,
                ctx_ptr,
                Some(recv_trampoline),
                Box::into_raw(Box::new(data_tx)) as *mut c_void,
            );
        })
        .await;
        Ok(())
    }
```

`invoke` 辅助 (泛型闭包在 ctx 线程执行, 返回值经 oneshot 回传):

```rust
    /// 把闭包 marshal 到 agent 的 glib 线程执行并取回结果.
    async fn invoke<T, F>(&self, f: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(*mut sys::NiceAgent, u32) -> T + Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<T>();
        let agent = self.agent;
        let stream = self.stream;
        // glib::MainContext::invoke 调度闭包到 ctx 线程并阻塞当前线程至完成 —
        // 这里用 spawn + channel 避免阻塞 tokio worker (spawn 不等待, 结果走 tx).
        self.ctx.spawn(move || {
            let out = f(agent, stream);
            let _ = tx.send(out);
        });
        rx.await.map_err(|_| IceError::Closed).unwrap_or_else(|_| unreachable!("invoke"))
    }
```

注意: `ctx.spawn` 不返回值 — 闭包内发 tx. `rx.await` Err 分支 → `IceError::Closed` (实现把 invoke 返回类型改 `Result<T, IceError>`, 去掉 unreachable!). `recv_trampoline`:

```rust
unsafe extern "C" fn recv_trampoline(
    _agent: *mut sys::NiceAgent,
    _stream: std::ffi::c_uint,
    _component: std::ffi::c_uint,
    len: std::ffi::c_uint,
    buf: *mut std::ffi::c_char,
    data: *mut c_void,
) {
    // SAFETY: data 是 Box<mpsc::Sender<Vec<u8>>>, 回调在 agent ctx 线程触发,
    // try_send 无锁; buf 长度 len 有效.
    let tx = unsafe { &*(data as *const mpsc::Sender<Vec<u8>>) };
    let bytes = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len as usize) };
    let _ = tx.try_send(bytes.to_vec());
}
```

`IceError`:

```rust
/// agent 操作错误 (控制路径, 见各变体注释).
#[derive(Debug, thiserror::Error)]
pub enum IceError {
    #[error("libnice agent 创建失败 (库缺失?)")]
    NullAgent,
    #[error("add_stream 返回 0")]
    CreateStream,
    #[error("gather_candidates 失败")]
    GatherFailed,
    #[error("对端 SDP 被拒绝")]
    RemoteRejected,
    #[error("等待超时: {0}")]
    Timeout(&'static str),
    #[error("agent 已关闭")]
    Closed,
}
```

- [ ] **Step 4: 跑测试 + 门禁**

Run: `cargo test -p ipcam-gst ice::agent -- --nocapture` → `two_agents_connect_loopback` passed (Windows 开发机 nice-10.dll 在 PATH; 若 DLL 缺失会静默 skip — 此时先确认 PATH 导出). 四门禁.

---

### Task 4: sigserver (media_talk_rust 第二 bin)

**Files:**
- Create: `crates/media_talk_rust/src/bin/sigserver.rs` (含 `#[cfg(test)] mod tests`)
- Modify: `crates/media_talk_rust/Cargo.toml` (dependencies 加 `tokio-tungstenite = { workspace = true }`)

**Interfaces:**
- Produces (Task 6 依赖): WS 协议见"任务接口速览" Task 4. 地址 `ws://<host>:9000`, 消息一律 JSON 文本帧.

- [ ] **Step 1: 写失败测试 (bin 内嵌, 起真实服务 + 两个 tungstenite 客户端)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relays_offer_between_two_peers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        tokio::spawn(async move { serve(listener).await.unwrap() });

        let url = format!("ws://127.0.0.1:{port}");
        let (mut c1, _) = connect_async(&url).await.unwrap();
        let (mut c2, _) = connect_async(&url).await.unwrap();
        c1.send(join("room-a").into()).await.unwrap();
        c2.send(join("room-a").into()).await.unwrap();
        assert_eq!(recv_type(&mut c1).await, "joined");
        assert_eq!(recv_type(&mut c2).await, "joined");

        c1.send(text_msg("offer", "sdp-A").into()).await.unwrap();
        let (ty, payload) = recv_typed(&mut c2).await;
        assert_eq!(ty, "offer");
        assert_eq!(payload, "sdp-A");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_third_peer() {
        // 第三个 join 收到 error; 前两人通道不受影响
    }
}
```

Run: `cargo test -p media_talk_rust --bin sigserver` → FAIL (bin 不存在).

- [ ] **Step 2: 实现 sigserver.rs (~130 行)**

```rust
//! sigserver: ICE 验证用微型 WS 信令转发 (房间制, 两人配对).
//! 生产形态由 kamailio 取代 (spec 产品化路线), 本服务仅测试期用.

use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

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
}

type Rooms = Arc<Mutex<HashMap<String, [Option<mpsc::UnboundedSender<Message>>; 2]>>>;

/// 服务入口: 接受连接, 按房间两两配对转发 offer/answer/hangup.
///
/// # Arguments
///
/// * `listener`: 已绑定的 std listener (测试可绑 0 端口; main 绑 CLI 端口).
///
/// returns: Result<(), std::io::Error>
///
/// # Examples
///
/// ```
///
/// ```
async fn serve(listener: TcpListener) -> std::io::Result<()> {
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));
    loop {
        let (stream, _) = tokio::net::TcpListener::from_std(listener.try_clone()?)?
            .accept()
            .await?;
        let rooms = rooms.clone();
        tokio::spawn(async move {
            let ws = tokio_tungstenite::accept_async(stream).await?;
            handle(ws, rooms).await
        });
    }
}
```

要点: std listener 每次 try_clone + from_std 不可行 (from_std 要求每次一个) — 实现改为 `let l = tokio::net::TcpListener::from_std(listener)?; loop { let (stream, _) = l.accept().await?; ... }` (函数签名为 `async fn serve(l: tokio::net::TcpListener)`; 测试里 std listener 转 once). 单连接处理 `handle`:

```rust
async fn handle(
    mut ws: WebSocketStream<TcpStream>,
    rooms: Rooms,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    // join 帧分配槽位; 之后的帧原样转发给同房另一槽.
    let Some(first) = ws.next().await else { return Ok(()) };
    let frame: Frame = serde_json::from_slice(first?.into_data().as_slice()).unwrap_or(Frame {
        kind: "error".into(), room: String::new(), payload: String::new(),
        detail: "bad json".into(),
    });
    if frame.kind != "join" || frame.room.is_empty() {
        ws.send(Message::text(error("first frame must be join"))).await?;
        return Ok(());
    }
    let (slot, peer_tx) = {
        let mut rooms = rooms.lock();
        let room = rooms.entry(frame.room.clone()).or_default();
        let slot = if room[0].is_none() { 0 } else if room[1].is_none() { 1 } else {
            None
        };
        match slot {
            None => {
                drop(rooms);
                ws.send(Message::text(error("room full"))).await?;
                return Ok(());
            }
            Some(s) => {
                let (tx, rx) = mpsc::unbounded_channel::<Message>();
                let peer = room[1 - s].clone();
                room[s] = Some(tx);
                (s, peer)
            }
        }
    };
    ws.send(Message::text(joined(slot as u8))).await?;

    let (mut ws_tx, mut ws_rx) = ws.split();
    let fwd = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });
    while let Some(msg) = ws_rx.next().await {
        let msg = msg?;
        if msg.is_close() {
            break;
        }
        // 原样转发 JSON 文本帧 (offer/answer/hangup)
        if let Some(peer) = &peer_tx {
            let _ = peer.send(msg);
        }
    }
    drop(fwd);
    rooms.lock().get_mut(/* room */).map(|r| r[slot] = None);
    Ok(())
}
```

清理用 slot 记录 room key. `join/joined/error` 为小的 JSON 构造 helper. main:

```rust
fn main() -> anyhow::Result<()> {
    let bind = std::env::args()
        .position(|a| a == "--bind")
        .and_then(|i| std::env::args().nth(i + 1))
        .unwrap_or_else(|| "0.0.0.0:9000".into());
    tracing_subscriber::fmt().with_env_filter("info").init();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&bind).await?;
        tracing::info!(%bind, "sigserver listening");
        serve(listener).await
    })?;
    Ok(())
}
```

- [ ] **Step 3: 跑测试 + 门禁**

Run: `cargo test -p media_talk_rust --bin sigserver` → passed; 四门禁 (`cargo tree | grep tokio-tungstenite` 先确认已在树, §8).

---

### Task 5: `ipcam-gst/src/ice_media.rs` — nicesink/nicesrc 媒体管线

**Files:**
- Create: `crates/ipcam-gst/src/ice_media.rs`
- Modify: `crates/ipcam-gst/src/lib.rs` (加 `mod ice_media;` + `pub use`)
- Modify: `crates/ipcam-gst/src/rtp_send/chain.rs` (把源支路构造 fn 提为 `pub(crate)`, 逻辑零改动)
- Test: 同文件 `#[cfg(test)] mod tests` (factory-guarded 图结构断言, 沿用 pipeline.rs:168 的 guard 先例)

**Interfaces:**
- Consumes: Task 3 `IceAgent::{raw_agent, state_rx}`, `TrackSource`/`RawTaps`/`AudioOutput` (已有), `make()`/`add_and_sync`/`link_chain`/`leaky_queue` (gstutil).
- Produces (Task 6 依赖): `IceMediaConfig`/`AudioRecv`/`start_ice_media`/`IceMediaHandle` 见"任务接口速览"; `IceMediaHandle::bytes() -> (u64, u64)` 为原子计数器快照 (tx,rx 字节).

- [ ] **Step 1: 先读 `crates/ipcam-gst/src/rtp_send/chain.rs`, 把"源支路构造"提取复用**

把构造视频源支路 (file/camera 解码重编码前段) 与音频源支路的私有 fn 改为 `pub(crate)` (不改任何逻辑与属性), 供 ice_media 调用. 若源支路与 udpsink 尾部耦合在一个 fn 内, 拆为 `xxx_source_branch() -> Vec<Element>` + 尾部两步, chain.rs 原调用点改为拼接.

- [ ] **Step 2: 写失败测试 (图结构, 缺件 guard)**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn has_factory(p: &gst::Pipeline, name: &str) -> bool {
        p.iter().any(|e| e.factory().and_then(|f| f.name()) == Some(name.into()))
    }

    #[test]
    fn builds_media_pipeline_with_nice_elements() {
        crate::ensure_init_internal().unwrap();
        if gst::ElementFactory::find("nicesink").is_none() {
            tracing::warn!("nicesink not available (gst-plugins-bad missing), skipping test");
            return;
        }
        // agent 用最小真实 agent (无 STUN); 仅验证图能建到 Ready 前, 不启动
        let agent = agent_for_test();
        let cfg = IceMediaConfig {
            agent: &agent,
            video_src: None,
            audio_send: true,
            audio_recv: AudioRecv::TapOnly,
            max_width: 640,
            taps: crate::RawTaps::default(),
        };
        let p = build_pipeline(&cfg).unwrap();
        assert!(has_factory(&p, "nicesink"));
        assert!(has_factory(&p, "nicesrc"));
        assert!(has_factory(&p, "rtph264depay") || !cfg_has_video(&cfg));
    }
}
```

`agent_for_test()`: `IceAgent::new(IceConfig { stun: None, controlling: true })`, 失败则 `return` (同 guard 惯例).

Run: `cargo test -p ipcam-gst ice_media` → FAIL.

- [ ] **Step 3: 实现 build_pipeline (关键: agent 指针塞 gst 属性)**

```rust
//! ICE 通话媒体管线: 复用对讲链路编码配置, 收发端点从 udpsink/udpsrc 换成
//! nicesink/nicesrc (共享 IceAgent 的 NiceAgent, component 1=video 2=audio).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gstreamer::glib;
use gstreamer::prelude::*;

use crate::ice::agent::IceAgent;
use crate::rtp_send::TrackSource;
use crate::tap::RawTaps;
use crate::{AudioOutput, GstStreamError, gstutil::{add_and_sync, leaky_queue, link_chain, make}};

/// 音频接收方式: 板子扬声器 / 仅 tap (开发机无 ALSA).
#[derive(Debug, Clone)]
pub enum AudioRecv {
    Disabled,
    Alsa(String),
    TapOnly,
}

/// ICE 媒体会话配置.
pub struct IceMediaConfig<'a> {
    /// 已连通 (或连通中) 的 ICE agent.
    pub agent: &'a IceAgent,
    /// 发送视频源; None = 只收不放视频 (调试时常见).
    pub video_src: Option<TrackSource>,
    /// 是否发送音频.
    pub audio_send: bool,
    /// 音频接收方式.
    pub audio_recv: AudioRecv,
    /// 发送限宽.
    pub max_width: u32,
    /// 收端帧回调 (video=RGBA 帧, audio=S16LE 块), 与 ingest tap 同语义.
    pub taps: RawTaps,
}

/// 存活中的 ICE 媒体会话.
pub struct IceMediaHandle {
    pipeline: gst::Pipeline,
    bytes: Arc<AtomicBytes>,
}

#[derive(Default)]
struct AtomicBytes {
    tx: AtomicU64,
    rx: AtomicU64,
}

impl IceMediaHandle {
    /// 停止会话 (pipeline 置 Null; agent 由调用方之后关闭, 顺序见模块注释).
    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    /// (发送字节, 接收字节) 快照, 供 UI 状态行.
    pub fn bytes(&self) -> (u64, u64) {
        (self.bytes.tx.load(Ordering::Relaxed), self.bytes.rx.load(Ordering::Relaxed))
    }
}

/// 构建并启动 ICE 媒体管线.
///
/// # Arguments
///
/// * `cfg`: 媒体配置; `cfg.agent` 必须已 gathering 完成并喂入对端 blob.
///
/// returns: Result<IceMediaHandle, GstStreamError>
///
/// # Examples
///
/// ```
///
/// ```
pub fn start_ice_media(cfg: &IceMediaConfig<'_>) -> Result<IceMediaHandle, GstStreamError> {
    let pipeline = gst::Pipeline::new();
    let bytes = Arc::new(AtomicBytes::default());

    // 接收: nicesrc(component 1) → depay → parse → avdec → videoconvert → appsink
    let vrx_src = make_nice_element("nicesrc", cfg, 1)?;
    let vrx_chain = crate::ingest::video_chain_names(crate::VideoCodec::H264) ... // 复用 ingest 的 depay/parse/decode 链名表
    // 注: 若 ingest 的链名表非 pub, 就地写 ["rtph264depay","h264parse","avdec_h264","videoconvert"].

    // 发送: 源支路 (rtp_send 复用) → x264enc(与 rtp_send/chain.rs 同属性) →
    //       rtph264pay(pt=96) → nicesink(component 1)
    // 音频收: nicesrc(component 2) → rtpopusdepay → opusdec → audioconvert →
    //         [alsasink | appsink S16LE tap]
    // 音频发: 源支路 (复用) → audioconvert/audioresample → capsfilter(48000,2)
    //         → opusenc → rtpopuspay(pt=111) → nicesink(component 2)

    add_and_sync(&pipeline, &elements[..])?;
    link_chain(&elements[..])?;
    install_byte_probes(&pipeline, &bytes)?; // nicesink sink pad (tx) / nicesrc src pad (rx)
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| GstStreamError::Stream(format!("ice media play: {e}")))?;
    Ok(IceMediaHandle { pipeline, bytes })
}

/// 建 nicesink/nicesrc 并注入共享 NiceAgent 指针.
fn make_nice_element(name: &str, cfg: &IceMediaConfig<'_>, component: u32) -> Result<gst::Element, GstStreamError> {
    let e = make(name)?;
    let v = ptr_value(cfg.agent.raw_agent());
    e.set_property("agent", &v);
    e.set_property("stream", 1u32); // agent 只有一个 stream
    e.set_property("component", component);
    Ok(e)
}

/// 把裸指针包成 G_TYPE_POINTER 的 glib::Value.
fn ptr_value(p: *mut std::ffi::c_void) -> glib::Value {
    let mut v = glib::Value::from_type(glib::Type::POINTER);
    // SAFETY: v 刚按 POINTER 类型初始化, set_pointer/get_pointer 类型匹配;
    // 指针本身的所有权仍归 IceAgent (元件内部 add ref, 模块注释已述顺序纪律).
    unsafe {
        glib::gobject_ffi::g_value_set_pointer(v.to_glib_none_mut().0, p);
    }
    v
}
```

- [ ] **Step 4: 补充元素 (属性照抄现有成熟配置)**

- 视频发送编码器 (与 `rtp_send/chain.rs` 逐属性一致, 去掉 doorphone 专用 slice-max-size 约束 — 两端都是本程序, FU-A 正常):

```rust
let enc = make("x264enc")?;
enc.set_property("tune", "zerolatency");
enc.set_property("speed-preset", "veryfast");
enc.set_property("bitrate", 400u32);
enc.set_property("key-int-max", 30u32);
enc.set_property("aud", false);
let pay = make("rtph264pay")?;
pay.set_property("pt", 96u32);
pay.set_property("config-interval", 1u32);
```

- 音频发送: `audioconvert/audioresample → capsfilter(audio/x-raw, rate=48000, channels=2) → opusenc → rtpopuspay(pt=111)`. capsfilter 用 `caps: gst::Caps::from_str("audio/x-raw, rate=48000, channels=2")`.
- 视频接收 appsink (照搬 `ingest/track.rs:310` 的 `build_video_appsink` 配置: `video/x-raw,format=RGBA`, max-buffers=2, drop, sync=false), tap 回调直接复用 `cfg.taps.video`.
- 音频接收: `rtpopusdepay → opusdec → audioconvert → audioresample → tee`; tee 两支: alsasink(device) (AudioRecv::Alsa) 与/或 S16LE appsink (max-buffers=8, 同 `build_audio_appsink`).
- byte probe: 在 nicesink 的 sink pad 加 BUFFER probe `bytes.tx += buffer.size()`, nicesrc src pad 同 (guard: 若某分支未建则跳过).

- [ ] **Step 5: 跑测试 + 门禁**

Run: `cargo test -p ipcam-gst ice_media` → passed (factory 齐时; 缺件 skip); 四门禁.

---

### Task 6: call-ui `src/ice.rs` — 控制器 + WS 信令 + 手动通道

**Files:**
- Create: `crates/call-ui/src/ice.rs`
- Modify: `crates/call-ui/src/lib.rs` (加 `mod ice;` + spawn 接线)
- Modify: `crates/call-ui/Cargo.toml` (加 `tokio-tungstenite = { workspace = true }`)
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `ipcam_gst::ice::agent::*`, `ipcam_gst::ice::blob::IceBlob`, `ipcam_gst::ice_media::*`, sigserver 协议 (Task 4), `bridge::UiHandle`, `store::Store`.
- Produces (Task 7 依赖): `pub(crate) fn wire_ice(ui: &MainWindow, store: &Rc<Store>, handle: &UiHandle)`; `UiEvent::IceStatus { state: String, tx_kb: u64, rx_kb: u64 }` (Task 7 在 app.rs 消费); kv keys `ice_sigserver` / `ice_room` / `ice_stun`; `pub(crate) struct IceController` (构造时收 `UiHandle`, 方法 `call(&self) / hangup(&self) / local_blob_text() -> Result<String, String> / apply_remote_blob(&self, text: &str) -> Result<(), String>`, 内部状态 `Arc<Mutex<IceInner>>`).

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sigserver_frame() {
        let f: Frame = serde_json::from_str(r#"{"type":"offer","payload":"v=0\r\n"}"#).unwrap();
        assert_eq!(f.kind, "offer");
        assert_eq!(f.payload, "v=0\r\n");
    }

    #[test]
    fn decides_offerer_by_slot() {
        assert!(is_offerer(0) && !is_offerer(1));
    }
}
```

Run: FAIL.

- [ ] **Step 2: 实现 Frame/通道**

`Frame` 与 sigserver 同构 (serde). WS 通道: `async fn ws_exchange(sigserver: &str, room: &str, local: IceBlob) -> Result<IceBlob, IceError>` — connect → join → 等 joined(拿 slot) → `is_offerer(slot)`: offerer 发 offer 再等 answer; 非 offerer 等 offer → add_remote → 发 answer (本地 blob). 返回对端 blob.

- [ ] **Step 3: 实现 IceController (线程模型照 weather::spawn: std::thread + 自建 current-thread tokio runtime)**

状态机 `Idle → Gathering → Exchanging → Connecting → Active → Idle`:

```rust
pub(crate) fn spawn(handle: UiHandle, store_path: /* Store::open 每线程自建 */) -> (IceController, std::thread::JoinHandle<()>) {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<IceCmd>();
    let inner = Arc::new(Mutex::new(IceInner::Idle));
    let ctl = IceController { inner: inner.clone(), cmd_tx };
    let join = std::thread::Builder::new().name("ice-ctl".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(run(cmd_rx, handle, inner));
    }).unwrap();
    (ctl, join)
}
```

`run` 循环: `cmd_rx.recv()` → `Call` 分支:

```rust
// 1. 读 kv (Store::open(), 同 weather.rs:202 惯例): sigserver/room/stun
// 2. IceAgent::new(IceConfig { stun, controlling: is_offerer(?) }) — 先建 agent 再 join 拿 slot:
//    controlling 需在 new 前定, 但 slot 在 join 后知.
//    解: join 先拿 slot (WS 连接在 agent 创建前), 再按 slot 建 agent.
// 3. local_blob + ws_exchange 并发 (answerer 先等 offer 再产出 blob — 见 Task 4 协议:
//    offerer 发本地 sdp; answerer 收 offer 后本地 gather 应已完成, 回发本地 sdp)
// 4. add_remote → 等 state_rx == Connected (10s 超时) → post(IceStatus "已连接")
// 5. start_ice_media(...) → Active; 每秒 post(IceStatus + bytes)
// Hangup → stop media → drop agent → Idle + post(IceStatus "空闲")
// 任何一步错 → post(UiEvent::IceStatus state=错误文本) + 回 Idle
```

手动通道: `ManualBlob(cmd)` 经同一 cmd_tx 进状态机: `ExportBlob` → 若当前在 Gathering/Exchanging 且有本地 blob 则返回文本 (oneshot); `ImportBlob(text)` → add_remote. v1 手动模式先把"自动 WS"关掉: UI 加 `ice_manual: bool` kv + 复选框, true 时 run 在 local_blob 后停下等 ImportBlob (用 std::sync::mpsc 同步等待, 同 wake 模式), 并缓存本地 blob 供导出.

`UiEvent::IceStatus` 在 `src/app.rs:44-79` 加变体 (Task 7 做 UI 时一并接线, 本任务只发不改 app.rs? 不行 — 编译要过. 本任务在 app.rs 加最小变体 + AppState 字段 + on_event arm (Weather 模式: 更新字段返回 None), sync_props 接线留给 Task 7 — 不行, sync_props 不读新字段会 clippy dead_code? 字段写不读 → clipy 不查私有字段死读 (warn 的是从不读字段? rustc dead_code 对 never-read 字段报警告, RUSTFLAGS -D warnings 会炸!). 所以 Task 6 必须把同步做全: app.rs 变体 + AppState.ice_status 字段 + on_event arm + props.rs sync_props 里 set 到新 slint 属性 — 但 slint 属性在 Task 7 才加. 结论: Task 6 与 Task 7 的 app.rs/props.rs/main.slint 改动耦合, 合并执行顺序: Task 6 先把 UiEvent/字段/sync_props/slint 属性全部做掉 (UI 卡片本体留 Task 7). 允许 Task 6 先加 `in property <string> ice-status-text` 等 MainWindow 属性与同步代码, Task 7 只做卡片可视化.
```

- [ ] **Step 4: 跑测试 + 门禁**

Run: `cargo test -p call-ui ice` → passed; 四门禁 (含 `cargo clippy -p call-ui --all-targets -- -D warnings`).

---

### Task 7: UI — 设置页 ICE 卡片

**Files:**
- Create: `crates/call-ui/ui/settings/ice_card.slint`
- Modify: `crates/call-ui/ui/settings_page.slint` (import + 实例 + 属性/callback 转发)
- Modify: `crates/call-ui/ui/main.slint` (MainWindow 属性 + 转发)
- Modify: `crates/call-ui/src/ice.rs` (`wire_ice` 装配回调)
- Modify: `crates/call-ui/src/wiring.rs` (在 run 装配链调用 `ice::wire_ice`)
- Modify: `crates/call-ui/src/lib.rs` (startup kv 播种 + spawn)

**Interfaces:**
- Consumes: Task 6 的 `IceController`/`wire_ice`/kv keys/`UiEvent::IceStatus`.
- Produces: 用户可见功能 (无可被代码依赖的接口).

- [ ] **Step 1: ice_card.slint (结构照 `general_card.slint`)**

```slint
import { FilledCard, KbTextField, AppButton } from "../components.slint"; // 名称以实际导出为准

export component IceSettingsCard inherits FilledCard {
    callback set-sigserver(string);
    callback set-room(string);
    callback set-stun(string);
    callback ice-call();
    callback ice-hangup();
    callback ice-export-blob();
    callback ice-import-apply(string);
    callback kb-field-focused(int);
    in-out property <string> sigserver: "";
    in-out property <string> room: "";
    in-out property <string> stun: "";
    in property <string> status-text: "";
    in property <string> stats-text: "";
    in property <bool> calling: false;
    in property <string> blob-dialog-text: "";

    // 布局: 三行 KbTextField (field-id 9/10/11, 占位提示 "ws://..." / "房间名" / "stun://..."),
    // 按钮行 [呼叫/挂断], 状态行 status-text + stats-text,
    // 手动模式按钮 [导出本地 blob]/[粘贴对端 blob(弹框 TextEdit + 应用)]
}
```

slint 语法细节 (VerticalLayout/spacing/按钮) 照抄 `general_card.slint` 现有片段; 弹框用 `CameraDialogs` 同款 overlay 模式 (`ice_dialogs.slint` 或内嵌 `Rectangle`), TextEdit 用 slint 内置 `TextEdit`.

注意: material TextField placeholder 不渲染, 提示走 supporting-text; 但 KbTextField 已封装 — 看其导出接口照用.

- [ ] **Step 2: 属性与回调穿透 settings_page.slint → main.slint (照 city/weather-key 先例)**

`settings_page.slint`: import + `IceSettingsCard { ... }` 实例 + `in-out`/`callback` 声明转发 (照 :62-63); `main.slint`: MainWindow 根属性 + `SettingsPage` 实例绑定 (照 :255-256).

- [ ] **Step 3: wire_ice 装配 (src/ice.rs)**

```rust
pub(crate) fn wire_ice(
    ui: &MainWindow,
    store: &Rc<Store>,
    ctl: &IceController,
) {
    // kv 播种 (lib.rs startup 读 kv → ui.set_ice_sigserver(...) 等, 照 weather)
    // on_set_sigserver/room/stun: store.kv_set + toast("已保存, 下次呼叫生效")
    // on_ice_call: ctl.call()
    // on_ice_hangup: ctl.hangup()
    // on_ice_export_blob: ctl.local_blob_text() → ui.set_ice_blob_dialog_text + 开弹框
    // on_ice_import_apply(text): ctl.apply_remote_blob(text) + 关弹框
}
```

- [ ] **Step 4: UiEvent::IceStatus 全链 (若 Task 6 已做则跳过)**

`app.rs`: `UiEvent::IceStatus { state: String, tx_kb: u64, rx_kb: u64 }` + `AppState.ice_status: (String, u64, u64)` + on_event arm 更新字段返回 None (Weather 模式). `props.rs::sync_props`: `ui.set_ice_status_text(format!("{}  发送 {} KB / 接收 {} KB", ...))` 或 status/stats 两行分开.

- [ ] **Step 5: 构建 + 手工冒烟**

Run: `cargo check -p call-ui` → 通过; `cargo fmt/clippy -p call-ui` → 通过.
冒烟 (手工, 记录结果进 commit message): `cargo run --bin sigserver` + 两个 `media_talk_rust gui` 实例 (或一实例 + 板子) 设置页点呼叫 → 状态行 "已连接" + stats 增长. Windows 双实例可行 (两进程).

---

### Task 8: CI / 交叉构建 / 文档

**Files:**
- Modify: `.github/workflows/rust.yml` (3 处 apt 列表加 `libnice-dev`)
- Modify: `Dockerfile.rk3566` (apt 列表加 `libnice-dev`)
- Modify: `docs/cross-build.md` (板端运行依赖加 gstreamer1.0-nice; librime 节旁边加 libnice 节)
- Modify: `media_talk_rust.service` (注释加一行板端 gst-inspect 验证提示, 可选)

**Interfaces:** 无代码接口.

- [ ] **Step 1: rust.yml** — 3 个 apt step 的包列表 (line 30/45/58 一带) 末尾加 `libnice-dev`, 注释 "(ALSA + GStreamer + librime + libnice)".

- [ ] **Step 2: Dockerfile.rk3566** — apt install 块加 `libnice-dev` (librime-dev 同行).

- [ ] **Step 3: cross-build.md** — 板端运行依赖清单加 `gstreamer1.0-nice` (不带 -dev); 新增小节: "ICE 通话验证 (libnice): 构建期 libnice-dev; 运行期 gstreamer1.0-nice; 部署后先 `gst-inspect-1.0 nicesink` 确认插件存在 (泰山派镜像可能裁剪 gst 插件)".

- [ ] **Step 4: 本地门禁** — 四门禁全跑 (CI 改动无法本地验证, 靠审查).

---

### Task 9: 收尾 — openspec archive + 终检

**Files:**
- Modify: `openspec/changes/add-ice-calling/*` (archive 移交)
- Create: `openspec/changes/archive/2026-11-02-add-ice-calling/` (git mv)
- Modify: `openspec/specs/ice-calling/spec.md` (基线沉淀)

- [ ] **Step 1: 核对 spec 偏差回填** — 设计文档 `docs/superpowers/specs/2026-10-30-ice-calling-design.md` 的 blob 格式节加一行偏差说明 (实际采用 libnice 原生 SDP), 与 Task 2 决策一致.

- [ ] **Step 2: archive**

```bash
mkdir -p openspec/changes/archive
git mv openspec/changes/add-ice-calling openspec/changes/archive/2026-11-02-add-ice-calling
# specs/ice-calling/spec.md 同步进 openspec/specs/ice-calling/ (按 archive 先例结构)
```

- [ ] **Step 3: 终检清单 (CLAUDE.md §10)** — 逐项过并贴证据:
  - `cargo fmt --all` 最后一行
  - `cargo clippy --workspace --all-targets --features sw-decode -- -D warnings` 最后一行
  - `cargo test --workspace --features sw-decode` 最后一行 (baseline 53 + 新增数)
  - `git status` 干净 (probe 临时目录 `target/tmp/librice-probe` 在 target/ 下, 已被 gitignore; 确认无 `??` 垃圾)
  - 提交消息符合 §6; OpenSpec 已归档

- [ ] **Step 4: 汇报** — 贴三门禁输出 + 冒烟结果 (Task 7 Step 5), 等用户说"提交"再 commit (§6/§8: OpenSpec archive 已完成, 允许提交).

---

## Self-Review 记录

1. **Spec 覆盖**: spec 各节 → Task: 总体架构(1/3/5), 双通道信令(4/6/7), 媒体通路(5), UI(7), sigserver(4), 平台依赖(8), 测试计划(Task 3 环回 = spec"LAN 自动"前置; Task 4 中继测试; Task 5 图测试), 风险表(GC 线程纪律→Task 3 invoke 模式+注释; 明文→Global Constraints 说明; 资源回收→Task 6 hangup 顺序). blob 格式偏差已在 Task 2 与 Task 9 Step 1 双向记录.
2. **占位符扫描**: Task 5 Step 3 的 ingest 链名表引用留了实现指引 (ingest 现有代码, 非占位); Task 6 Step 3 是伪代码形态但每步操作明确 — 均为"按现有代码模式落笔"类指引, 实现者手头有探索报告 (agent-79/80/81 的结论已并入本文档 Interfaces/属性值).
3. **类型一致性**: `IceAgent` 方法名 (local_blob/add_remote/state_rx/raw_agent/send_test/attach_test_recv) 在 Task 3/5/6 间一致; `IceBlob::as_str` 用于 Task 4 协议 payload 与 Task 6; Frame JSON 字段 (type/room/payload/detail) 在 sigserver 与 call-ui 一致; component 语义 (1=video, 2=audio) 在 Task 3 (add_stream 2) 与 Task 5 (make_nice_element component) 一致.
