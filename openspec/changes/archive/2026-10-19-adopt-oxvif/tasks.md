## 1. Spike scaffold 清扫

(本 change 已与用户决议跳过 spike-验证,删除早期 spike 阶段的源码与文档,使仓库只剩 adopt 路径)

- [x] 1.1 删 `crates/media_talk/src/ipc/oxvif_spike.rs`
- [x] 1.2 删 `crates/media_talk/src/ipc/oxvif_spike_mock.rs`
- [x] 1.3 改 `crates/media_talk/src/ipc.rs`,删除 `#[cfg(feature = "oxvif-spike")] pub mod oxvif_spike;` 与 `pub mod oxvif_spike_mock;`
- [x] 1.4 改 `crates/media_talk/src/main.rs`,删除 `#[cfg(feature = "oxvif-spike")] ProbeOxvif { ... }` 枚举变体与派发项 (顺带加 `init_backend()` 与 `use std::process::ExitCode`)
- [x] 1.5 删 `docs/oxvif-spike-result.md`
- [x] 1.6 删 `scripts/spike-against-cam.sh`
- [x] 1.7 删 `openspec/changes/oxvif-spike/`(整个目录)
- [x] 1.8 改 `README.md` 的"已知问题"节,删 oxvif spike 链接
- [x] 1.9 改 `.github/workflows/rust.yml`,删 `oxvif-spike` job

## 2. 依赖与 Cargo.toml

- [x] 2.1 `crates/ipcam-discovery/Cargo.toml` `[dependencies]` 加 `oxvif = { version = "=0.12.0", default-features = false, features = ["mock"] }` 严格 pin
- [x] 2.2 同一 `Cargo.toml` 加 `[dev-dependencies] oxvif = ...`(可选,但保持`cargo test -p ipcam-discovery`能验证依赖可解决)
- [x] 2.3 `crates/media_talk/Cargo.toml` 移除 spike 期间的 `oxvif = ...` 行(改到 `ipcam-discovery/Cargo.toml`,media_talk 通过 ipcam-discovery 间接获得)
- [x] 2.4 同一 `Cargo.toml` 移除 `oxvif-spike = ["dep:oxvif"]` feature 行
- [x] 2.5 删 spike 期间 `oxvif` 引入到 `[target.'cfg(...= "linux")'.dependencies]`(若有)的冗余 entry

## 3. ipcam-discovery 重构

- [x] 3.1 新建 `crates/ipcam-discovery/src/_legacy/` 子模块,把现有 `soap.rs`、`ws_security.rs`、`device_mgmt.rs` 整体迁入,改名加 `_legacy` 后缀(`device_mgmt_legacy.rs` 等)
- [x] 3.2 把 `lib.rs::ws_discovery_probe` 整体迁到 `lib.rs::_legacy::ws_discovery_probe`,内部使用 `_legacy::device_mgmt_legacy::DeviceManagementClientLegacy`
- [x] 3.3 把 `lib.rs::ProbeMatch` 与 `local_name`/`parse_probe_message`/`scan_url`/`build_probe_message`/`default_local_addrs` 等 helper 整体迁到 `_legacy` 模块
- [x] 3.4 在 `lib.rs` 重写 `probe_all_with_config()`:读环境变量 `MEDIA_TALK_DISCOVERY_BACKEND`,分支调 `oxvif_backend::probe_all_with_config`(默认)或 `_legacy::probe_all_with_config`
- [x] 3.5 在 `lib.rs` 重写公开 `DeviceManagementClient`:内部可设 `OxvifSession` 或 `LegacyClient`,`new()` / `list_profiles()` / `get_stream_uri()` / `get_capabilities()` 签名与返回类型完全不变
- [x] 3.6 新建 `lib.rs::oxvif_backend`:私有内部模块,实现 `probe_all_with_config` 走 `oxvif::discovery::probe()`,私有类型映射 `oxvif::DiscoveredDevice` → `ipcam_core::DiscoveredDevice`、`oxvif::MediaProfile` → `ipcam_core::VideoProfile`
- [x] 3.7 `crates/ipcam-discovery/src/lib.rs` 头部加 `mod oxvif_backend;` 与 `#[cfg(feature = "legacy-backend")] mod _legacy;`(legacy feature 默认开,可 inline 删)
- [x] 3.8 跑 `cargo test -p ipcam-discovery` 确认既有用例(`parse_probe_matches_with_s` 等指向 `ProbeMatch`)迁移到 `_legacy::ProbeMatch` 后仍绿,53 baseline + 3 backend selection = 56 passed

