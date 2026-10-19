# 监控页网络摄像头管理 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 call-ui GUI 中加入网络摄像头管理: 设置页负责搜索/手动添加/删除 (SQLite 持久化), 监控页负责查看 (可折叠侧边栏切换, 复用对讲视频显示通路)。

**Architecture:** ingest (RTSP 拉流) 新增"本地查看"模式 (不建 webrtcsink 支路) + tap 限宽缩放, 监控页经同一个 `VideoFrameSink`/`video-frame` 属性上屏; 摄像头元数据落 `Store` 同库新表 `cameras`; ONVIF 搜索用 ipcam-discovery 在独立线程跑, 结果经 `invoke_from_event_loop` 回 UI。

**Tech Stack:** Rust workspace, GStreamer (rtspsrc/webrtcsink/appsink), Slint 1.18 (Material 组件集), rusqlite, tokio, oxvif (ipcam-discovery)。

设计文档: `docs/superpowers/specs/2026-10-19-camera-monitor-design.md`

## Global Constraints

- 所有 cargo 命令必须带 GStreamer 环境前缀:
  `export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH" && export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"`
- 每个 Task 收尾门禁: `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --features sw-decode -- -D warnings`、`cargo clippy -p call-ui --all-targets -- -D warnings` (call-ui 无 sw-decode feature)、`cargo test --workspace --features sw-decode`
- 提交格式 `[类型] 模块: 标题` (类型: 新增/修复/重构/优化/文档); **只 commit 不 push**
- 库代码禁 println/unwrap/expect (测试除外); 日志用 tracing; 注释用中文; 导出函数注释按 CLAUDE.md §2 模板 (功能描述 + # Arguments 列表项 + returns + # Examples)
- slint 界面文案必须包 `@tr("...")` (i18n), 后端只发数据不预格式化
- 监控/对讲共用 `video-frame` 属性: 同一时刻只有一路在生产帧 (phase==0 时监控, phase!=0 时对讲), 不得并发启动两路 tap

---

### Task 1: ingest 本地查看模式 (webrtc 开关)

ingest 管线现在必建 webrtcsink 转推支路。加 `GstStreamConfig.webrtc: bool` (默认 true, 现有三个构造点全用 `..Default::default()` 零改动): false 时不创建 webrtcsink, 视频轨只剩 tap 消费, 音频轨无消费者时直接忽略。监控页 (Task 6) 依赖此模式。

**Files:**
- Modify: `crates/ipcam-gst/src/ingest/config.rs` (加字段 + Default)
- Modify: `crates/ipcam-gst/src/ingest/pipeline.rs:51-97` (build_pipeline 条件创建 webrtcsink)
- Modify: `crates/ipcam-gst/src/ingest/track.rs` (install_pad_added/link_video/link_audio 的 ws 参数改 Option)
- Test: `crates/ipcam-gst/src/ingest/pipeline.rs` (mod tests, 新增)

**Interfaces:**
- Consumes: 现有 `GstStreamConfig`/`build_pipeline`/`install_pad_added` 结构
- Produces:
  - `GstStreamConfig { pub webrtc: bool, .. }` — `true` = 建 webrtcsink 转推 (现状), `false` = 仅本地 tap
  - `install_pad_added(src, pipeline, tracks, audio_output, ws: Option<gst::Element>, handle, taps, tap_max_width)` — tap_max_width 在 Task 2 加, 本任务签名先不含它
  - `link_video(..., ws: Option<&gst::Element>, ...)` / `link_audio(..., ws: Option<&gst::Element>, ...)`

- [ ] **Step 1: 写失败测试**

在 `crates/ipcam-gst/src/ingest/pipeline.rs` 文件末尾新增测试模块:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::config::GstStreamConfig;
    use crate::ingest::stats::GstStreamHandle;
    use crate::ingest::tap::RawTaps;

    fn has_factory(p: &gst::Pipeline, name: &str) -> bool {
        p.iterate_elements()
            .into_iter()
            .any(|e| e.factory().map(|f| f.name() == name).unwrap_or(false))
    }

    /// 本地查看模式: webrtc=false 时管线只建 rtspsrc, 不建 webrtcsink;
    /// true (默认) 保持现状两个都有
    #[test]
    fn local_mode_skips_webrtcsink() {
        crate::ensure_init_internal().unwrap();
        let local = GstStreamConfig {
            webrtc: false,
            ..Default::default()
        };
        let (p, _t) = build_pipeline(&local, GstStreamHandle::new(), RawTaps::default())
            .expect("local pipeline builds");
        assert!(has_factory(&p, "rtspsrc"));
        assert!(!has_factory(&p, "webrtcsink"));

        let (p2, _t2) = build_pipeline(
            &GstStreamConfig::default(),
            GstStreamHandle::new(),
            RawTaps::default(),
        )
        .expect("webrtc pipeline builds");
        assert!(has_factory(&p2, "webrtcsink"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p ipcam-gst --lib local_mode_skips_webrtcsink`
Expected: FAIL — `GstStreamConfig` has no field `webrtc` (编译错误)

- [ ] **Step 3: config.rs 加字段**

`GstStreamConfig` 结构体 `signalling_port` 字段后加:

```rust
    /// 是否创建 webrtcsink 转推支路. true = RTSP → WebRTC 转推 (web 端
    /// 查看, 现状); false = 仅本地 tap (GUI 监控页), 不建 webrtcsink,
    /// 不需要信令服务器. false 且 RawTaps.video 为 None 时视频轨无消费者,
    /// link_video 会报 Link 错误
    pub webrtc: bool,
```

`Default` impl 里加 `webrtc: true,` (现有构造点全部 `..Default::default()`, 行为不变)。

- [ ] **Step 4: pipeline.rs 条件创建 webrtcsink**

`build_pipeline` 中 `let ws = make("webrtcsink")?;` 到 signaller 设置一段替换为:

```rust
    // webrtc=false = 仅本地 tap 模式 (GUI 监控页): 不建 webrtcsink,
    // 音视频轨只剩 tap/alsa 消费者 (见 track.rs link_video/link_audio)
    let ws = if cfg.webrtc {
        let ws = make("webrtcsink")?;
        // Producer identity on the signalling channel; consumers match on
        // `meta.name`. Keep it unique per session.
        ws.set_property_from_str("meta", &format!("meta,name={}", cfg.stream_name));
        // webrtcsink 连到进程内共享的信令服务器（ensure_signalling_server
        // 起在 8443）——注意不是 signalling-server-host/port，那两个属性
        // 是给"自己跑服务器"模式用的，连别人的服务器要走 signaller 的 uri
        let signaller = ws.property::<gst::glib::Object>("signaller");
        signaller.set_property(
            "uri",
            format!("ws://{}:{}", cfg.signalling_host, cfg.signalling_port),
        );
        Some(ws)
    } else {
        None
    };

    match &ws {
        Some(ws) => pipeline
            .add_many([&src, ws])
            .map_err(|e| GstStreamError::Init(format!("pipeline add rtspsrc/webrtcsink: {e}")))?,
        None => pipeline
            .add(&src)
            .map_err(|e| GstStreamError::Init(format!("pipeline add rtspsrc: {e}")))?,
    }
```

`install_pad_added` 调用的 `ws` 实参改为 `ws` (已是 Option)。

- [ ] **Step 5: track.rs 的 ws 参数改 Option**

`install_pad_added` 签名 `ws: gst::Element` 改 `ws: Option<gst::Element>`; 闭包内 `link_video(&pipeline, pad, s, &tracks, &ws, ...)` 传 `ws.as_ref()`, `link_audio` 同理。

`link_video` 签名 `ws: &gst::Element` 改 `ws: Option<&gst::Element>`, 函数体开头加消费者检查, 链接段改为:

```rust
    // 至少一个消费者: webrtcsink 支路 (webrtc 模式) 或 tap 支路;
    // 两个都没有这路轨纯属浪费带宽, 直接报错
    if ws.is_none() && tap.is_none() {
        return Err(GstStreamError::Link(
            "video track has no consumer (webrtc disabled and no tap)".into(),
        ));
    }
```

原 `match &tap_branch { Some(b) => ..., None => ... }` 段替换为:

```rust
    let parse_src = static_pad(&parse, "src")?;
    match (&tap_branch, ws) {
        (Some(b), _) => {
            parse_src
                .link(&static_pad(&b.tee, "sink")?)
                .map_err(|e| GstStreamError::Link(format!("failed to link parse to tee: {e}")))?;
            if let Some(ws) = ws {
                let ws_pad = request_ws_pad(ws, "video_%u")?;
                link_tee_to_pad(&b.tee, &ws_pad)?;
            }
            link_tee_branch(&b.tee, &b.gui_chain[0])?;
            let gui_refs: Vec<&gst::Element> = b.gui_chain.iter().collect();
            link_chain(&gui_refs, "video tap branch")?;
        }
        (None, Some(ws)) => {
            let ws_pad = request_ws_pad(ws, "video_%u")?;
            parse_src.link(&ws_pad).map_err(|e| {
                GstStreamError::Link(format!("failed to link parse to webrtcsink: {e}"))
            })?;
        }
        (None, None) => unreachable!("consumer check above rejects this"),
    }
```

注意原代码里 `let ws_pad = request_ws_pad(ws, "video_%u")?;` 在 match 前, 要删掉 (挪进分支内)。

`link_audio` 签名 `ws: &gst::Element` 改 `ws: Option<&gst::Element>`, 开头加:

```rust
    // 本地模式 + 无音频 tap + 不开喇叭 = 音频轨没有消费者, 标记占用后
    // 直接忽略 (监控页不需要摄像头声音)
    if ws.is_none() && matches!(audio_output, AudioOutput::Disabled) && tap.is_none() {
        claim_track(tracks, true, encoding)?;
        info!(encoding = encoding, "audio track ignored (no consumer in local mode)");
        return Ok(());
    }
```

web 支路构建改为条件:

```rust
    let web = match ws {
        Some(_) => Some(build_web_audio_elements()?),
        None => None,
    };
```

后续 `all.extend(web.iter())` 改 `if let Some(web) = &web { all.extend(web.iter()); }`; 链接段:

```rust
    if let Some(web) = &web {
        let web_refs: Vec<&gst::Element> = web.iter().collect();
        link_chain(&web_refs, "web audio branch")?;
        let Some(ws) = ws else {
            // web 支路只在 ws=Some 时构建, 这里不可达; 库代码禁 expect,
            // 用防御性返回代替
            return Err(GstStreamError::Link("web branch without webrtcsink".into()));
        };
        let ws_pad = request_ws_pad(ws, "audio_%u")?;
        static_pad(web.last().expect("branch is non-empty"), "src")?
            .link(&ws_pad)
            .map_err(|e| GstStreamError::Link(format!("failed to link opusenc to webrtcsink: {e}")))?;
        link_tee_branch(&tee, &web[0])?;
    }
```

(`request_ws_pad` 前已用 if-let 防御, 符合"库代码禁 expect"约束。)

- [ ] **Step 6: 跑测试确认通过 + 门禁**

Run: `cargo test -p ipcam-gst --lib` → PASS (含新测试)
Run: 门禁四连 (见 Global Constraints)

- [ ] **Step 7: Commit**

```bash
git add crates/ipcam-gst/src/ingest/
git commit -m "[新增] ipcam-gst: ingest 支持本地查看模式 (webrtc 开关, 不建 webrtcsink 支路)"
```

---

### Task 2: ingest tap_max_width 移植 + 删 aarch64 挡板

把 rtp_recv 的 tap 限宽缩放 (commit c764e33: videoscale + capsfilter PAR=1/1 + 限宽) 移植到 ingest tap 分支; 顺手删掉 `track.rs` 里 `cfg!(aarch64)` 直接报错的调试挡板 (监控页要在 rk3566 上跑 tap, 挡板是 blocker; 删除后板上走 avdec 软解, 配合限宽带宽可控)。

**Files:**
- Modify: `crates/ipcam-gst/src/ingest/config.rs` (加 `tap_max_width` 字段 + Default)
- Modify: `crates/ipcam-gst/src/ingest/pipeline.rs` (build_pipeline 透传)
- Modify: `crates/ipcam-gst/src/ingest/track.rs:177-259` (install_pad_added/link_video/build_video_tap_branch 透传 + 缩放 + 删挡板)
- Test: `crates/ipcam-gst/src/ingest/track.rs` (mod tests, 新增)

**Interfaces:**
- Consumes: Task 1 的 `install_pad_added`/`link_video`/`build_video_tap_branch`
- Produces:
  - `GstStreamConfig { pub tap_max_width: Option<u32>, .. }` — None = 不缩放 (现状)
  - `build_video_tap_branch(decoder_name: &str, sink: VideoFrameSink, max_width: Option<u32>) -> Result<VideoTapBranch, GstStreamError>`

- [ ] **Step 1: 写失败测试**

`crates/ipcam-gst/src/ingest/track.rs` 末尾新增:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_sink() -> VideoFrameSink {
        std::sync::Arc::new(parking_lot::Mutex::new(|_f: RawVideoFrame| {}))
    }

    fn chain_factories(chain: &[gst::Element]) -> Vec<String> {
        chain
            .iter()
            .map(|e| e.factory().map(|f| f.name().to_string()).unwrap_or_default())
            .collect()
    }

    /// 限宽时 tap 链插 videoscale + capsfilter (PAR=1/1 锁方像素,
    /// 否则 videoscale 只压宽度画面变形 — 见 docs/pitfalls.md);
    /// 不限宽保持原样 (queue/decoder/conv/appsink 四件)
    #[test]
    fn tap_branch_scales_only_when_capped() {
        crate::ensure_init_internal().unwrap();
        let capped = build_video_tap_branch("avdec_h264", dummy_sink(), Some(640))
            .expect("capped branch builds");
        let names = chain_factories(&capped.gui_chain);
        assert!(names.contains(&"videoscale".to_string()), "{names:?}");
        assert!(names.contains(&"capsfilter".to_string()), "{names:?}");

        let plain = build_video_tap_branch("avdec_h264", dummy_sink(), None)
            .expect("plain branch builds");
        assert_eq!(plain.gui_chain.len(), 4);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p ipcam-gst --lib tap_branch_scales_only_when_capped`
Expected: FAIL — `build_video_tap_branch` takes 2 arguments (编译错误)

- [ ] **Step 3: config.rs 加字段**

`webrtc` 字段后加:

```rust
    /// tap 分支显示限宽: Some(w) 时解码后 videoscale 降采样到 w 以内
    /// (保持宽高比, capsfilter 锁 PAR=1/1) 再吐回调; None = 不缩放.
    /// 1080p 摄像头全尺寸 RGBA @30fps ≈ 165MB/s, ARM 板上必须限
    pub tap_max_width: Option<u32>,
```

`Default` 加 `tap_max_width: None,`。

- [ ] **Step 4: track.rs 改造**

删 `build_video_tap_branch` 里的 aarch64 挡板 (track.rs:250-252 的 `if cfg!(all(target_os = "linux", target_arch = "aarch64")) { return Err(...) }` 三行)。

`build_video_tap_branch` 改为:

```rust
fn build_video_tap_branch(
    decoder_name: &str,
    sink: VideoFrameSink,
    max_width: Option<u32>,
) -> Result<VideoTapBranch, GstStreamError> {
    let tee = make("tee")?;
    let gui_queue = leaky_queue()?;
    let decoder = make(decoder_name)?;
    let conv = make("videoconvert")?;
    let mut gui_chain = vec![gui_queue, decoder, conv];
    if let Some(w) = max_width {
        // 显示限宽: 摄像头常发 1080p, 全尺寸 RGBA 帧的 memcpy/上传带宽
        // 在 ARM 板上扛不住. 限宽必须同时锁 PAR=1/1: 只限宽 videoscale
        // 会只压宽度 (320x240 → 160x240 变形), 同 rtp_recv 踩坑记录
        gui_chain.push(make("videoscale")?);
        let caps = make("capsfilter")?;
        caps.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .field("width", gst::IntRange::new(1i32, w as i32))
                .build(),
        );
        gui_chain.push(caps);
    }
    gui_chain.push(build_video_appsink(sink));
    Ok(VideoTapBranch {
        tee,
        gui_chain,
    })
}
```

透传链: `install_pad_added` 加参数 `tap_max_width: Option<u32>` 并传给 `link_video`; `link_video` 加同名参数, `build_video_tap_branch(decoder_name, sink.clone(), tap_max_width)`; `pipeline.rs` 的 `install_pad_added` 调用加实参 `cfg.tap_max_width`。

- [ ] **Step 5: 跑测试确认通过 + 门禁**

Run: `cargo test -p ipcam-gst --lib` → PASS
Run: 门禁四连

- [ ] **Step 6: Commit**

```bash
git add crates/ipcam-gst/src/ingest/
git commit -m "[优化] ipcam-gst: ingest tap 分支限宽缩放 (tap_max_width), 删除 aarch64 调试挡板"
```

---

### Task 3: Store cameras 表 CRUD

照 contacts 模式同库加 `cameras` 表。Task 4/5 的 UI 和装配都消费这里的类型。

**Files:**
- Modify: `crates/call-ui/src/store.rs` (CameraRow + 建表 + CRUD + 测试)

**Interfaces:**
- Consumes: 现有 `Store`/`open_at` 模式
- Produces:
  - `pub struct CameraRow { pub id: i64, pub name: String, pub uri: String, pub username: String, pub password: String }`
  - `Store::cameras(&self) -> Result<Vec<CameraRow>>` (按 name 排序)
  - `Store::add_camera(&self, name: &str, uri: &str, username: &str, password: &str) -> Result<i64>`
  - `Store::delete_camera(&self, id: i64) -> Result<()>`

- [ ] **Step 1: 写失败测试**

`store.rs` 测试模块加:

```rust
    #[test]
    fn cameras_crud_roundtrip() {
        let s = mem_store();
        assert!(s.cameras().unwrap().is_empty());

        let id = s
            .add_camera("大门", "rtsp://192.168.1.64:554/ch01", "admin", "123456")
            .unwrap();
        let id2 = s
            .add_camera("后院", "rtsp://192.168.1.65/11", "", "")
            .unwrap();
        let all = s.cameras().unwrap();
        assert_eq!(all.len(), 2);
        let c = all.iter().find(|c| c.id == id).unwrap();
        assert_eq!(
            (c.name.as_str(), c.uri.as_str(), c.username.as_str()),
            ("大门", "rtsp://192.168.1.64:554/ch01", "admin")
        );

        s.delete_camera(id2).unwrap();
        let all = s.cameras().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p call-ui cameras_crud_roundtrip`
Expected: FAIL — no method `cameras`/`add_camera` (编译错误)

- [ ] **Step 3: 实现**

`store.rs` 顶部 `CallRecordRow` 后加:

```rust
/// 一条保存的网络摄像头 (监控页数据源; 密码明文本机存储, 见设计文档风险节)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraRow {
    pub id: i64,
    pub name: String,
    pub uri: String,
    pub username: String,
    pub password: String,
}
```

`open_at` 的 `execute_batch` SQL 末尾追加:

```sql
            CREATE TABLE IF NOT EXISTS cameras (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                name       TEXT NOT NULL,
                uri        TEXT NOT NULL,
                username   TEXT NOT NULL DEFAULT '',
                password   TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (datetime('now', 'localtime'))
            );
```

`Store` impl 加:

```rust
    /// 全部已保存摄像头, 按名称排序
    pub fn cameras(&self) -> Result<Vec<CameraRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, uri, username, password FROM cameras
             ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CameraRow {
                id: r.get(0)?,
                name: r.get(1)?,
                uri: r.get(2)?,
                username: r.get(3)?,
                password: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn add_camera(&self, name: &str, uri: &str, username: &str, password: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO cameras (name, uri, username, password) VALUES (?1, ?2, ?3, ?4)",
            params![name, uri, username, password],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_camera(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM cameras WHERE id = ?1", params![id])?;
        Ok(())
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p call-ui store` → PASS (3 个 store 测试)

- [ ] **Step 5: Commit**

```bash
git add crates/call-ui/src/store.rs
git commit -m "[新增] call-ui: Store 加 cameras 表 (监控页摄像头持久化)"
```

---

### Task 4: slint 共享类型 + 设置页摄像头管理 UI

设置页加"摄像头"卡片: 已存列表 (可删) + 搜索添加对话框 (扫描 → 选设备 → 输账号密码 → 选 profile) + 手动添加对话框。对话框用页面内覆盖层实现 (Material 组件集无可靠 Dialog, 且全屏 PopupWindow 会吃事件 — toast 注释里有前科)。本任务只写 UI 和回调声明, Rust 装配在 Task 5。

**Files:**
- Modify: `crates/call-ui/ui/components.slint` (加 CameraEntry/DiscoveredEntry/ProfileEntry)
- Modify: `crates/call-ui/ui/main.slint` (加属性/回调声明, 透传给 SettingsPage)
- Modify: `crates/call-ui/ui/settings_page.slint` (摄像头卡片 + 两个对话框覆盖层)

**Interfaces:**
- Consumes: 无 (纯 UI)
- Produces (Task 5/6 依赖的回调与属性, 签名必须逐字一致):
  - `in property <[CameraEntry]> cameras` / `in property <bool> camera-busy` / `in property <[DiscoveredEntry]> discovered-devices` / `in property <[ProfileEntry]> camera-profiles`
  - `callback add-camera(string, string, string, string)` (name, uri, user, pass)
  - `callback delete-camera(int)`
  - `callback search-cameras()`
  - `callback fetch-camera-profiles(string, string, string)` (xaddr, user, pass)
  - `callback save-discovered-camera(string, string, string, string, string)` (xaddr, name, user, pass, profile-token)
  - struct: `CameraEntry { id: int, name: string, uri: string }`, `DiscoveredEntry { index: int, label: string, xaddr: string }`, `ProfileEntry { token: string, label: string }`

- [ ] **Step 1: components.slint 加共享类型**

`ContactEntry` 后加:

```slint
// 已保存摄像头 (SQLite cameras 表, Rust store.rs 维护)
export struct CameraEntry {
    id: int,
    name: string,
    uri: string,
}

// ONVIF 搜索到的设备 (未保存; index 是 Rust 侧列表下标)
export struct DiscoveredEntry {
    index: int,
    label: string,   // "厂商 型号 (地址)" 或裸地址
    xaddr: string,
}

// 设备的媒体 profile (选择后保存)
export struct ProfileEntry {
    token: string,
    label: string,   // "H264 1920x1080" 等
}
```

- [ ] **Step 2: main.slint 加声明**

`import { CallLogEntry, ContactEntry, AppToast, ErrorBanner }` 改为 `import { CallLogEntry, ContactEntry, CameraEntry, DiscoveredEntry, ProfileEntry, AppToast, ErrorBanner }`。

属性区 (`in property <[ContactEntry]> contacts: [];` 后) 加:

```slint
    in property <[CameraEntry]> cameras: [];
    // 摄像头操作忙标志 (搜索中/拉 profile 中), 对话框里转菊花文案
    in property <bool> camera-busy: false;
    in property <[DiscoveredEntry]> discovered-devices: [];
    in property <[ProfileEntry]> camera-profiles: [];
```

回调区 (`callback delete-contact(int);` 后) 加:

```slint
    // 摄像头管理 (设置页): 手动添加/删除/ONVIF 搜索/拉 profile/保存搜索到的设备
    callback add-camera(string, string, string, string); // name, uri, user, pass
    callback delete-camera(int);
    callback search-cameras();
    callback fetch-camera-profiles(string, string, string); // xaddr, user, pass
    callback save-discovered-camera(string, string, string, string, string); // xaddr, name, user, pass, token
```

SettingsPage 实例 (`if root.phase == 0 && root.page == 2: SettingsPage`) 加透传:

```slint
        cameras: root.cameras;
        camera-busy: root.camera-busy;
        discovered-devices: root.discovered-devices;
        camera-profiles: root.camera-profiles;
        add-camera(n, u, user, p) => { root.add-camera(n, u, user, p); }
        delete-camera(id) => { root.delete-camera(id); }
        search-cameras => { root.search-cameras(); }
        fetch-camera-profiles(x, user, p) => { root.fetch-camera-profiles(x, user, p); }
        save-discovered-camera(x, n, user, p, t) => { root.save-discovered-camera(x, n, user, p, t); }
        show-toast(t) => { root.show-toast(t); }
```

- [ ] **Step 3: settings_page.slint 摄像头卡片 + 对话框**

整文件替换为 (保留原设置卡, 加摄像头卡和覆盖层; 外层包 ScrollView 防超高):

```slint
// 设置页: 只读配置信息 + 摄像头管理 (增删; 查看在监控页)
import {
    FilledCard,
    FilledButton,
    TextButton,
    IconButton,
    TextField,
    ListTile,
    HorizontalDivider,
    MaterialText,
    MaterialTypography,
    MaterialPalette,
} from "../material-1.1.0/material.slint";
import { ScrollView } from "std-widgets.slint";
import { CameraEntry, DiscoveredEntry, ProfileEntry } from "components.slint";

export component SettingsPage inherits Rectangle {
    in property <string> settings-text;
    in property <[CameraEntry]> cameras: [];
    in property <bool> camera-busy: false;
    in property <[DiscoveredEntry]> discovered-devices: [];
    in property <[ProfileEntry]> camera-profiles: [];
    callback back();
    callback set-language(string); // "zh" | "en"
    callback add-camera(string, string, string, string);
    callback delete-camera(int);
    callback search-cameras();
    callback fetch-camera-profiles(string, string, string);
    callback save-discovered-camera(string, string, string, string, string);
    callback show-toast(string);

    // 对话框: 0=无 1=搜索添加 2=手动添加
    property <int> dialog: 0;
    // 搜索对话框步骤: 0=设备列表 1=凭据 2=选 profile
    property <int> search-step: 0;
    property <string> sel-xaddr: "";
    property <string> sel-label: "";
    // 步骤 1 的输入在切到步骤 2 时快照进属性 (条件块内 id 跨块不可见,
    // 步骤 2 保存时用快照值而不是引用输入框)
    property <string> sel-name: "";
    property <string> sel-user: "";
    property <string> sel-pass: "";

    background: MaterialPalette.background;

    VerticalLayout {
        padding: 24px;
        spacing: 12px;
        HorizontalLayout {
            alignment: start;
            TextButton {
                icon: @image-url("icons/arrow-left.svg");
                text: @tr("返回");
                clicked => { root.back(); }
            }
        }

        ScrollView {
            vertical-stretch: 1;
            VerticalLayout {
                alignment: center;
                spacing: 16px;

                // ===== 原设置卡 (只读配置) =====
                FilledCard {
                    width: 640px;
                    VerticalLayout {
                        padding: 28px;
                        spacing: 20px;
                        MaterialText {
                            text: @tr("设置");
                            style: MaterialTypography.headline_small;
                        }

                        HorizontalDivider {}

                        HorizontalLayout {
                            spacing: 12px;
                            alignment: start;
                            MaterialText {
                                text: @tr("语言");
                                style: MaterialTypography.body_large;
                                color: MaterialPalette.on_surface_variant;
                                vertical-alignment: center;
                            }

                            TextButton {
                                text: "中文";
                                clicked => { root.set-language("zh"); }
                            }

                            TextButton {
                                text: "English";
                                clicked => { root.set-language("en"); }
                            }
                        }

                        HorizontalDivider {}

                        MaterialText {
                            text: root.settings-text;
                            style: MaterialTypography.body_large;
                            color: MaterialPalette.on_surface_variant;
                        }
                    }
                }

                // ===== 摄像头管理卡 =====
                FilledCard {
                    width: 640px;
                    VerticalLayout {
                        padding: 28px;
                        spacing: 16px;
                        MaterialText {
                            text: @tr("摄像头");
                            style: MaterialTypography.headline_small;
                        }

                        HorizontalDivider {}

                        if root.cameras.length == 0: MaterialText {
                            text: @tr("暂无已保存的摄像头");
                            style: MaterialTypography.body_large;
                            color: MaterialPalette.on_surface_variant;
                        }

                        for cam in root.cameras: ListTile {
                            text: cam.name;
                            supporting-text: cam.uri;
                            IconButton {
                                icon: @image-url("icons/trash-2.svg");
                                clicked => { root.delete-camera(cam.id); }
                            }
                        }

                        HorizontalLayout {
                            spacing: 12px;
                            alignment: start;
                            FilledButton {
                                text: @tr("搜索添加");
                                clicked => {
                                    root.search-step = 0;
                                    root.dialog = 1;
                                    root.search-cameras();
                                }
                            }

                            TextButton {
                                text: @tr("手动添加");
                                clicked => { root.dialog = 2; }
                            }
                        }
                    }
                }
            }
        }
    }

    // ===== 对话框遮罩 (纯 Rectangle 覆盖层, 不用 PopupWindow) =====
    if root.dialog != 0: Rectangle {
        width: 100%;
        height: 100%;
        background: #00000080;
        TouchArea {} // 吃掉点击, 不穿透到下层

        FilledCard {
            x: (parent.width - self.width) / 2;
            y: (parent.height - self.height) / 2;
            width: 560px;
            height: 480px;

            // ----- 搜索添加 -----
            if root.dialog == 1: VerticalLayout {
                padding: 24px;
                spacing: 12px;
                MaterialText {
                    text: @tr("搜索添加摄像头");
                    style: MaterialTypography.title_large;
                }

                // 步骤 0: 设备列表
                if root.search-step == 0: VerticalLayout {
                    spacing: 8px;
                    vertical-stretch: 1;
                    if root.camera-busy: MaterialText {
                        text: @tr("正在搜索局域网设备 ...");
                        style: MaterialTypography.body_large;
                        color: MaterialPalette.on_surface_variant;
                    }
                    if !root.camera-busy && root.discovered-devices.length == 0: MaterialText {
                        text: @tr("未发现设备, 可重试或手动添加");
                        style: MaterialTypography.body_large;
                        color: MaterialPalette.on_surface_variant;
                    }
                    ScrollView {
                        vertical-stretch: 1;
                        for dev in root.discovered-devices: ListTile {
                            text: dev.label;
                            supporting-text: dev.xaddr;
                            clicked => {
                                root.sel-xaddr = dev.xaddr;
                                root.sel-label = dev.label;
                                root.search-step = 1;
                            }
                        }
                    }

                    HorizontalLayout {
                        spacing: 12px;
                        alignment: end;
                        TextButton {
                            text: @tr("重新搜索");
                            clicked => { root.search-cameras(); }
                        }
                    }
                }

                // 步骤 1: 凭据
                if root.search-step == 1: VerticalLayout {
                    spacing: 12px;
                    vertical-stretch: 1;
                    MaterialText {
                        text: root.sel-label;
                        style: MaterialTypography.body_large;
                    }

                    name-input := TextField {
                        label: @tr("名称");
                        text: root.sel-label;
                    }

                    user-input := TextField {
                        label: @tr("用户名");
                    }

                    pass-input := TextField {
                        label: @tr("密码");
                        input-type: password;
                    }

                    if root.camera-busy: MaterialText {
                        text: @tr("正在读取设备配置 ...");
                        style: MaterialTypography.body_large;
                        color: MaterialPalette.on_surface_variant;
                    }

                    HorizontalLayout {
                        spacing: 12px;
                        alignment: end;
                        TextButton {
                            text: @tr("上一步");
                            clicked => { root.search-step = 0; }
                        }

                        FilledButton {
                            text: @tr("获取配置");
                            enabled: name-input.text != "";
                            clicked => {
                                root.sel-name = name-input.text;
                                root.sel-user = user-input.text;
                                root.sel-pass = pass-input.text;
                                root.fetch-camera-profiles(root.sel-xaddr, root.sel-user, root.sel-pass);
                                root.search-step = 2;
                            }
                        }
                    }
                }

                // 步骤 2: 选 profile 保存
                if root.search-step == 2: VerticalLayout {
                    spacing: 8px;
                    vertical-stretch: 1;
                    if root.camera-profiles.length == 0: MaterialText {
                        text: root.camera-busy ? @tr("正在读取设备配置 ...") : @tr("未读到媒体配置, 请检查账号密码");
                        style: MaterialTypography.body_large;
                        color: MaterialPalette.on_surface_variant;
                    }

                    for prof in root.camera-profiles: ListTile {
                        text: prof.label;
                        clicked => {
                            root.save-discovered-camera(root.sel-xaddr, root.sel-name, root.sel-user, root.sel-pass, prof.token);
                            root.dialog = 0;
                        }
                    }

                    HorizontalLayout {
                        spacing: 12px;
                        alignment: end;
                        TextButton {
                            text: @tr("上一步");
                            clicked => { root.search-step = 1; }
                        }
                    }
                }

                HorizontalDivider {}

                HorizontalLayout {
                    alignment: end;
                    TextButton {
                        text: @tr("取消");
                        clicked => { root.dialog = 0; }
                    }
                }
            }

            // ----- 手动添加 -----
            if root.dialog == 2: VerticalLayout {
                padding: 24px;
                spacing: 12px;
                MaterialText {
                    text: @tr("手动添加摄像头");
                    style: MaterialTypography.title_large;
                }

                m-name := TextField {
                    label: @tr("名称");
                }

                m-uri := TextField {
                    label: "RTSP URL";
                }

                m-user := TextField {
                    label: @tr("用户名 (可空)");
                }

                m-pass := TextField {
                    label: @tr("密码 (可空)");
                    input-type: password;
                }

                Rectangle { vertical-stretch: 1; }

                HorizontalLayout {
                    spacing: 12px;
                    alignment: end;
                    TextButton {
                        text: @tr("取消");
                        clicked => { root.dialog = 0; }
                    }

                    FilledButton {
                        text: @tr("保存");
                        enabled: m-name.text != "" && m-uri.text != "";
                        clicked => {
                            root.add-camera(m-name.text, m-uri.text, m-user.text, m-pass.text);
                            root.dialog = 0;
                        }
                    }
                }
            }
        }
    }
}
```

注意: 步骤 1 的输入框 id 在 `if` 条件块内, 步骤 2 的块里不可见 — 所以
上面在点"获取配置"时把三个值快照进 `sel-name/sel-user/sel-pass` 属性,
步骤 2 保存用快照值, 不引用输入框 (slint 编译器会抓跨块 id 引用)。

- [ ] **Step 4: 编译验证**

Run: `cargo build -p call-ui`
Expected: 编译通过 (slint 编译期检查回调/属性签名); 若有 unused 警告属正常 (Task 5 才接 Rust)

- [ ] **Step 5: Commit**

```bash
git add crates/call-ui/ui/
git commit -m "[新增] call-ui: 设置页摄像头管理 UI (列表/搜索添加/手动添加对话框)"
```

---

### Task 5: Rust 装配 — wire_cameras + ONVIF 搜索桥

新模块 `camera.rs`: 通讯录模式的 CRUD 装配 + ONVIF 搜索/拉 profile/保存的线程桥 (UI 线程发起 → 独立线程跑 tokio current_thread runtime → `invoke_from_event_loop` 回属性)。本任务只做设置页部分; 监控页控制器在 Task 6 进同一文件。

**Files:**
- Create: `crates/call-ui/src/camera.rs`
- Modify: `crates/call-ui/Cargo.toml` (加 ipcam-discovery 依赖)
- Modify: `crates/call-ui/src/props.rs` (refresh_cameras)
- Modify: `crates/call-ui/src/lib.rs` (mod camera + run() 装配)
- Test: `crates/call-ui/src/camera.rs` (纯函数单测)

**Interfaces:**
- Consumes: Task 3 `Store::cameras/add_camera/delete_camera`; Task 4 的回调/属性签名; `ipcam_discovery::{probe_all_with_config, DiscoveryConfig, DeviceManagementClient, DiscoveryCredentials, parse_xaddr_endpoint}`
- Produces:
  - `pub(crate) fn wire_settings(ui: &MainWindow, store: &Rc<Store>)` — 设置页全部摄像头回调装配
  - `fn device_label(d: &ipcam_core::DiscoveredDevice) -> String` (可测纯函数)
  - `fn profile_label(p: &ipcam_core::VideoProfile) -> String` (可测纯函数)

- [ ] **Step 1: 加依赖**

`crates/call-ui/Cargo.toml` `[dependencies]` 加:

```toml
ipcam-discovery = { path = "../ipcam-discovery" }
```

- [ ] **Step 2: 写失败测试**

`camera.rs` 先写纯函数和测试 (不依赖 UI):

```rust
//! 摄像头功能: 设置页管理 (搜索/添加/删除) + 监控页查看控制 (Task 6).
//! ONVIF 搜索是异步网络操作, 在独立线程跑 tokio runtime, 结果经
//! invoke_from_event_loop 回 UI 线程刷属性, UI 线程零阻塞.

use std::rc::Rc;

use slint::ComponentHandle;
use tracing::{error, warn};

use crate::MainWindow;
use crate::props;
use crate::store::Store;

/// 设备列表行文案: 有厂商/型号拼 "厂商 型号 (地址)", 都没有退化为地址
fn device_label(d: &ipcam_core::DiscoveredDevice) -> String {
    match (&d.manufacturer, &d.model) {
        (Some(m), Some(md)) => format!("{m} {md} ({})", d.address),
        _ => d.address.clone(),
    }
}

/// profile 行文案: "H264 1920x1080"; 未知分辨率只写编码
fn profile_label(p: &ipcam_core::VideoProfile) -> String {
    if p.width > 0 {
        format!("{:?} {}x{}", p.codec, p.width, p.height)
    } else {
        format!("{:?}", p.codec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_label_prefers_manufacturer_model() {
        let d = ipcam_core::DiscoveredDevice {
            id: Default::default(), // DeviceId 是 Uuid 类型别名, nil 即可
            address: "192.168.1.64".into(),
            xaddr: None,
            scopes: vec![],
            manufacturer: Some("Hikvision".into()),
            model: Some("DS-2CD".into()),
            auth_status: ipcam_core::AuthStatus::Unknown,
            profiles: vec![],
        };
        assert_eq!(device_label(&d), "Hikvision DS-2CD (192.168.1.64)");
    }

    #[test]
    fn profile_label_with_and_without_resolution() {
        let p = ipcam_core::VideoProfile {
            profile_id: "p0".into(),
            uri: None,
            codec: ipcam_core::VideoCodec::H264,
            width: 1920,
            height: 1080,
            fps: 25.0,
        };
        assert_eq!(profile_label(&p), "H264 1920x1080");
    }
}
```

(`DeviceId` 在 `ipcam-core/src/lib.rs:15` 是 `Uuid` 类型别名, 测试里
`Default::default()` 走 Uuid 的 nil 值, 不需要给 call-ui 加 uuid 依赖。)

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test -p call-ui device_label`
Expected: FAIL — `camera` module 未注册 / 函数不存在 (编译错误)

- [ ] **Step 4: 实现 wire_settings**

`camera.rs` 追加装配代码:

```rust
/// 设置页摄像头管理: CRUD 全量重刷 (通讯录模式) + ONVIF 搜索线程桥
pub(crate) fn wire_settings(ui: &MainWindow, store: &Rc<Store>) {
    props::refresh_cameras(ui, store);

    // 手动添加
    {
        let (store, weak) = (store.clone(), ui.as_weak());
        ui.on_add_camera(move |name, uri, user, pass| {
            if let Err(e) = store.add_camera(&name, &uri, &user, &pass) {
                error!(err = %e, "add camera failed");
            }
            if let Some(ui) = weak.upgrade() {
                props::refresh_cameras(&ui, &store);
            }
        });
    }
    // 删除
    {
        let (store, weak) = (store.clone(), ui.as_weak());
        ui.on_delete_camera(move |id| {
            if let Err(e) = store.delete_camera(id as i64) {
                error!(err = %e, "delete camera failed");
            }
            if let Some(ui) = weak.upgrade() {
                props::refresh_cameras(&ui, &store);
            }
        });
    }
    // ONVIF 搜索: busy 置位 → 线程搜索 → 回刷设备列表
    {
        let weak = ui.as_weak();
        ui.on_search_cameras(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_camera_busy(true);
            let weak = weak.clone();
            std::thread::spawn(move || {
                let devices = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map(|rt| {
                        rt.block_on(ipcam_discovery::probe_all_with_config(
                            ipcam_discovery::DiscoveryConfig::default(),
                        ))
                    })
                    .unwrap_or_default();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let entries: Vec<crate::DiscoveredEntry> = devices
                        .iter()
                        .enumerate()
                        .map(|(i, d)| crate::DiscoveredEntry {
                            index: i as i32,
                            label: device_label(d).into(),
                            xaddr: d.xaddr.clone().unwrap_or_default().into(),
                        })
                        .collect();
                    ui.set_discovered_devices(slint::ModelRc::new(slint::VecModel::from(entries)));
                    ui.set_camera_busy(false);
                });
            });
        });
    }
    // 拉 profile: 选中设备 + 凭据 → DeviceManagementClient
    {
        let weak = ui.as_weak();
        ui.on_fetch_camera_profiles(move |xaddr, user, pass| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_camera_busy(true);
            ui.set_camera_profiles(slint::ModelRc::new(slint::VecModel::from(
                Vec::<crate::ProfileEntry>::new(),
            )));
            let weak = weak.clone();
            std::thread::spawn(move || {
                let profiles = fetch_profiles_blocking(&xaddr, &user, &pass);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match profiles {
                        Ok(list) => {
                            let entries: Vec<crate::ProfileEntry> = list
                                .iter()
                                .map(|p| crate::ProfileEntry {
                                    token: p.profile_id.clone().into(),
                                    label: profile_label(p).into(),
                                })
                                .collect();
                            ui.set_camera_profiles(slint::ModelRc::new(slint::VecModel::from(
                                entries,
                            )));
                        }
                        Err(e) => warn!(error = %e, "fetch camera profiles failed"),
                    }
                    ui.set_camera_busy(false);
                });
            });
        });
    }
    // 保存搜索到的设备: 取 profile 的 RTSP URI (profile 自带 uri 优先,
    // 否则 get_stream_uri 现取) → 入库 → 重刷列表 + toast
    {
        let (store, weak) = (store.clone(), ui.as_weak());
        ui.on_save_discovered_camera(move |xaddr, name, user, pass, token| {
            let (store, weak) = (store.clone(), weak.clone());
            std::thread::spawn(move || {
                let saved = save_discovered_blocking(&xaddr, &user, &pass, &token);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match saved {
                        Ok(uri) => {
                            if let Err(e) = store.add_camera(&name, &uri, &user, &pass) {
                                error!(err = %e, "add camera failed");
                            }
                            props::refresh_cameras(&ui, &store);
                            ui.invoke_notify(0, 0, slint::SharedString::from("摄像头已保存"));
                        }
                        Err(e) => {
                            warn!(error = %e, "save discovered camera failed");
                            ui.invoke_notify(1, 0, slint::SharedString::from(format!("保存失败: {e}")));
                        }
                    }
                });
            });
        });
    }
}

