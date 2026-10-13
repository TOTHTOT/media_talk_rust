# call-ui 健康度优化 + ipcam-sip 收口 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把散在 call-ui(session.rs/media.rs) 和 examples/answer.rs 的三份 SDP 编码挑选拷贝收口进 ipcam-sip::negotiate, 顺带修 build_av_answer m-line 不裁剪的既有 bug 与 process_register 吞错误, 再对 call-ui 做函数级拆分, repowise health 复测 session.rs ≥8.5。

**Architecture:** ipcam-sip 新增 negotiate 模块 (谓词化 pick + 按 offer 裁剪的 build_answer), 不反向依赖 ipcam-gst; call-ui/answer.rs 改为传入谓词调用。随后纯代码移动式拆分 session.rs 的 run/incoming_inner/handle_cmd 与 lib.rs 的 run。

**Tech Stack:** Rust, sdp-rs, rsipstack, GStreamer (不涉及管线改动), Slint (不涉及 UI 改动)

## Global Constraints

- 每条 cargo 命令前缀:
  `export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH" && export PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"`
- 每个任务收尾门禁: `cargo fmt --all` + `cargo clippy --workspace --all-targets --features sw-decode -- -D warnings` + `cargo test --workspace --features sw-decode`, 全绿才提交
- 提交消息中文前缀 `[新增]/[修复]/[重构]/[测试]/[文档]`; **只 commit 不 push**
- 库代码禁 `println!`/`unwrap`/`expect` (测试代码可用); 日志只用 tracing; 注释中文
- 新 pub 函数遵守 CLAUDE.md §2 注释模板 (`# Arguments` 参数名+冒号+作用, 空 `# Examples`); `#[allow]` 必须带 `// reason:` 内联注释
- 设计文档: `docs/superpowers/specs/2026-10-13-call-ui-health-design.md`

---

### Task 1: ipcam-sip negotiate 模块

**Files:**
- Create: `crates/ipcam-sip/src/negotiate.rs`
- Modify: `crates/ipcam-sip/src/lib.rs` (加 `pub mod negotiate;`)
- Modify: `crates/ipcam-sip/src/sdp.rs` (三个私有函数改 `pub(crate)`)

**Interfaces:**
- Consumes: `sdp::{OfferMedias, OfferMedia, OfferCodec, session_skeleton, audio_media_description, video_media_description, build_av_answer}`; `ipcam_core::{AudioCodec, VideoCodec}`
- Produces (Task 3/4 依赖):
  - `pub type NegotiatedAnswer = (Option<(u8, AudioCodec)>, Option<(u8, VideoCodec, Option<String>)>, String);`
  - `pub fn pick_audio(offer: &OfferMedias, pred: impl Fn(&str) -> Option<AudioCodec>) -> Option<(u8, AudioCodec)>`
  - `pub fn pick_video(offer: &OfferMedias, pred: impl Fn(&str) -> Option<VideoCodec>) -> Option<(u8, VideoCodec, Option<String>)>`
  - `pub fn build_answer(offer: &OfferMedias, local_ip: IpAddr, audio_port: u16, video_port: u16, audio_pred: impl Fn(&str) -> Option<AudioCodec>, video_pred: impl Fn(&str) -> Option<VideoCodec>) -> NegotiatedAnswer`

- [ ] **Step 1: sdp.rs 私有函数改 pub(crate)**

`crates/ipcam-sip/src/sdp.rs` 中 `fn session_skeleton`、`fn audio_media_description`、`fn video_media_description` 三个改为 `pub(crate) fn`。`build_av_answer` 保持 `pub` 不动 (Task 1 的回归测试要靠它对比, 之后作为便捷 API 保留)。

- [ ] **Step 2: 写失败测试 (negotiate.rs 先只有函数签名+todo!() 或空 mod)**

