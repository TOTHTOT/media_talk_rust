# rtp_send 多输入源实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** rtp_send.rs 支持按轨选择输入源 (文件/RTSP 相机/本机相机/麦克风), 统一解码重编码后经现有 RTP 发送链发出。

**Architecture:** 每路媒体 (音频/视频) 独立指定 `TrackSource`; 文件/RTSP 走 uridecodebin 出裸流, 设备源按平台选元件; 所有源汇入统一的发送链 (视频 x264enc 重编码, 音频 G.711 重编码)。

**Tech Stack:** Rust, GStreamer (uridecodebin/x264enc/rtph264pay/alawenc/mulawenc), tracing。

**Spec:** `docs/superpowers/specs/2026-09-20-rtp-send-multi-source-design.md`

## Global Constraints

- 每条 cargo 命令前必须: `export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH" && export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"`
- 提交风格: `[类型] 描述` (中文, 英文标点 + 空格)
- 日志只用 tracing, 禁止 println
- 门禁: `cargo fmt --all`、`cargo clippy --workspace --all-targets --features sw-decode -- -D warnings`、`cargo test --workspace --features sw-decode` 全绿才提交
- 工作区里 rtp_send.rs 有用户的半成品草图 (PacketSource trait / MediaSource enum / FileSource / CameraSource / GstPipeline 和 tags::Codec 的误导入), Task 1 整体替换掉, 不保留
- x264enc 属 plugins-ugly: CI 要加 `gstreamer1.0-plugins-ugly`, 否则测试没元件

---

### Task 1: 类型骨架 + 视频文件轨重编码链

**Files:**
- Modify: `crates/ipcam-gst/src/rtp_send.rs` (整体重构)
- Modify: `crates/ipcam-sip/examples/call.rs` (适配新配置类型, 保编译)
- Modify: `.github/workflows/rust.yml` (CI 加 plugins-ugly)

**Interfaces:**
- Produces (后续任务依赖):
  - `pub enum TrackSource { File(PathBuf), Rtsp { uri: String }, LocalCamera { device: Option<String> }, Mic }`
  - `pub struct RtpSendConfig { pub audio: Option<(TrackSource, AudioDest)>, pub video: Option<(TrackSource, RtpDest)> }`
  - `enum TrackKind { Audio, Video }` (模块内私有)
  - `fn build_video_track(pipeline: &gst::Pipeline, source: &TrackSource, dest: RtpDest, stats: Arc<SendStats>) -> Result<(), GstStreamError>`
  - `fn plug_uri_source(pipeline: &gst::Pipeline, uri: &str, kind: TrackKind, on_raw_pad: impl Fn(gst::Pad) -> Result<(), GstStreamError> + Send + 'static) -> Result<(), GstStreamError>`
  - `fn add_link_and_plug(...)` (现有, 保留不动)

- [ ] **Step 1: 写失败测试**

