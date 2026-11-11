## Why

现有通话验证只有 SIP 链路, 它回答的是「局域网内注册 / 呼叫 / 挂断的礼数」, 回答不了「跨网 P2P 打洞到底能不能通」. 产品化 (公网 SIP + STUN/TURN) 之前, 需要一条独立的 ICE 验证通路, 把打洞形态 (成功 / 失败 / 候选类型) 看清楚:

1. 音视频双向通话, 两端各跑一个本程序实例 (开发板 + 电脑), 媒体通路必须过 ICE (LAN 测试也一样照跑, host 候选会被自动选中).
2. 信令双通道: 局域网用微型 WS sigserver 自动交换候选 blob; 跨网 (sigserver 不可达) 时退化为人肉导出 / 导入 blob.
3. 现有 SIP 通话链路一行不动, ICE 通话是独立验证模式.

## What Changes

- **新增** `ipcam-gst` 的 `ice` 模块 (`sys` / `agent` / `blob` 三个子模块): 手写 libnice FFI (~15 个符号, 对照本机 0.1.23 头文件), `IceAgent` (gather / `local_blob` / `add_remote` / 状态 watch 通道), `IceBlob` 序列化与候选行解析; `build.rs` 新增 nice 链接
- **新增** `crates/ipcam-gst/src/ice_media.rs`: nicesink / nicesrc 媒体管线 (视频 H264 pt 96, 音频 Opus pt 111, 双 component), `start_ice_media` / `IceMediaHandle`
- **新增** bin `crates/media_talk_rust/src/bin/sigserver.rs`: `media_talk_rust` 第二 bin, tokio + tokio-tungstenite 房间制 WS 信令转发 (join / offer / answer / hangup, 满员拒绝)
- **新增** `crates/call-ui/src/ice.rs`: 后台 ICE 控制器 + WS 信令通道 + 手动 blob 通道
- **新增** `crates/call-ui/ui/settings/ice_card.slint`: 设置页 ICE 卡片 (sigserver 地址 / 房间名 / 呼叫 / 手动模式 / 通话状态行)
- **修改** `.github/workflows/rust.yml`、`Dockerfile.rk3566`、`docs/cross-build.md`、`media_talk_rust.service`: libnice-dev / gstreamer1.0-nice 系统依赖与验证提示
- **非破坏性**: 全部为新增 API 与新增接线, 现有 SIP / RTSP 链路一行不动; **不新增 crate, 不改 workspace root members**; **唯一新增依赖 tokio-tungstenite** (已在依赖树, call-ui 与 media_talk_rust 各加 workspace 引用); 无 TURN; 非 trickle (gather 完成一次性出全量 blob)

## Capabilities

### New Capabilities
- `ice-calling`: ICE 跨网 P2P 音视频通话验证 —— libnice 手写 FFI 打洞 + 双通道信令 (WS sigserver 自动 / 手动 blob 兜底) + nicesink / nicesrc 媒体收发, 通话状态与收发字节统计可观测

### Modified Capabilities
<!-- 无现有 spec 的需求变更 -->

## Impact

- `crates/ipcam-gst`: 新增 `src/ice/{mod.rs, sys.rs, agent.rs, blob.rs}` 与 `src/ice_media.rs`, 新 pub API (`ice` 模块 + `start_ice_media`), 新增 `build.rs` 链接 libnice; `rtp_send/chain.rs` 源支路构造 fn 提为 `pub(crate)` (逻辑零改动)
- `crates/media_talk_rust`: 新增 bin `sigserver`, `Cargo.toml` 加 `tokio-tungstenite = { workspace = true }`
- `crates/call-ui`: 新增 `src/ice.rs` 与 `ui/settings/ice_card.slint`, `wiring.rs` / `lib.rs` 内部接线, `Cargo.toml` 加 `tokio-tungstenite = { workspace = true }`
- `.github/workflows/rust.yml` / `Dockerfile.rk3566`: apt 列表加 `libnice-dev`
- `docs/cross-build.md`: 板端运行依赖加 gstreamer1.0-nice, 新增 libnice 节
- Windows 开发机无需另装 libnice 开发包 (gstreamer msvc 捆绑 nice.lib + nice-10.dll, 已实测可链接)
- SIP 链路不动; 不新增 crate, 不改 root members, 不新增 feature flag