/// 阻塞版 profile 拉取 (独立线程 + 一次性 runtime)
fn fetch_profiles_blocking(
    xaddr: &str,
    user: &str,
    pass: &str,
) -> Result<Vec<ipcam_core::VideoProfile>, ipcam_discovery::DiscoveryError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(ipcam_discovery::DiscoveryError::Io)?;
    rt.block_on(async {
        let url = ipcam_discovery::parse_xaddr_endpoint(xaddr)?;
        let client = ipcam_discovery::DeviceManagementClient::new(
            url,
            ipcam_discovery::DiscoveryCredentials::new(user, pass),
            std::time::Duration::from_secs(5),
        );
        client.list_profiles().await
    })
}

/// 阻塞版保存: 重拉 profile 列表找到 token 对应项, 自带 uri 优先,
/// 否则 get_stream_uri 现取 (凭据此时必然有效, 上一步刚验证过)
fn save_discovered_blocking(
    xaddr: &str,
    user: &str,
    pass: &str,
    token: &str,
) -> Result<String, ipcam_discovery::DiscoveryError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(ipcam_discovery::DiscoveryError::Io)?;
    rt.block_on(async {
        let url = ipcam_discovery::parse_xaddr_endpoint(xaddr)?;
        let client = ipcam_discovery::DeviceManagementClient::new(
            url,
            ipcam_discovery::DiscoveryCredentials::new(user, pass),
            std::time::Duration::from_secs(5),
        );
        let profiles = client.list_profiles().await?;
        match profiles.iter().find(|p| p.profile_id == token) {
            Some(p) => match &p.uri {
                Some(uri) => Ok(uri.clone()),
                None => client.get_stream_uri(token).await,
            },
            None => Err(ipcam_discovery::DiscoveryError::DeviceMgmt(format!(
                "profile {token} not found on device"
            ))),
        }
    })
}
```

注意: `parse_xaddr_endpoint` 返回 `Result<reqwest::Url, DiscoveryError>` (`oxvif_backend.rs:158`), 上面已按 `?` 传播写好。

`invoke_notify` 是 main.slint 里 slint 实现的 notify 回调, Rust 侧生成 `invoke_notify(level, kind, detail)`; level 0/1 → toast, kind 0 = Raw (detail 原文显示)。

`props.rs` 加:

```rust
/// 从 SQLite 全量刷新摄像头列表到 slint 属性 (同 refresh_contacts 模式)
pub(crate) fn refresh_cameras(ui: &MainWindow, store: &store::Store) {
    match store.cameras() {
        Ok(list) => {
            let entries: Vec<crate::CameraEntry> = list
                .iter()
                .map(|c| crate::CameraEntry {
                    id: c.id as i32,
                    name: c.name.as_str().into(),
                    uri: c.uri.as_str().into(),
                })
                .collect();
            ui.set_cameras(slint::ModelRc::new(slint::VecModel::from(entries)));
        }
        Err(e) => error!(err = %e, "load cameras failed"),
    }
}
```

`lib.rs`: `mod persist;` 后加 `mod camera;`; `run()` 里 `persist::wire_contacts(&ui, &store);` 后加 `camera::wire_settings(&ui, &store);`。

- [ ] **Step 5: 跑测试 + 门禁**

Run: `cargo test -p call-ui camera` → PASS
Run: 门禁四连

- [ ] **Step 6: Commit**

```bash
git add crates/call-ui/
git commit -m "[新增] call-ui: 设置页摄像头管理装配 (CRUD + ONVIF 搜索线程桥)"
```

---

### Task 6: 监控页 UI + MonitorController + 通话冲突接线

监控页: 可折叠侧边栏 (已保存摄像头列表) + 视频区 (复用 `video-frame` 属性) + 录制/截图占位按钮。MonitorController 管 ingest 句柄 (Task 1 的本地模式 + Task 2 的限宽)。拨打/接听时停止监控 (同一路 `make_frame_sink` 和 video-frame 属性不能两家同时用)。

**Files:**
- Create: `crates/call-ui/ui/monitor_page.slint`
- Modify: `crates/call-ui/ui/main.slint` (page==3 分支 + monitor 回调/属性)
- Modify: `crates/call-ui/ui/home/app_grid.slint:18-23` ("监视"图标 open-page(3))
- Modify: `crates/call-ui/src/camera.rs` (MonitorController + wire_monitor)
- Modify: `crates/call-ui/src/wiring.rs` (wire_callbacks 加 monitor 参数, dial/accept 时停监控)
- Modify: `crates/call-ui/src/lib.rs` (run() 装配)
- Test: `crates/call-ui/src/camera.rs` (MonitorController 状态单测)

**Interfaces:**
- Consumes: Task 1 `GstStreamConfig.webrtc`、Task 2 `tap_max_width`、Task 3 `CameraRow`、Task 4 `CameraEntry`、现有 `video::make_frame_sink`/`ipcam_gst::{start_with_taps, RawTaps, GstStreamHandle}`
- Produces:
  - `pub(crate) struct MonitorController { current: Option<ipcam_gst::GstStreamHandle>, selected: Option<i64> }`
  - `MonitorController::select(&mut self, cam: &CameraRow, tap: &VideoFrameSink) -> Result<(), ipcam_gst::GstStreamError>`
  - `MonitorController::stop(&mut self)`
  - `pub(crate) fn wire_monitor(ui: &MainWindow, store: &Rc<Store>, monitor: &Rc<RefCell<MonitorController>>, tap: &VideoFrameSink)`
  - slint: `callback monitor-select(int)` / `callback monitor-leave()` / `in property <int> monitor-selected-id`

- [ ] **Step 1: 写失败测试**

`camera.rs` 测试模块加 (controller 不依赖 UI; 不真起管线, 只验证状态机):

```rust
    #[test]
    fn monitor_stop_is_idempotent_and_clears_selection() {
        let mut c = MonitorController::new();
        assert!(c.selected_id().is_none());
        c.stop(); // 空状态 stop 不 panic
        assert!(c.selected_id().is_none());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p call-ui monitor_stop`
Expected: FAIL — `MonitorController` 不存在 (编译错误)

- [ ] **Step 3: camera.rs 加 MonitorController + wire_monitor**

```rust
use std::cell::RefCell;

use ipcam_gst::VideoFrameSink;

/// 监控查看控制器: 任一时刻最多一路 ingest 在跑 (单路全屏形态).
/// 与对讲共用同一个 tap/video-frame 属性 — 靠"phase==0 才能进监控页
/// + 拨打/接听时强制 stop"保证不并发
pub(crate) struct MonitorController {
    current: Option<ipcam_gst::GstStreamHandle>,
    selected: Option<i64>,
}

impl MonitorController {
    pub fn new() -> Self {
        Self {
            current: None,
            selected: None,
        }
    }

    pub fn selected_id(&self) -> Option<i64> {
        self.selected
    }

    /// 选中摄像头: 停旧起新 (本地查看模式 + 限宽 640, 不要摄像头音频).
    /// 启动失败 (RTSP 不通/认证错) 时旧流已停, 返回 Err 由调用方 toast
    pub fn select(
        &mut self,
        cam: &crate::store::CameraRow,
        tap: &VideoFrameSink,
    ) -> Result<(), ipcam_gst::GstStreamError> {
        self.stop();
        let cfg = ipcam_gst::GstStreamConfig {
            uri: cam.uri.clone(),
            credentials: if cam.username.is_empty() {
                None
            } else {
                Some((cam.username.clone(), cam.password.clone()))
            },
            audio_output: ipcam_gst::AudioOutput::Disabled,
            webrtc: false,
            tap_max_width: Some(640),
            // stream_name 本地模式用不到 (不建 webrtcsink), 随便给个唯一的
            stream_name: format!("monitor-{}", cam.id),
            ..Default::default()
        };
        let taps = ipcam_gst::RawTaps {
            video: Some(tap.clone()),
            audio: None,
        };
        let handle = ipcam_gst::start_with_taps(cfg, taps)?;
        self.current = Some(handle);
        self.selected = Some(cam.id);
        Ok(())
    }

    /// 停止当前查看 (幂等)
    pub fn stop(&mut self) {
        if let Some(h) = self.current.take() {
            h.stop();
        }
        self.selected = None;
    }
}

/// 监控页装配: 选中查看 / 离开停止
pub(crate) fn wire_monitor(
    ui: &MainWindow,
    store: &Rc<Store>,
    monitor: &Rc<RefCell<MonitorController>>,
    tap: &VideoFrameSink,
) {
    {
        let (store, monitor, tap, weak) = (store.clone(), monitor.clone(), tap.clone(), ui.as_weak());
        ui.on_monitor_select(move |id| {
            let cam = store
                .cameras()
                .ok()
                .and_then(|list| list.into_iter().find(|c| c.id == id as i64));
            let Some(cam) = cam else { return };
            let mut m = monitor.borrow_mut();
            match m.select(&cam, &tap) {
                Ok(()) => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_monitor_selected_id(id);
                    }
                }
                Err(e) => {
                    warn!(error = %e, uri = %cam.uri, "monitor select failed");
                    if let Some(ui) = weak.upgrade() {
                        ui.set_monitor_selected_id(-1);
                        ui.invoke_notify(1, 0, slint::SharedString::from("摄像头连接失败"));
                    }
                }
            }
        });
    }
    {
        let monitor = monitor.clone();
        ui.on_monitor_leave(move || {
            monitor.borrow_mut().stop();
        });
    }
}
```

`RawTaps` 的 `video`/`audio` 字段是 pub (tap.rs:46-51), 按上面字面量构造即可。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p call-ui monitor` → PASS