`crates/ipcam-sip/src/negotiate.rs` 的测试模块:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdp::{build_av_answer, parse_offer_all};
    use std::net::Ipv4Addr;

    /// 全音视频 offer: 音频 [opus,PCMU,PCMA], 视频 [VP8,H264(fmtp)]
    const AV_OFFER: &str = concat!(
        "v=0\r\n",
        "o=- 1 1 IN IP4 192.168.1.50\r\n",
        "s=-\r\n",
        "c=IN IP4 192.168.1.50\r\n",
        "t=0 0\r\n",
        "m=audio 30000 RTP/AVP 111 0 8\r\n",
        "a=rtpmap:111 opus/48000/2\r\n",
        "a=rtpmap:0 PCMU/8000\r\n",
        "a=rtpmap:8 PCMA/8000\r\n",
        "m=video 30002 RTP/AVP 96 97\r\n",
        "a=rtpmap:96 VP8/90000\r\n",
        "a=rtpmap:97 H264/90000\r\n",
        "a=fmtp:97 profile-level-id=42801F\r\n",
    );
    /// 仅音频 offer (老门禁无摄像头场景)
    const AUDIO_ONLY_OFFER: &str = concat!(
        "v=0\r\n",
        "o=- 1 1 IN IP4 192.168.1.50\r\n",
        "s=-\r\n",
        "c=IN IP4 192.168.1.50\r\n",
        "t=0 0\r\n",
        "m=audio 30000 RTP/AVP 0\r\n",
        "a=rtpmap:0 PCMU/8000\r\n",
    );

    fn g711_pred(name: &str) -> Option<AudioCodec> {
        match name {
            "PCMU" => Some(AudioCodec::G711U),
            "PCMA" => Some(AudioCodec::G711A),
            _ => None,
        }
    }
    fn h26x_pred(name: &str) -> Option<VideoCodec> {
        match VideoCodec::from_name(name) {
            vc @ (VideoCodec::H264 | VideoCodec::H265) => Some(vc),
            _ => None,
        }
    }
    fn offer(s: &str) -> OfferMedias {
        parse_offer_all(s.as_bytes()).expect("offer must parse")
    }
    fn local() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
    }

    /// 按对端偏好序挑第一个谓词接受的: opus 打头跳过选 PCMU
    #[test]
    fn pick_audio_prefers_offer_order() {
        assert_eq!(
            pick_audio(&offer(AV_OFFER), g711_pred),
            Some((0, AudioCodec::G711U))
        );
    }

    #[test]
    fn pick_audio_none_when_all_unsupported() {
        assert_eq!(pick_audio(&offer(AV_OFFER), |_| None), None);
    }

    /// 视频跳过 VP8 选 H264, fmtp 随选中项回显
    #[test]
    fn pick_video_skips_vp8_and_echoes_fmtp() {
        let picked = pick_video(&offer(AV_OFFER), h26x_pred);
        assert_eq!(
            picked,
            Some((97, VideoCodec::H264, Some("profile-level-id=42801F".into())))
        );
    }

    /// 回归钉死: 两路都选中时与旧 build_av_answer 逐字节一致 (互操作不变)
    #[test]
    fn build_answer_full_av_matches_legacy() {
        let simple = offer(concat!(
            "v=0\r\n",
            "o=- 1 1 IN IP4 192.168.1.50\r\n",
            "s=-\r\n",
            "c=IN IP4 192.168.1.50\r\n",
            "t=0 0\r\n",
            "m=audio 30000 RTP/AVP 0\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
            "m=video 30002 RTP/AVP 96\r\n",
            "a=rtpmap:96 H264/90000\r\n",
            "a=fmtp:96 profile-level-id=42e01f\r\n",
        ));
        let (_, _, answer) = build_answer(&simple, local(), 40000, 40002, g711_pred, h26x_pred);
        let legacy = build_av_answer(local(), 40000, 40002, 0, 96, Some("profile-level-id=42e01f"));
        assert_eq!(answer, legacy.to_string());
    }

    /// offer 没有视频路 → answer 不带 m=video (旧行为会无中生有)
    #[test]
    fn build_answer_audio_only_offer_omits_video() {
        let (_, _, answer) =
            build_answer(&offer(AUDIO_ONLY_OFFER), local(), 40000, 40002, g711_pred, h26x_pred);
        assert!(!answer.contains("m=video"), "answer:\n{answer}");
        assert!(answer.contains("m=audio 40000 RTP/AVP 0"), "answer:\n{answer}");
    }

    /// offer 有音频路但全不支持 → 该路端口置 0 (RFC 3264 拒路)
    #[test]
    fn build_answer_rejects_unsupported_audio_with_port_zero() {
        let o = offer(concat!(
            "v=0\r\n",
            "o=- 1 1 IN IP4 192.168.1.50\r\n",
            "s=-\r\n",
            "c=IN IP4 192.168.1.50\r\n",
            "t=0 0\r\n",
            "m=audio 30000 RTP/AVP 111\r\n",
            "a=rtpmap:111 opus/48000/2\r\n",
        ));
        let (audio, _, answer) = build_answer(&o, local(), 40000, 40002, g711_pred, h26x_pred);
        assert_eq!(audio, None);
        assert!(answer.contains("m=audio 0 RTP/AVP 111"), "answer:\n{answer}");
    }

    /// 视频全不支持同样置 0; 音频正常选中
    #[test]
    fn build_answer_rejects_unsupported_video_with_port_zero() {
        let (audio, video, answer) =
            build_answer(&offer(AV_OFFER), local(), 40000, 40002, g711_pred, |_| None);
        assert_eq!(audio, Some((0, AudioCodec::G711U)));
        assert_eq!(video, None);
        assert!(answer.contains("m=video 0 RTP/AVP 96"), "answer:\n{answer}");
    }
}
```

先只建含测试和 `todo!()` 实现的 negotiate.rs + lib.rs 挂 mod, 跑:

Run: `cargo test -p ipcam-sip negotiate 2>&1 | tail -5`
Expected: 编译失败或测试 panic (todo), 确认测试真的在跑

- [ ] **Step 3: 实现 negotiate.rs**

```rust
//! SDP 编码协商: 按 offer 偏好序挑选本端支持的编码并构造 answer.
//! 从 call-ui (session.rs/media.rs) 和 examples/answer.rs 的三份逐字
//! 拷贝收口而来; 支持什么编码由调用方以谓词传入 (ipcam-sip 不反向
//! 依赖 ipcam-gst, 分层不破).