`crates/ipcam-gst/src/rtp_send.rs` 底部 tests 模块, 替换现有测试:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 文件源视频轨: 解码重编码后 3 秒内必须出 RTP 包
    #[test]
    fn video_file_track_emits_packets() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let sender = start_rtp_sender(RtpSendConfig {
            audio: None,
            video: Some((
                TrackSource::File(PathBuf::from("../../assets/oceans.mp4")),
                RtpDest {
                    addr: "127.0.0.1:40002".parse().unwrap(),
                    payload_type: 96,
                },
            )),
        })
        .expect("sender starts");
        std::thread::sleep(Duration::from_secs(3));
        let (video, _audio) = sender.packet_counts();
        sender.stop();
        assert!(video > 0, "no video RTP packets emitted");
    }
}
```

- [ ] **Step 2: 跑测试确认编译失败**

Run: `cargo test -p ipcam-gst video_file_track_emits_packets 2>&1 | tail -5`
Expected: FAIL (编译错误, TrackSource/RtpSendConfig 字段不存在; 且工作区草图里 `cfg.file` 引用已失效)

- [ ] **Step 3: 重构 rtp_send.rs**

整体替换文件内容 (删除 PacketSource trait / MediaSource / FileSource / CameraSource 草图, 删除 `gstreamer::ffi::GstPipeline` 和 `gstreamer::tags::Codec` 导入, 删除 qtdemux 相关逻辑)。保留不动: `RtpDest` / `AudioCodec` / `AudioDest` / `SendStats` / `RtpSender` / bus 线程 / stats 线程 / `make_udpsink` / `install_pkt_probe` / `add_link_and_plug`。改动部分:

模块文档注释改为:

```rust
//! 多源 → RTP 发送器 (SIP 通话联调用): 每路媒体独立指定源 (文件/RTSP/
//! 本机相机/麦克风), 解码后统一重编码发出.
//!
//! 链形 (每轨独立):
//!   视频: 源 → [解码] → queue → videoconvert → x264enc
//!         → rtph264pay(config-interval=1) → udpsink
//!   音频: 源 → [解码] → queue → audioconvert → audioresample
//!         → capsfilter(8kHz/mono) → alawenc|mulawenc
//!         → rtppcmapay|rtppcmupay → udpsink
//!
//! 设计约束 (门口机黑屏三轮修复的经验, 全部固化在发送链里):
//! - x264enc aud=false + option-string slice-max-size=1300: 无 AUD,
//!   每个 NAL 小于 MTU, mode 0 对端不需要认 FU-A 分片
//! - rtph264pay config-interval=1: SPS/PPS 秒级周期重发, 对端中途
//!   开始收也能解
//! - udpsink sync=true: 按 buffer 时间戳限速, 否则以最快速度泼出去
//! - 发送 pt/音频编码必须取对端 answer 里的值 (RFC 3264)
```

类型定义 (替换草图):

```rust
/// 一路媒体的来源
#[derive(Debug, Clone)]
pub enum TrackSource {
    /// 文件 (mp4/wav/mp3 均可, 只取当前需要的轨)
    File(PathBuf),
    /// RTSP 网络相机 (凭据内嵌在 uri 里, H264/H265 均可)
    Rtsp { uri: String },
    /// 本机相机: Linux v4l2src (USB 和 MIPI/CSI 同元件), Windows ksvideosrc
    LocalCamera { device: Option<String> },
    /// 本机麦克风: Windows wasapisrc, Linux alsasrc
    Mic,
}

/// 要建的是哪一路 (源段按它匹配 uridecodebin 的裸流 pad)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackKind {
    Audio,
    Video,
}

#[derive(Debug, Clone)]
pub struct RtpSendConfig {
    /// None = 不发这路 (对端没接这路媒体时)
    pub audio: Option<(TrackSource, AudioDest)>,
    pub video: Option<(TrackSource, RtpDest)>,
}
```

`start_rtp_sender` 改为:

```rust
pub fn start_rtp_sender(cfg: RtpSendConfig) -> Result<RtpSender, GstStreamError> {
    ensure_init_internal()?;
    if cfg.audio.is_none() && cfg.video.is_none() {
        return Err(GstStreamError::InvalidConfig(
            "neither audio nor video destination given".into(),
        ));
    }

    let pipeline = gst::Pipeline::new();
    let stats = Arc::new(SendStats::default());
    let stop_flag = Arc::new(AtomicBool::new(false));

    if let Some((source, dest)) = &cfg.video {
        build_video_track(&pipeline, source, *dest, stats.clone())?;
    }
    if let Some((source, dest)) = &cfg.audio {
        build_audio_track(&pipeline, source, *dest, stats.clone())?;
    }

    // bus 线程 / stats 线程 / set Playing / info! 日志, 与现状一致
    // (日志里 file= 改为 audio=?/video=? 两个来源)
    // ...
    Ok(RtpSender { pipeline, stop_flag, stats })
}
```

源段 (uridecodebin, 文件和 RTSP 共用; Task 3 才接 Rtsp 分支, 本任务只接 File):

```rust
/// 文件/RTSP 源: uridecodebin 解码出裸流, pad-added 里按 TrackKind 匹配,
/// 命中的 pad 交给 on_raw_pad 接发送链. 源里没有请求的轨 = pad 永远不来,
/// 不阻塞另一路 (靠 stats 0 包发现)
fn plug_uri_source(
    pipeline: &gst::Pipeline,
    uri: &str,
    kind: TrackKind,
    on_raw_pad: impl Fn(gst::Pad) -> Result<(), GstStreamError> + Send + 'static,
) -> Result<(), GstStreamError> {
    let dec = make("uridecodebin")?;
    dec.set_property("uri", uri);
    pipeline
        .add(&dec)
        .map_err(|e| GstStreamError::Init(format!("add uridecodebin: {e}")))?;
    dec.connect_pad_added(move |_dec, pad| {
        let Some(caps) = pad.current_caps() else { return };
        let Some(s) = caps.structure(0) else { return };
        let hit = matches!(
            (kind, s.name().as_str()),
            (TrackKind::Video, "video/x-raw") | (TrackKind::Audio, "audio/x-raw")
        );
        if hit {
            if let Err(e) = on_raw_pad(pad.clone()) {
                warn!(error = %e, "rtp sender: failed to link send chain");
            }
        }
    });
    dec.sync_state_with_parent()
        .map_err(|e| GstStreamError::Init(format!("sync uridecodebin: {e}")))?;
    Ok(())
}