- [ ] **Step 5: monitor_page.slint (新建)**

```slint
// 监控页: 可折叠侧边栏切换已保存摄像头, 右侧视频画面 (复用对讲的
// video-frame 通路). 录制/截图是占位 (toast 暂不支持), 见设计文档预留节
import {
    FilledCard,
    IconButton,
    ListTile,
    MaterialText,
    MaterialTypography,
    MaterialPalette,
    OutlineButton,
} from "../material-1.1.0/material.slint";
import { ScrollView } from "std-widgets.slint";
import { CameraEntry } from "components.slint";

export component MonitorPage inherits Rectangle {
    in property <[CameraEntry]> cameras: [];
    in property <image> video-frame;
    in property <int> selected-id: -1;
    callback back();
    callback select-camera(int);
    callback show-toast(string);

    property <bool> sidebar-open: true;
    background: MaterialPalette.background;

    HorizontalLayout {
        // ===== 可折叠侧边栏 =====
        if root.sidebar-open: Rectangle {
            width: 260px;
            background: MaterialPalette.surface_container;
            VerticalLayout {
                padding: 12px;
                spacing: 4px;
                MaterialText {
                    text: @tr("摄像头");
                    style: MaterialTypography.title_medium;
                }

                if root.cameras.length == 0: MaterialText {
                    text: @tr("暂无摄像头\n请到设置页添加");
                    style: MaterialTypography.body_medium;
                    color: MaterialPalette.on_surface_variant;
                }

                ScrollView {
                    vertical-stretch: 1;
                    for cam in root.cameras: Rectangle {
                        height: 56px;
                        background: cam.id == root.selected-id ? MaterialPalette.secondary_container : transparent;
                        border-radius: 8px;
                        ListTile {
                            width: 100%;
                            text: cam.name;
                            clicked => { root.select-camera(cam.id); }
                        }
                    }
                }
            }
        }

        // ===== 视频区 =====
        Rectangle {
            horizontal-stretch: 1;
            background: #000000;

            Image {
                width: 100%;
                height: 100%;
                source: root.video-frame;
                image-fit: contain;
            }

            // 顶部工具条: 折叠/返回
            HorizontalLayout {
                y: 8px;
                x: 8px;
                spacing: 4px;
                IconButton {
                    icon: @image-url("icons/menu.svg");
                    clicked => { root.sidebar-open = !root.sidebar-open; }
                }

                IconButton {
                    icon: @image-url("icons/arrow-left.svg");
                    clicked => { root.back(); }
                }
            }

            // 底部操作: 录制/截图 (占位)
            HorizontalLayout {
                y: parent.height - 56px;
                width: 100%;
                alignment: center;
                spacing: 16px;
                OutlineButton {
                    text: @tr("录制");
                    clicked => { root.show-toast(@tr("录制功能暂不支持")); }
                }

                OutlineButton {
                    text: @tr("截图");
                    clicked => { root.show-toast(@tr("截图功能暂不支持")); }
                }
            }
        }
    }
}
```