use std::net::IpAddr;

use ipcam_core::{AudioCodec, VideoCodec};
use sdp_rs::MediaDescription;
use sdp_rs::lines::attribute::Rtpmap;
use sdp_rs::lines::media::{Media, MediaType, ProtoType};
use sdp_rs::lines::Attribute;
use tracing::warn;

use crate::sdp::{
    OfferMedia, OfferMedias, audio_media_description, session_skeleton,
    video_media_description,
};

/// 协商结果: (选中的音频 pt+编码, 选中的视频 pt+编码+fmtp, answer 文本)
pub type NegotiatedAnswer = (
    Option<(u8, AudioCodec)>,
    Option<(u8, VideoCodec, Option<String>)>,
    String,
);

/// 音频按对端 offer 的偏好序挑第一个谓词接受的编码; 全不匹配返回 None.
///
/// # Arguments
///
/// * `offer`: 解析后的对端 offer
/// * `pred`: 编码名 → 本端支持则 Some(AudioCodec), 不支持 None
///
/// returns: Option<(u8, AudioCodec)> — (offer 里的 pt, 编码)
///
/// # Examples
///
/// ```
///
/// ```
pub fn pick_audio(
    offer: &OfferMedias,
    pred: impl Fn(&str) -> Option<AudioCodec>,
) -> Option<(u8, AudioCodec)> {
    let a = offer.audio.as_ref()?;
    let picked = a
        .codecs
        .iter()
        .find_map(|c| pred(&c.codec).map(|ac| (c.payload_type, ac)));
    if picked.is_none() {
        warn!(codecs = ?a.codecs, "no supported audio codec in offer");
    }
    picked
}

/// 视频同上, 多带回选中编码的 fmtp (answer 要回显, 见 build_answer).
///
/// # Arguments
///
/// * `offer`: 解析后的对端 offer
/// * `pred`: 编码名 → 本端支持则 Some(VideoCodec), 不支持 None
///
/// returns: Option<(u8, VideoCodec, Option<String>)> — (pt, 编码, fmtp)
///
/// # Examples
///
/// ```
///
/// ```
pub fn pick_video(
    offer: &OfferMedias,
    pred: impl Fn(&str) -> Option<VideoCodec>,
) -> Option<(u8, VideoCodec, Option<String>)> {
    let v = offer.video.as_ref()?;
    let picked = v
        .codecs
        .iter()
        .find_map(|c| pred(&c.codec).map(|vc| (c.payload_type, vc, c.fmtp.clone())));
    if picked.is_none() {
        warn!(codecs = ?v.codecs, "no supported video codec in offer");
    }
    picked
}