fn file_uri(path: &std::path::Path) -> Result<String, GstStreamError> {
    let abs = path.canonicalize().map_err(|e| {
        GstStreamError::InvalidConfig(format!("media file not found: {}: {e}", path.display()))
    })?;
    Ok(glib::filename_to_uri(abs, None).to_string())
}
```

视频轨:

```rust
fn build_video_track(
    pipeline: &gst::Pipeline,
    source: &TrackSource,
    dest: RtpDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    match source {
        TrackSource::File(path) => {
            let uri = file_uri(path)?;
            let pipeline = pipeline.clone();
            plug_uri_source(&pipeline, &uri, TrackKind::Video, move |pad| {
                link_video_send_chain(&pipeline, &pad, dest, stats.clone())
            })
        }
        other => Err(GstStreamError::InvalidConfig(format!(
            "video source not wired yet: {other:?}"
        ))),
    }
}

/// 视频发送链 (所有源共用): 裸流重编码, 参数集周期重发
fn link_video_send_chain(
    pipeline: &gst::Pipeline,
    raw_pad: &gst::Pad,
    dest: RtpDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    let queue = make("queue")?;
    let convert = make("videoconvert")?;
    let enc = make("x264enc")?;
    enc.set_property_from_str("tune", "zerolatency");
    enc.set_property_from_str("speed-preset", "veryfast");
    enc.set_property("bitrate", 400u32); // kbps, CIF 档足够
    enc.set_property("key-int-max", 30u32); // 秒级 IDR, 对端中途收也能起
    // 不插 AUD NAL (设备解析器不认); 每个 NAL 切到 MTU 以下,
    // 从源头消除 FU-A 分片需求 (mode 0)
    enc.set_property("aud", false);
    enc.set_property("option-string", "slice-max-size=1300");
    let pay = make("rtph264pay")?;
    pay.set_property("pt", dest.payload_type as u32);
    pay.set_property("config-interval", 1i32);
    install_pkt_probe(&pay, stats.clone(), false)?;
    let sink = make_udpsink(dest)?;
    add_link_and_plug(pipeline, raw_pad, &[&queue, &convert, &enc, &pay, &sink], "video")
}
```

`add_link_and_plug` 的 `demux_pad` 参数改名为 `src_pad` (语义泛化), 实现不变。

- [ ] **Step 4: 适配 call.rs 保编译**

`crates/ipcam-sip/examples/call.rs` 里 `start_rtp_sender` 调用改为:

```rust
let file = TrackSource::File(PathBuf::from("assets/oceans.mp4"));
let sender = ipcam_gst::start_rtp_sender(ipcam_gst::RtpSendConfig {
    audio: audio.map(|d| (file.clone(), d)),
    video: peers.video.map(|p| (file, ipcam_gst::RtpDest {
        addr: p.addr,
        payload_type: p.payload_type,
    })),
})?;
```

(`use ipcam_gst::TrackSource;`, 删掉原 `file` 变量; AudioDest 构建逻辑不动。`glib` 若在 ipcam-gst 未直接依赖, 用 `gst::glib`。)

- [ ] **Step 5: CI 加 plugins-ugly**

`.github/workflows/rust.yml` 的 apt install 行追加 `gstreamer1.0-plugins-ugly` (x264enc 所在)。

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo test -p ipcam-gst video_file_track_emits_packets 2>&1 | tail -5`
Expected: PASS, video_pkts > 0

- [ ] **Step 7: 门禁 + 提交**

Run: fmt + clippy + 全 workspace test 全绿后:

```bash
git add crates/ipcam-gst/src/rtp_send.rs crates/ipcam-sip/examples/call.rs .github/workflows/rust.yml
git commit -m "[重构] rtp_send: 按轨 TrackSource 抽象 + 视频轨统一 x264enc 重编码"
```

---

### Task 2: 音频文件轨 + 双轨 loopback 测试