icons 目录已有 arrow-left.svg 和 trash-2.svg; menu.svg 不存在, 新建
`crates/call-ui/ui/icons/menu.svg` (lucide menu 图标, 风格与现有一致):

```svg
<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="4" x2="20" y1="6" y2="6"/><line x1="4" x2="20" y1="12" y2="12"/><line x1="4" x2="20" y1="18" y2="18"/></svg>
```

- [ ] **Step 6: main.slint / app_grid.slint 接线**

main.slint: import 加 `import { MonitorPage } from "monitor_page.slint";`; 属性区加 `in property <int> monitor-selected-id: -1;`; 回调区加:

```slint
    // 监控页: 选中查看 / 离开停止 (Rust 侧 MonitorController)
    callback monitor-select(int);
    callback monitor-leave();
```

SettingsPage 块后加:

```slint
    // ===== 监控页 =====
    if root.phase == 0 && root.page == 3: MonitorPage {
        y: 30px;
        width: 100%;
        height: parent.height - 30px;
        cameras: root.cameras;
        video-frame: root.video-frame;
        selected-id: root.monitor-selected-id;
        back => {
            root.monitor-leave();
            root.page = 0;
        }
        select-camera(id) => { root.monitor-select(id); }
        show-toast(t) => { root.show-toast(t); }
    }
```

