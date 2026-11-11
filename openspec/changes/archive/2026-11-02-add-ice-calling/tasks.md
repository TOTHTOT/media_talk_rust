## 1. ipcam-gst FFI 与链接

- [x] 1.1 新建 `crates/ipcam-gst/build.rs` 与 `src/ice/{mod.rs, sys.rs}`: FFI 符号手写声明 (~15 个) 对照本机 libnice 0.1.23 头文件, `lib.rs` 挂 `pub mod ice;`, 链接 nice

## 2. ice/blob.rs — SDP blob 与候选行解析

- [x] 2.1 新建 `src/ice/blob.rs` (`IceBlob` / `CandidateLine` / `CandKind`), `mod.rs` 挂 `pub mod blob;`, 基于 libnice 原生 SDP 文本, TDD 单测

## 3. ice/agent.rs — IceAgent 与环回冒烟

- [x] 3.1 新建 `src/ice/agent.rs` (`IceConfig` / `IceState` / `IceAgent`), 自建 GMainContext 线程 + 信号回调只塞 channel, 双 agent 环回冒烟测试 (风险前置)

## 4. sigserver (media_talk_rust 第二 bin)

- [x] 4.1 新建 `crates/media_talk_rust/src/bin/sigserver.rs`: 房间制 WS 转发 (join / offer / answer / hangup, 满员拒绝), `Cargo.toml` 加 `tokio-tungstenite = { workspace = true }`

## 5. ice_media.rs — nicesink / nicesrc 媒体管线

- [x] 5.1 新建 `src/ice_media.rs` (`IceMediaConfig` / `start_ice_media` / `IceMediaHandle`): 视频 component 1 (H264 pt 96) + 音频 component 2 (Opus pt 111), `rtp_send/chain.rs` 源支路构造 fn 提 `pub(crate)` (逻辑零改动)

## 6. call-ui ice 控制器

- [x] 6.1 新建 `crates/call-ui/src/ice.rs`: 后台控制器 + WS 信令通道 + 手动 blob 通道, `lib.rs` spawn 接线, `Cargo.toml` 加 `tokio-tungstenite = { workspace = true }`

## 7. 设置页 ICE 卡片

- [x] 7.1 新建 `crates/call-ui/ui/settings/ice_card.slint`, `settings_page.slint` / `main.slint` / `wiring.rs` 属性与 callback 转发, `wire_ice` 装配

## 8. CI / 交叉构建 / 文档

- [x] 8.1 `.github/workflows/rust.yml` 与 `Dockerfile.rk3566` apt 列表加 `libnice-dev`; `docs/cross-build.md` 加 gstreamer1.0-nice 与 libnice 节; `media_talk_rust.service` 加板端 gst-inspect 验证提示
- [x] 8.2 过门禁: `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets --features sw-decode -- -D warnings` / `cargo clippy -p call-ui --all-targets -- -D warnings` / `cargo test --workspace --features sw-decode`
