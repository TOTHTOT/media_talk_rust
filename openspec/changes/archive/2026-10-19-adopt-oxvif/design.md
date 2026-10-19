## Context

`ipcam-discovery` 是手写的 ONVIF 客户端(UDP 多播 + reqwest + quick-xml + 自实现 SOAP envelopes + 自实现 WS-Security / Digest 鉴权),共约 700 行核心代码。最近一次 repowise 健康度扫描把 `lib.rs::parse_probe_message` 标为仓库内最差大脑方法 (5.55/10)。

[`oxvif 0.12.0`](https://crates.io/crates/oxvif) (MIT,2026-04-02 发布,29 版本,单维护者 `smiti1642`) 提供完整 ONVIF Profile S/T/G 实现 + 内置 MockTransport / MockServer。**已与用户决议:跳过 spike,直接上生产**(参见 `openspec/changes/oxvif-spike/` 文档化的反方意见)。

外部约束:
- 实验环境 `192.168.1.0/24` 有 12 台测试摄像机,11 valid + 1 invalid(192.168.1.19)
- 部署目标板 `aarch64-unknown-linux-gnu.2.31`,生产环境 rk356x
- CLAUDE.md §7 要求跨 ≥ 2 crate / public API 变更必须先 OpenSpec 流程(本 change 跨 `ipcam-discovery`、`media_talk`,完全命中)

## Goals / Non-Goals

**Goals**

1. `ipcam-discovery` 内部实现替换为基于 `oxvif 0.12.0` 的调用,保留 public API 兼容(零修改外部 caller)
2. 删除 spike 阶段产生的 scaffold(`oxvif_spike*` 模块 + `docs/oxvif-spike-result.md` + `scripts/spike-against-cam.sh` + `openspec/changes/oxvif-spike/`)
3. 删除 spike 引入的 `oxvif-spike` CI job(默认 build 已含 oxvif)
4. 默认 build / 测试保持全绿
5. 公开 API 保持原签名(`probe_all_with_config`、`DeviceManagementClient::new` / `list_profiles` / `get_stream_uri` / `get_capabilities`、`Discovery` trait)

**Non-Goals**

1. ❌ 不在 spec / design 阶段做真机兼容性验证(`192.168.1.x`)— 已与用户决议不再做 spike-验证,板端直跑
2. ❌ 不动 ONVIF Profile T 高级特性(PTZ / Imaging / Events / Recording);oxvif 已经支持,本 change 不引入新功能
3. ❌ 不动 `crates/ipcam-rtsp` / `crates/web-display` / `crates/hardware-decode` / `crates/ipcam-core`(公开 API 完全不变)
4. ❌ 不引入新 LTS / nightly toolchain 假设;`=0.12.0` 严格 pin 但 `Cargo.toml` 默认 features 即可
5. ❌ 不保留 legacy backend 回滚路径 — 用户已决议 ship oxvif-only,回滚走 git revert + 重 deploy 流程

## Decisions

### 1. Strict pin `oxvif = "=0.12.0"`

**决策**: 严格等号 pin。0.13+ 任何升级走新 change 评估。

**Why**: 单人维护 0.x 库,自动升级风险大。`Cargo.lock` 锁定,防止 `cargo update` 抖动。

**Rejection**:
- `^0.12` 默认 — 可能在无意中引入 0.12.1 / 0.13.0
- `latest` 疯狂 — 每天都在变

### 2. 公开 API 完全不动

**决策**: `crate::ipcam_discovery::*` 公开项目保持不变。`probe_all_with_config()`、`DeviceManagementClient::new/list_profiles/get_stream_uri/get_capabilities`、`Discovery` trait 全部保留。

**Why**: `media_talk` 已有 2 个 caller (`discover::run`,`serve::run`),`web-display` 1 个 caller (`stream::resolve_stream_uri`),改公开 API 会级联到这些 caller,review 难度上升。

**Rejection**:
- 公开 `DeviceManagementClient.inner_session()` 之类的 getter — 让 caller 主动管理 session,破坏封装
- 重命名 `ipcam-discovery` → `onvif-client` — 公开 API 重命名属于 breaking,**严格禁止** in this change

### 3. 单 backend: oxvif only

**决策**: `ipcam-discovery` 内部只接 `oxvif::OnvifSession`。**没有** `_legacy/` 子模块,**没有** `legacy-backend` feature,**没有** `MEDIA_TALK_DISCOVERY_BACKEND` env flag。

**Why**: 用户已决议 ship oxvif-only 直上生产。env flag 是我之前提的"安全网",用户认为多余 — 他们说"oxvif 不行就 git revert"。

**Rejection**:
- 保留 `_legacy/` + env flag — 用户不要
- 把老代码放另一个 workspace member — 走 git revert 路径,不需要
- 编译期 feature flag (`--features legacy`) — 编译期比 env 慢得多

### 4. Profile 字段退化 (`width/height/fps/codec` 默认 0)

**决策**: oxvif `MediaProfile` 不带 codec/width/height/fps(那些在 `VideoEncoderConfiguration`)。默认 backend 我们不做第二次 GetVideoEncoderConfiguration 调用,字段默认 `Unknown` / 0 / 0 / 0.0。

**Why**: 多一次 round-trip = 多一次失败点。spec req 3 (MODIFIED Requirements) 显式记录此行为。

**Rejection**:
- 调用 `GetVideoEncoderConfiguration` 拉完整 — 留给后续按需添加,本 change 不展开

### 5. Spike scaffold 全清

**决策**: 完全删除 spike 阶段留下的:`crates/media_talk/src/ipc/oxvif_spike.rs` + `oxvif_spike_mock.rs`、`crates/media_talk/src/ipc/oxvif-spike` feature gate、`scripts/spike-against-cam.sh`、`docs/oxvif-spike-result.md`、`openspec/changes/oxvif-spike/`、`ProbeOxvif` CLI 子命令、`.github/workflows/rust.yml` 的 `oxvif-spike` job、README 的"已知问题 #0"链接。

**Why**: spike 设计是"尝试后回滚",我们直接采纳,中间态不再有效。保留 spike 文件会让 reviewer 误以为 spike 路线仍在跑。

**Rejection**:
- 保留 `oxvif-spike` 目录作为"决策记录" — 写进 archive / docs 即可,不需源码层留存

## Risks / Trade-offs

- **[Risk]** oxvif multicast 行为与本项目自实现不同(实验网段绑定 6 个 socket,oxvif README 描述是"每非 loopback IPv4 + 0.0.0.0 兜底")。可能 192.168.1.x 12 台抓不全。 → **Mitigation**: git revert + 重 deploy(用户已接受此回滚路径)。
- **[Risk]** oxvif 单人维护 + 4 个月历史,后续 0.13.x / 1.0 升级需要新 change。 → **Mitigation**: `=0.12.0` 严格 pin;`Cargo.lock` 不允许浮动;升级路径独立 review。
- **[Risk]** 共享 dep upgrade: `quick-xml 0.36→0.41`、`reqwest 0.12→0.13`、`if-addrs 0.10→0.15`、`sha1 0.10→0.11` — 28 个 dep chain 编译时间 +30-60s,可能让 CI 默认 job 慢一些。 → **Mitigation**: `Swatinem/rust-cache@v2` 已经 cache;CI 默认 job 增量应该 < 30s;若有新 warning 在 `-D warnings` 下被 clippy catch,fix 一并。
- **[Risk]** 真机(11 valid + 1 invalid creds)从未经验证直接上生产。 → **Mitigation**: 用户已决议走快路径。回滚 = git revert。
- **[Risk]** profile 字段退化 (`width/height/fps/codec` 默认 0)。 → **Mitigation**: spec 已记录;前端 `/api/devices` 输出仍合法,只是字段默认值。后续按需补 GetVideoEncoderConfiguration 调用。

## Migration Plan

1. **合并 adopt-oxvif commit**(包含 oxvif 替换 + spike cleanup): 用户执行 review & `git push`
2. **板端部署**: 标准流程,无特殊配置(`MEDIA_TALK_DISCOVERY_BACKEND` 不再读)
3. **回滚** (假设需要): git revert <commit> + 重 build + 重 deploy (5-10 分钟)
4. **`openspec archive adopt-oxvif`** 把 specs 沉淀到 `openspec/specs/`

## Open Questions

1. `Cargo.toml` 是否需要 `[workspace.dependencies]` 把 oxvif 提到 workspace root? 当前只在 `ipcam-discovery/Cargo.toml` 加,本地化。**倾向不加**,保持局部可见性。
2. CI 是否需要新 job 把 `cargo test -p ipcam-discovery` 拆分出来跑得更快?现在 46 个测试里只有 6 个 media_talk 测试,ipcam-discovery 3 个测试,合起来不到 3s 没必要拆。
3. 真机兼容性验证门要不要预留 trigger 文件 `docs/decide-on-real-cam.md` 等用户部署后写? **倾向不预留,直接上板**,用户在板端观察,真出问题记 git revert + archive。