app_grid.slint: "监视" AppIcon 的 `clicked => { root.show-toast(@tr("门口机监视功能开发中")); }` 改为 `clicked => { root.open-page(3); }`; 顶部 open-page 注释改 `// 1=拨号 2=设置 3=监控`。

- [ ] **Step 7: wiring.rs / lib.rs 装配 (通话冲突)**

`wiring.rs` 的 `wire_callbacks` 加参数 `monitor: &std::rc::Rc<std::cell::RefCell<crate::camera::MonitorController>>`; dial 和 accept-call 两个回调闭包的第一行加:

```rust
        // 监控查看与通话共用视频通路: 起通话前必须停掉监控 ingest
        monitor.borrow_mut().stop();
```

`lib.rs` `run()`: `let tap = video::make_frame_sink(ui.as_weak());` 后把 `session::spawn(cfg, cmd_rx, handle, tap)?` 改为传 `tap.clone()`; `wiring::wire_callbacks(...)` 调用加 monitor 实参; `camera::wire_settings(&ui, &store);` 后加:

```rust
    let monitor = std::rc::Rc::new(std::cell::RefCell::new(camera::MonitorController::new()));
    wiring::wire_callbacks(&ui, &app, &cmd_tx, &monitor);
    camera::wire_monitor(&ui, &store, &monitor, &tap);
```