/// 从 offer 协商编码并构建 answer SDP (初始 accept 和 re-INVITE 共用).
/// 按 offer 裁剪 m-line: offer 没有的媒体路 answer 不带; offer 有但
/// 谓词全不匹配的路端口置 0 (RFC 3264 拒路, 明确告知拒收); 两路都
/// 选中时输出与 build_av_answer 逐字节一致 (互操作不变, 有测试钉死).
/// answer 的 pt/fmtp 回显 offer 里选中的编码 (RFC 3264).
///
/// # Arguments
///
/// * `offer`: 解析后的对端 offer
/// * `local_ip`: 本机 IP (answer 的 o=/c= 地址)
/// * `audio_port`: 本机音频 RTP 接收端口
/// * `video_port`: 本机视频 RTP 接收端口
/// * `audio_pred`: 音频编码谓词
/// * `video_pred`: 视频编码谓词
///
/// returns: NegotiatedAnswer — (音频选中, 视频选中, answer 文本)
///
/// # Examples
///
/// ```
///
/// ```
pub fn build_answer(
    offer: &OfferMedias,
    local_ip: IpAddr,
    audio_port: u16,
    video_port: u16,
    audio_pred: impl Fn(&str) -> Option<AudioCodec>,
    video_pred: impl Fn(&str) -> Option<VideoCodec>,
) -> NegotiatedAnswer {
    let audio = pick_audio(offer, audio_pred);
    let video = pick_video(offer, video_pred);
    // answer 的 sess_version 要大于 offer 的 ("1"), 同 build_audio_answer
    let mut sdp = session_skeleton(local_ip, "2");
    if let Some(oa) = &offer.audio {
        match audio {
            Some((pt, _)) => sdp
                .media_descriptions
                .push(audio_media_description(audio_port, &[pt])),
            None => sdp
                .media_descriptions
                .push(rejected_media(MediaType::Audio, oa)),
        }
    }
    if let Some(ov) = &offer.video {
        match &video {
            Some((pt, _, fmtp)) => sdp.media_descriptions.push(video_media_description(
                video_port,
                *pt,
                fmtp.as_deref(),
            )),
            None => sdp
                .media_descriptions
                .push(rejected_media(MediaType::Video, ov)),
        }
    }
    (audio, video, sdp.to_string())
}

/// RFC 3264 拒路: 端口置 0, fmt 回显 offer 第一个 pt (codecs 非空由
/// parse_fmt_codecs 保证). 不带 sendrecv — 拒收的路不声明方向
fn rejected_media(ty: MediaType, offered: &OfferMedia) -> MediaDescription {
    let first = &offered.codecs[0];
    MediaDescription {
        media: Media {
            media: ty,
            port: 0,
            num_of_ports: None,
            proto: ProtoType::RtpAvp,
            fmt: first.payload_type.to_string(),
        },
        info: None,
        connections: vec![],
        bandwidths: vec![],
        key: None,
        attributes: vec![Attribute::Rtpmap(Rtpmap {
            payload_type: first.payload_type as u32,
            encoding_name: first.codec.clone(),
            clock_rate: first.clock_rate,
            encoding_params: first.encoding_params,
        })],
    }
}
```

注意 `Rtpmap.clock_rate` 的类型: sdp.rs 里是 `i32`, `OfferCodec.clock_rate` 也是 `i32`, 直接赋值即可; 若编译器报类型不符按编译器提示调整。

- [ ] **Step 4: 跑测试确认全过**

Run: `cargo test -p ipcam-sip`
Expected: negotiate 7 个测试 + 既有测试全 PASS

- [ ] **Step 5: 门禁 + 提交**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets --features sw-decode -- -D warnings && cargo test --workspace --features sw-decode`

```bash
git add crates/ipcam-sip/src/negotiate.rs crates/ipcam-sip/src/lib.rs crates/ipcam-sip/src/sdp.rs
git commit -m "[新增] ipcam-sip: negotiate 模块 — 谓词化编码挑选 + 按 offer 裁剪的 build_answer"
```

---

### Task 2: process_register 透出永久失败原因

**Files:**
- Modify: `crates/ipcam-sip/src/client.rs:181-184` (Err 分支) 和 `:451` 附近测试