**Files:**
- Modify: `crates/ipcam-gst/src/rtp_send.rs`

**Interfaces:**
- Consumes: `TrackSource` / `RtpSendConfig` / `plug_uri_source` / `add_link_and_plug` (Task 1)
- Produces: `fn build_audio_track(pipeline: &gst::Pipeline, source: &TrackSource, dest: AudioDest, stats: Arc<SendStats>) -> Result<(), GstStreamError>`、`fn link_audio_send_chain(pipeline, raw_pad, dest, stats) -> Result<(), GstStreamError>`

- [ ] **Step 1: 写失败测试**

tests 模块追加 (即原 `sender_actually_emits_packets` 的新形态):

```rust
/// 双轨都来自同一文件: 两路都必须出包 (文件会被解码两次, 联调工具不做共享)
#[test]
fn av_file_tracks_emit_packets() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let file = TrackSource::File(PathBuf::from("../../assets/oceans.mp4"));
    let sender = start_rtp_sender(RtpSendConfig {
        audio: Some((
            file.clone(),
            AudioDest {
                addr: "127.0.0.1:40000".parse().unwrap(),
                payload_type: 8,
                codec: AudioCodec::Pcma,
            },
        )),
        video: Some((
            file,
            RtpDest {
                addr: "127.0.0.1:40002".parse().unwrap(),
                payload_type: 96,
            },
        )),
    })
    .expect("sender starts");
    std::thread::sleep(Duration::from_secs(3));
    let (video, audio) = sender.packet_counts();
    sender.stop();
    assert!(video > 0, "no video RTP packets emitted");
    assert!(audio > 0, "no audio RTP packets emitted");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p ipcam-gst av_file_tracks_emit_packets 2>&1 | tail -5`
Expected: FAIL (`build_audio_track` 的 File 分支未接)

- [ ] **Step 3: 实现音频轨**

```rust
fn build_audio_track(
    pipeline: &gst::Pipeline,
    source: &TrackSource,
    dest: AudioDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    match source {
        TrackSource::File(path) => {
            let uri = file_uri(path)?;
            let pipeline = pipeline.clone();
            plug_uri_source(&pipeline, &uri, TrackKind::Audio, move |pad| {
                link_audio_send_chain(&pipeline, &pad, dest, stats.clone())
            })
        }
        other => Err(GstStreamError::InvalidConfig(format!(
            "audio source not wired yet: {other:?}"
        ))),
    }
}

/// 音频发送链 (所有源共用): 重采样到 8kHz 单声道, 按 answer 选 G.711
fn link_audio_send_chain(
    pipeline: &gst::Pipeline,
    raw_pad: &gst::Pad,
    dest: AudioDest,
    stats: Arc<SendStats>,
) -> Result<(), GstStreamError> {
    let queue = make("queue")?;
    let convert = make("audioconvert")?;
    let resample = make("audioresample")?;
    let caps = make("capsfilter")?;
    caps.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("rate", 8000i32)
            .field("channels", 1i32)
            .build(),
    );
    let (enc_name, pay_name) = dest.codec.elements();
    let enc = make(enc_name)?;
    let pay = make(pay_name)?;
    pay.set_property("pt", dest.payload_type as u32);
    install_pkt_probe(&pay, stats.clone(), true)?;
    let sink = make_udpsink(RtpDest {
        addr: dest.addr,
        payload_type: dest.payload_type,
    })?;
    add_link_and_plug(
        pipeline,
        raw_pad,
        &[&queue, &convert, &resample, &caps, &enc, &pay, &sink],
        "audio",
    )
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p ipcam-gst 2>&1 | grep "test result"`
Expected: 两个测试都 PASS

- [ ] **Step 5: 门禁 + 提交**

```bash
git add crates/ipcam-gst/src/rtp_send.rs
git commit -m "[功能] rtp_send: 音频文件轨接入, 双轨 loopback 测试"
```

---

### Task 3: RTSP 源

**Files:**
- Modify: `crates/ipcam-gst/src/rtp_send.rs`

**Interfaces:**
- Consumes: `plug_uri_source` / `build_video_track` / `build_audio_track` (Task 1/2)
- Produces: `TrackSource::Rtsp { uri }` 在两条 build_*_track 里可用

- [ ] **Step 1: 接 Rtsp 分支**

`build_video_track` / `build_audio_track` 的 match 各加一个分支:

```rust
TrackSource::Rtsp { uri } => {
    let uri = uri.clone();
    let pipeline = pipeline.clone();
    plug_uri_source(&pipeline, &uri, TrackKind::Video /* 音频轨为 Audio */, move |pad| {
        link_video_send_chain(&pipeline, &pad, dest, stats.clone())
        /* 音频轨为 link_audio_send_chain */
    })
}
```

- [ ] **Step 2: 验证编译 + 全量测试**

Run: `cargo clippy -p ipcam-gst --all-targets -- -D warnings && cargo test -p ipcam-gst 2>&1 | grep "test result"`
Expected: 全绿 (RTSP 无 CI 相机, 不加自动测试; 手动: `cargo run --example rawtap` 或后续 call 联调验证)

- [ ] **Step 3: 提交**

```bash
git add crates/ipcam-gst/src/rtp_send.rs
git commit -m "[功能] rtp_send: RTSP 相机源 (uridecodebin 同路径)"
```

---

### Task 4: 本机设备源 (LocalCamera / Mic) 平台分发

**Files:**
- Modify: `crates/ipcam-gst/src/rtp_send.rs`

**Interfaces:**
- Consumes: `link_video_send_chain` / `link_audio_send_chain` (Task 1/2)
- Produces: `fn camera_src_name() -> &'static str`、`fn mic_src_name() -> &'static str`、`fn plug_device_source(pipeline, element_name, device: Option<&str>, on_raw_pad) -> Result<(), GstStreamError>`

- [ ] **Step 1: 实现**

```rust
/// 平台分发集中在这两个函数, 不用 #[cfg] 散布.
/// 未知平台编译能过, 运行时 make() 报元件不存在 (与缺插件行为一致)
fn camera_src_name() -> &'static str {
    if cfg!(windows) { "ksvideosrc" } else { "v4l2src" }
}

fn mic_src_name() -> &'static str {
    if cfg!(windows) { "wasapisrc" } else { "alsasrc" }
}

/// 设备源出来直接是裸流 (或可被下游 negotiate 成裸流), src pad 静态存在
fn plug_device_source(
    pipeline: &gst::Pipeline,
    element_name: &str,
    device: Option<&str>,
    on_raw_pad: impl FnOnce(gst::Pad) -> Result<(), GstStreamError>,
) -> Result<(), GstStreamError> {
    let src = make(element_name)?;
    if let Some(dev) = device {
        // v4l2src 用 device=/dev/videoX; ksvideosrc 用 device-path.
        // MIPI 相机在 Linux 上同为 v4l2src, 只是节点不同
        let prop = if element_name == "ksvideosrc" { "device-path" } else { "device" };
        src.set_property(prop, dev);
    }
    pipeline
        .add(&src)
        .map_err(|e| GstStreamError::Init(format!("add {element_name}: {e}")))?;
    src.sync_state_with_parent()
        .map_err(|e| GstStreamError::Init(format!("sync {element_name}: {e}")))?;
    let pad = src
        .static_pad("src")
        .ok_or_else(|| GstStreamError::Link(format!("{element_name} has no src pad")))?;
    on_raw_pad(pad)
}
```

`build_video_track` 加分支:

```rust
TrackSource::LocalCamera { device } => {
    let pipeline = pipeline.clone();
    let device = device.clone();
    plug_device_source(&pipeline, camera_src_name(), device.as_deref(), move |pad| {
        link_video_send_chain(&pipeline, &pad, dest, stats.clone())
    })
}
```

`build_audio_track` 加分支:

```rust
TrackSource::Mic => {
    let pipeline = pipeline.clone();
    plug_device_source(&pipeline, mic_src_name(), None, move |pad| {
        link_audio_send_chain(&pipeline, &pad, dest, stats.clone())
    })
}
```

(`LocalCamera` 用于音频轨 / `Mic` 用于视频轨的组合: 落到 `other =>` 报 InvalidConfig。)

- [ ] **Step 2: 验证编译 + 全量测试 + 门禁**

Run: fmt + clippy + `cargo test --workspace --features sw-decode`
Expected: 全绿 (设备源无自动测试, CI 无摄像头/麦克风; Windows 手动: call example 用 camera/mic 源实机联调)

- [ ] **Step 3: 提交**

```bash
git add crates/ipcam-gst/src/rtp_send.rs
git commit -m "[功能] rtp_send: 本机相机/麦克风源, 平台元件分发"
```

---

### Task 5: call example 多源参数