(即: wire_callbacks 原调用替换为带 monitor 的版本; monitor 创建挪到 wire_callbacks 之前; camera::wire_settings 位置不变。)

- [ ] **Step 8: 编译 + 测试 + 门禁**

Run: `cargo test -p call-ui` → PASS
Run: 门禁四连 (含 workspace clippy — wiring 签名变更影响面要全绿)

- [ ] **Step 9: Commit**

```bash
git add crates/call-ui/
git commit -m "[新增] call-ui: 监控页 (侧边栏切换摄像头 + 本地 ingest 查看), 通话时自动停止监控"
```

---

### Task 7: 收尾 — 全量门禁 + 手测清单 + 文档

**Files:**
- Modify: `docs/pitfalls.md` (待办节更新)

- [ ] **Step 1: 全量门禁**

Run: `cargo fmt --all -- --check`
Run: `cargo clippy --workspace --all-targets --features sw-decode -- -D warnings`
Run: `cargo clippy -p call-ui --all-targets -- -D warnings`
Run: `cargo test --workspace --features sw-decode`
Expected: 全绿

- [ ] **Step 2: 手测 (本机)**

```bash
cargo run --release --bin media_talk_rust gui
```

清单:
- [ ] 主页"监视"图标进监控页; 空列表提示去设置页添加
- [ ] 设置页手动添加一个摄像头 (可用 `assets/oceans.mp4` 起一个本地 RTSP 流或真实摄像头), 监控页侧边栏出现, 点击出画面
- [ ] 搜索添加: 局域网有 ONVIF 摄像头时走通 扫描→选设备→输凭据→选 profile→保存
- [ ] 查看中拨打电话: 监控停止, 通话视频正常; 挂断回主页
- [ ] 录制/截图按钮 toast"暂不支持"
- [ ] web 端 (`serve` 子命令) 回归: 浏览器播放不受影响 (Task 1/2 动了公共管线)

- [ ] **Step 3: pitfalls.md 更新**

待办节"监控页侧边栏缩略图预览"条目改为注明功能已落地 (引用 commit), 预留项保留; 若手测踩到新坑 (如某摄像头 RTSP 协商问题), 按"现象→根因→解法"格式补条目。

- [ ] **Step 4: Commit**

```bash
git add docs/pitfalls.md
git commit -m "[文档] pitfalls: 摄像头监控功能落地, 更新预留项状态"
```