**Interfaces:**
- Consumes: 无新依赖
- Produces: `process_register` 永久失败时返回 `Err`(含 "register rejected permanently: 403 Forbidden" 式原因) — call-ui session.rs 的 reg 分支 (Task 3 顺手调整显示) 依赖此行为

- [ ] **Step 1: 改测试断言 (先红)**

`client.rs` 现有测试 (~:451) 断言永久失败返回 Ok, 改为:

```rust
        let result = client.process_register().await;
        let err = result.expect_err("permanent failure must propagate the reason");
        assert!(
            err.to_string().contains("403"),
            "reason must reach caller: {err}"
        );
```

(该测试的 fake_uas 返回的是 `rsip::StatusCode::Forbidden`; 若不是, 按实际状态码改断言。)

Run: `cargo test -p ipcam-sip` → Expected: 该测试 FAIL (当前返回 Ok)

- [ ] **Step 2: 改实现**

`process_register` 的 Err 分支 (`warn!` + `break`) 改为:

```rust
                Err(e) => {
                    warn!(error = %e, "registration permanently failed, exiting");
                    // 从未注册成功, 无需 best-effort 注销; 原因透传给调用方 —
                    // UI 错误条要看得到 401/403, 不能只留在日志里
                    return Err(e);
                }
```

- [ ] **Step 3: 检查其它调用方不回归**