**Files:**
- Modify: `crates/ipcam-gst/src/rtp_send.rs` (parse 函数 + 单测)
- Modify: `crates/ipcam-sip/examples/call.rs`

**Interfaces:**
- Consumes: `TrackSource` (Task 1)
- Produces: `pub fn parse_track_source(s: &str) -> Result<TrackSource, String>`

- [ ] **Step 1: 写失败测试**

rtp_send.rs tests 追加:

```rust
#[test]
fn parse_track_source_variants() {
    assert!(matches!(
        parse_track_source("file:a/b.mp4"),
        Ok(TrackSource::File(p)) if p == PathBuf::from("a/b.mp4")
    ));
    assert!(matches!(
        parse_track_source("rtsp://cam/1"),
        Ok(TrackSource::Rtsp { uri }) if uri == "rtsp://cam/1"
    ));
    assert!(matches!(parse_track_source("camera"), Ok(TrackSource::LocalCamera { device: None })));
    assert!(matches!(
        parse_track_source("camera:/dev/video1"),
        Ok(TrackSource::LocalCamera { device: Some(d) }) if d == "/dev/video1"
    ));
    assert!(matches!(parse_track_source("mic"), Ok(TrackSource::Mic)));
    assert!(parse_track_source("bogus").is_err());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p ipcam-gst parse_track_source 2>&1 | tail -3`
Expected: FAIL (函数不存在)

- [ ] **Step 3: 实现**

```rust
/// CLI 源描述解析: file:<路径> | rtsp://<uri> | camera[:<设备>] | mic
pub fn parse_track_source(s: &str) -> Result<TrackSource, String> {
    if let Some(path) = s.strip_prefix("file:") {
        return Ok(TrackSource::File(PathBuf::from(path)));
    }
    if s.starts_with("rtsp://") {
        return Ok(TrackSource::Rtsp { uri: s.to_string() });
    }
    if s == "camera" {
        return Ok(TrackSource::LocalCamera { device: None });
    }
    if let Some(dev) = s.strip_prefix("camera:") {
        return Ok(TrackSource::LocalCamera { device: Some(dev.to_string()) });
    }
    if s == "mic" {
        return Ok(TrackSource::Mic);
    }
    Err(format!("unknown track source: {s} (file:/rtsp://camera/mic)"))
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p ipcam-gst parse_track_source 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: call.rs 接参数**

clap 加两个参数 (默认值 = 现行为):

```rust
/// 视频源: file:<路径> | rtsp://<uri> | camera[:<设备>]
#[arg(long, default_value = "file:assets/oceans.mp4")]
video_src: String,
/// 音频源: file:<路径> | mic
#[arg(long, default_value = "file:assets/oceans.mp4")]
audio_src: String,
```

`call_until_hangup` 改签名接收两个 `TrackSource`, `start_rtp_sender` 调用改为:

```rust
let sender = ipcam_gst::start_rtp_sender(ipcam_gst::RtpSendConfig {
    audio: audio.map(|d| (audio_src.clone(), d)),
    video: peers.video.map(|p| (video_src.clone(), ipcam_gst::RtpDest {
        addr: p.addr,
        payload_type: p.payload_type,
    })),
})?;
```

main 里 `parse_track_source(&args.video_src).map_err(anyhow::Error::msg)?`, 音频同理。

- [ ] **Step 6: 门禁 + 全量测试 + 提交**

Run: fmt + clippy + `cargo test --workspace --features sw-decode` 全绿:

```bash
git add crates/ipcam-gst/src/rtp_send.rs crates/ipcam-sip/examples/call.rs
git commit -m "[功能] call example: --video-src/--audio-src 多源参数"
```

---

### Task 6: 文档收尾

**Files:**
- Modify: `docs/pitfalls.md` (若联调中发现新坑才加, 否则不动)
- Modify: `docs/superpowers/specs/2026-09-20-rtp-send-multi-source-design.md` 状态改为已实现

- [ ] **Step 1: spec 状态更新 + 提交**

```bash
git commit -m "[文档] 多输入源 spec 标记已实现"
```

- [ ] **Step 2: 手动联调清单 (用户实机执行)**

- Windows: `cargo run --example call -- 1000 --video-src camera --audio-src mic` (笔记本摄像头 + 麦克风打门口机)
- Windows: `--video-src rtsp://<相机>/ch01 --audio-src mic`
- 板子 (后续): v4l2src USB 相机; MIPI 相机待带相机硬件