## 4. media_talk CLI / env wiring

- [ ] 4.1 `crates/media_talk/src/main.rs` 加 `init_env()`:读 `MEDIA_TALK_DISCOVERY_BACKEND`,非法值打印错误退出;有效值传给 `ipcam-discovery::configure_backend()`(静态全局,新加函数)
- [ ] 4.2 `crates/media_talk/src/ipc/discover.rs` 与 `serve.rs` 不动 caller;继续走 `ipcam_discovery::probe_all_with_config` / `DeviceManagementClient::new`
- [ ] 4.3 `crates/media_talk/src/main.rs` 删 `#[cfg(feature = "oxvif-spike")]` 守卫代码(任务 1.4 已覆盖)
- [ ] 4.4 `crates/media_talk/Cargo.toml` 删除 spike 期间的 `oxvif-spike` feature(任务 2.4 已覆盖)

## 5. CI 与 docs

- [x] 5.1 `.github/workflows/rust.yml`:删 `oxvif-spike` job 后,4 个剩余 job 不变 (`fmt` / `clippy` / `test` / `cross-check`)
- [x] 5.2 `README.md`:CLI 章节的 `discover` / `serve` 命令示例保留;Quickstart 不变
- [x] 5.3 `media_talk.service`:在 `[Service]` 段加一行注释示例,运维可在需要时加 `Environment=MEDIA_TALK_DISCOVERY_BACKEND=legacy`(不强制,默认保留现 RUST_LOG)

## 6. 完整 gate(self-check)

- [x] 6.1 `cargo fmt --all`
- [x] 6.2 `cargo clippy --workspace --all-targets --features sw-decode -- -D warnings`
- [x] 6.3 `cargo test --workspace --features sw-decode` (46 passed:53 baseline − 10 legacy tests − 3 backend dispatch + 3 parse_xaddr_endpoint)
- [x] 6.4 `cargo clippy -p ipcam-discovery -- -D warnings`
- [x] 6.5 `cargo tree -p media_talk | grep -i oxvif` → oxvif v0.12.0 (通过 ipcam-discovery 间接依赖,media_talk 不直接依赖)
- [x] 6.6 `git diff --stat crates/ipcam-discovery` → +215/-334 (lib.rs 大重构;3 个旧文件被 `_legacy/` 子模块替换,后又被用户要求删除,最终落到 oxvif 单一后端)
- [x] 6.7 不再需要 `_legacy` 公开面 — 用户决定 ship oxvif-only,移除 `legacy-backend` feature 与 `MEDIA_TALK_DISCOVERY_BACKEND` env flag,回滚路径走 git revert

## 7. 收尾(等用户 review 与 commit)

- [ ] 7.1 跑 `git diff --stat` 把变更摘要给用户看(预计 8-12 文件改动)
- [ ] 7.2 把 commit message 草稿拟给用户: `[重构] 用 oxvif 0.12.0 替换 ipcam-discovery 内部实现,保留 legacy backend fallback (OpenSpec change adopt-oxvif)`
- [ ] 7.3 等用户显式 `继续` 才执行 `git commit`
- [ ] 7.4 跟踪 CI(4 job)全绿
- [ ] 7.5 `openspec archive adopt-oxvif` 把 specs 沉淀到 `openspec/specs/ipcam-discovery/spec.md` + `openspec/specs/oxvif-discovery-backend/spec.md`