`grep -rn "process_register" crates/ --include="*.rs"`: 各调用点 (call-ui session.rs、examples/*.rs) 都是对结果 log/展示, Err 现在带原因, 只改信息不控制流程, 无需改动 (call-ui 显示文案 Task 3 调)。

- [ ] **Step 4: 测试 + 门禁 + 提交**

Run: `cargo test -p ipcam-sip` → 全 PASS; 然后门禁三件套。

```bash
git add crates/ipcam-sip/src/client.rs
git commit -m "[修复] ipcam-sip: 注册永久失败原因透出给调用方 (UI 可见 401/403)"
```

---

### Task 3: call-ui 切共享 negotiate (删 session.rs/media.rs 拷贝)

**Files:**
- Modify: `crates/call-ui/src/session.rs`
- Modify: `crates/call-ui/src/media.rs`

**Interfaces:**
- Consumes: Task 1 的 `ipcam_sip::negotiate::{pick_audio, pick_video, build_answer}`; Task 2 的 process_register Err
- Produces: `crate::media::{audio_pred, video_pred}` (pub(crate) 谓词, Task 5 拆分后仍用)

- [ ] **Step 1: media.rs 加谓词 + 切共享 pick**

media.rs 删本地 `pick_audio`/`pick_video` (~:157-187), 新增:

```rust
/// 可收发音频谓词 (复用 ipcam-gst 发送白名单 G.711): session 协商与
/// 本模块指纹比对共用这一个, 防漂移
pub(crate) fn audio_pred(name: &str) -> Option<AudioCodec> {
    ipcam_gst::sendable_audio_codec(name)
}

/// 可收视频谓词: H264/H265
pub(crate) fn video_pred(name: &str) -> Option<VideoCodec> {
    match VideoCodec::from_name(name) {
        vc @ (VideoCodec::H264 | VideoCodec::H265) => Some(vc),
        _ => None,
    }
}
```

`renegotiate` 里两处调用改为 `ipcam_sip::negotiate::pick_audio(offer, audio_pred)` / `pick_video(offer, video_pred)` (顶部加 `use ipcam_sip::negotiate;`)。

`audio_send_params` 重写为:

```rust
fn audio_send_params(offer: &OfferMedias) -> Option<AudioDest> {
    let a = offer.audio.as_ref()?;
    negotiate::pick_audio(offer, audio_pred).map(|(pt, codec)| AudioDest {
        addr: SocketAddr::new(a.addr, a.port),
        payload_type: pt,
        codec,
    })
}
```

`video_send_params` 里 `pick_video(offer)` 改为 `negotiate::pick_video(offer, video_pred)`。

media.rs 测试里 `pick_audio(&offer)`/`pick_video(&offer)` 相应改为 `negotiate::pick_audio(&offer, audio_pred)` 等, 断言不变。

- [ ] **Step 2: session.rs 删拷贝 + 切 build_answer**

删 `NegotiatedAnswer` type、`pick_audio`、`pick_video`、`negotiate_answer` (~:730-784)。`use ipcam_sip::sdp::{...}` 移除 `build_av_answer` (其它保留: `PT_PCMU/PT_PCMA` 仍用于 build_av_offer), 加 `use ipcam_sip::negotiate;`。

`incoming_inner` 的 accept 段 (~:505):

```rust
    let (audio_pt, video, answer) = negotiate::build_answer(
        &offer,
        IpAddr::V4(local),
        AUDIO_RTP_PORT,
        VIDEO_RTP_PORT,
        media::audio_pred,
        media::video_pred,
    );
```

re-INVITE 臂 (~:542) 同样把 `negotiate_answer(&new_offer, ...)` 换成同一调用 (`&new_offer`)。

reg 分支 (~:211) 文案顺手改 (配合 Task 2):

```rust
            r = &mut reg => {
                // 注册循环退出 = 永久失败 (401/403/404, Err 带原因) 或被取消
                warn!(result = ?r, "register loop exited");
                let reason = match r {
                    Err(e) => format!("{e}"),
                    Ok(()) => "注册循环退出".into(),
                };
                ui.post(UiEvent::RegisterFailed(reason));
                reg_done = true;
                break;
            }
```

- [ ] **Step 3: 测试 + 门禁 + 提交**

Run: `cargo test -p call-ui` → 全 PASS; 门禁三件套。

```bash
git add crates/call-ui/src/session.rs crates/call-ui/src/media.rs
git commit -m "[重构] call-ui: 编码挑选切 ipcam-sip::negotiate, 删逐字拷贝两份"
```

---

### Task 4: examples/answer.rs 切共享 negotiate (删第三份拷贝)

**Files:**
- Modify: `crates/ipcam-sip/examples/answer.rs`

**Interfaces:**
- Consumes: Task 1 的 negotiate API; answer.rs 是 example, 可继续用 `ipcam_gst::sendable_audio_codec` 作谓词

- [ ] **Step 1: 删除本地 pick_audio/pick_video/negotiate_answer/NegotiatedAnswer, 全部调用点切换**

answer.rs 的 `negotiate_answer` (~:307) 调用处换成:

```rust
    let (audio_pt, video, answer) = ipcam_sip::negotiate::build_answer(
        offer,
        local_ip,
        AUDIO_RTP_PORT,
        VIDEO_RTP_PORT,
        |name| ipcam_gst::sendable_audio_codec(name),
        |name| match VideoCodec::from_name(name) {
            vc @ (VideoCodec::H264 | VideoCodec::H265) => Some(vc),
            _ => None,
        },
    );
```

(端口常量名以 answer.rs 现有为准; 若谓词闭包出现多处, 抽两个本地 `fn audio_pred/video_pred` 小函数避免重复闭包。)

其余 `pick_audio(`/`pick_video(`/`.find_map(|c| ipcam_gst::sendable_audio_codec...)` 调用点 (如 CallMedia::renegotiate、~:334 的发送参数) 一律换成 `ipcam_sip::negotiate::pick_audio(offer, audio_pred)` 形态。`build_av_answer` import 删掉。行为不变: answer 仍按选中编码回显 pt/fmtp。

- [ ] **Step 2: 编译 + 门禁 + 提交**

Run: `cargo build -p ipcam-sip --examples` + 门禁三件套。

```bash
git add crates/ipcam-sip/examples/answer.rs
git commit -m "[重构] ipcam-sip: answer example 切共享 negotiate, 删第三份拷贝"
```

---

### Task 5: session.rs 函数级拆分

**Files:**
- Modify: `crates/call-ui/src/session.rs`

**Interfaces:**
- Consumes: 无 (纯代码移动, 行为不变; 既有测试+门禁兜底)
- Produces: 无新公开接口

**拆分目标** (全部 `async fn`/`fn` 保持现有可见性, 逻辑逐字搬移):

1. `run` 拆出:
   - `async fn setup(cfg: &GuiSipConfig, ui: &UiHandle) -> Result<(SipClient, Credential, DialogStateSender, ..., Ipv4Addr)>` — :123-148 的网卡选择/client 构造/spawn_endpoint/状态通道建立; client 构造失败仍 post RegisterFailed 后返回 Err
   - `async fn shutdown(client: &SipClient, reg: ..., reg_done: bool, slot: &CallSlot)` — :238-254 的补 BYE + stop + 等注销。reg 是 `std::pin::Pin<Box<impl Future<Output = Result<()>>>>`, 签名写 `reg: std::pin::Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>` 即可 (调用处 `Box::pin(client.process_register())` 强转)
2. `incoming_inner` 拆出:
   - `enum Decision { Accept, Reject(rsip::StatusCode), PeerCancelled }` 提到函数外
   - `async fn wait_user_decision(dialog: &InviteDialog, ui: &UiHandle, caller: &str, ctl_rx: &mut mpsc::Receiver<CallCtl>) -> Result<Decision>` — ringing + post Incoming + 三选一 select (:461-483)
   - `async fn incoming_call_loop(dialog: &InviteDialog, media: &mut Option<media::MediaHandles>, ctl_rx: &mut mpsc::Receiver<CallCtl>, update_rx: &mut mpsc::Receiver<UpdateMsg>, local: Ipv4Addr) -> Result<String>` — :521-557 的通话 select 循环
   - accept 段 (parse offer + 488 兜底 + accept + post Confirmed, :495-514) 留在 incoming_inner 主流程 — 它只有 ~20 行且顺序强耦合, 不强行抽
3. `handle_cmd` Dial 臂拆出 `prepare_dial` (:286-348 的检查部分):

```rust
/// Dial 前置: 占线检查 + URI 解析 + 占位 socket 绑定, 全过才占槽返回
/// 通话资源; 任一步失败已报 UI, 返回 None
///
/// # Arguments
///
/// * `client`: SIP 客户端句柄
/// * `number`: 拨号号码
/// * `local`: 本机 IPv4
/// * `slot`: 通话槽位 (全过时在此占槽)
/// * `ui`: 事件回投句柄 (失败原因上报)
///
/// returns: Option<(被叫 URI, RTP 端口对, 占位 socket 对, 控制通道接收端)>
///
/// # Examples
///
/// ```
///
/// ```
fn prepare_dial(
    client: &SipClient,
    number: &str,
    local: Ipv4Addr,
    slot: &CallSlot,
    ui: &UiHandle,
) -> Option<(
    rsip::Uri,
    (u16, u16),
    (std::net::UdpSocket, std::net::UdpSocket),
    mpsc::Receiver<CallCtl>,
)>;
```

Dial 臂瘦身为: `let Some((callee, rtp_ports, rtp_sockets, ctl_rx)) = prepare_dial(...) else { return };` + 原有 tokio::spawn。

每步搬移后跑 `cargo test -p call-ui` 确认不回归。

- [ ] **Step 1: 拆 run (setup/shutdown)** → `cargo test -p call-ui` PASS
- [ ] **Step 2: 拆 incoming_inner (wait_user_decision/incoming_call_loop)** → `cargo test -p call-ui` PASS
- [ ] **Step 3: 拆 handle_cmd (prepare_dial)** → `cargo test -p call-ui` PASS
- [ ] **Step 4: 门禁三件套全绿后提交**

```bash
git add crates/call-ui/src/session.rs
git commit -m "[重构] call-ui: session.rs 函数级拆分 (setup/shutdown/决策/通话循环/拨号前置)"
```

---

### Task 6: lib.rs 回调装配 + receiver.rs 死调用清理

**Files:**
- Modify: `crates/call-ui/src/lib.rs`
- Modify: `crates/ipcam-gst/src/rtp_recv/receiver.rs:237-239`

**Interfaces:**
- Consumes/Produces: 无接口变化

- [ ] **Step 1: lib.rs 抽 wire_callbacks**

`run` 里四个回调注册块替换为 `wire_callbacks(&ui, &app, &cmd_tx);`, 新函数:

```rust
/// 装配 UI 回调: 四个回调同一形态 — 状态机决策出 Effect, 翻译成
/// worker 命令发出, 再把状态刷到界面属性
fn wire_callbacks(
    ui: &MainWindow,
    app: &bridge::SharedApp,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<bridge::WorkerCmd>,
) {
    /// 决策落地: 有 Effect 才发命令, 然后刷属性.
    /// 注意必须先 `let eff = app.lock().xxx();` 绑定再调本函数 —
    /// 锁守卫若作为实参表达式存活到调用结束, 这里再 lock 会死锁
    /// (parking_lot Mutex 不可重入)
    fn apply(
        weak: &slint::Weak<MainWindow>,
        app: &bridge::SharedApp,
        cmd_tx: &tokio::sync::mpsc::UnboundedSender<bridge::WorkerCmd>,
        eff: Option<app::Effect>,
    ) {
        if let Some(eff) = eff {
            let cmd = match eff {
                app::Effect::Dial(n) => bridge::WorkerCmd::Dial(n),
                app::Effect::Accept => bridge::WorkerCmd::Accept,
                app::Effect::Reject => bridge::WorkerCmd::Reject,
                app::Effect::Hangup => bridge::WorkerCmd::Hangup,
            };
            let _ = cmd_tx.send(cmd);
        }
        if let Some(ui) = weak.upgrade() {
            sync_props(&ui, &app.lock());
        }
    }

    {
        let (app, cmd_tx, weak) = (app.clone(), cmd_tx.clone(), ui.as_weak());
        ui.on_dial(move |number| {
            let eff = app.lock().dial(number.as_str().to_string());
            apply(&weak, &app, &cmd_tx, eff);
        });
    }
    {
        let (app, cmd_tx, weak) = (app.clone(), cmd_tx.clone(), ui.as_weak());
        ui.on_accept_call(move || {
            let eff = app.lock().accept();
            apply(&weak, &app, &cmd_tx, eff);
        });
    }
    {
        let (app, cmd_tx, weak) = (app.clone(), cmd_tx.clone(), ui.as_weak());
        ui.on_reject_call(move || {
            let eff = app.lock().reject();
            apply(&weak, &app, &cmd_tx, eff);
        });
    }
    {
        let (app, cmd_tx, weak) = (app.clone(), cmd_tx.clone(), ui.as_weak());
        ui.on_hangup(move || {
            let eff = app.lock().hangup();
            apply(&weak, &app, &cmd_tx, eff);
        });
    }
}
```

`run` 中原四个注册块删除, 换成一行调用; `smoke()` 不动。

- [ ] **Step 2: receiver.rs 删死调用**

删 `crates/ipcam-gst/src/rtp_recv/receiver.rs` 的 :238-239 两行:

```rust
    appsink.set_property("emit-signals", true);
    disable_sink_async(&appsink);
```

原位留注释: `// appsink 走 set_callbacks 不需要 emit-signals; 它也不是 Bin,
// disable_sink_async 对它 no-op — 两处死配置已删 (auto*sink 的那些是真的, 别误删)`

注意: 只删 appsink 这两行; autovideosink/autoaudiosink 上的 `disable_sink_async` 是活的 (它们是 Bin), 保留。

- [ ] **Step 3: 门禁 + 提交**

门禁三件套全绿后:

```bash
git add crates/call-ui/src/lib.rs crates/ipcam-gst/src/rtp_recv/receiver.rs
git commit -m "[重构] call-ui: 回调装配抽 wire_callbacks; ipcam-gst: 删 appsink 死配置"
```

---

### Task 7: 终验 (repowise 复测 + 收尾核对)

**Files:**
- Modify: `docs/superpowers/specs/2026-10-13-call-ui-health-design.md` (如有实现期偏离, 补记)

- [ ] **Step 1: 全量门禁**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets --features sw-decode -- -D warnings && cargo test --workspace --features sw-decode`
Expected: 全绿

- [ ] **Step 2: repowise 复测对比**

Run: `repowise health --module crates/call-ui --format table`
预期: session.rs ≥8.5, 模块无 fair 档; dry_violation 标记消失。把前后数字记录下来 (提交信息或汇报用)。若 session.rs 仍 <8.5, 看剩余标记是什么, 能顺手收的收掉, 收不动的如实记录原因。

- [ ] **Step 3: 检查无残留**

Run: `grep -rn "fn pick_audio\|fn pick_video\|fn negotiate_answer" crates/ --include="*.rs"`
Expected: 只剩 `crates/ipcam-sip/src/negotiate.rs` 里的定义 (测试里的谓词辅助 fn 除外)

- [ ] **Step 4: 汇报 (不提交代码变更除非前面步骤有修复)**

向用户汇报: repowise 前后对比、遗留项、需要真机回归的点 (build_answer 行为变化: audio-only offer 不再无中生有 video m-line)。
