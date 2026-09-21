# CLAUDE.md — media_talk_rust

> AI 代理 (Claude Code 等) 在此仓库工作时的项目级指引。本文件与 `.claude/CLAUDE.md`(repowise 自动生成索引)并存,互不覆盖。

---

## 1. 硬门槛 — 提交 / 报告完成前**必须**全部通过

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --features sw-decode -- -D warnings
cargo test  --workspace --features sw-decode
```

- 三项中任何一项失败 → 修完再交,不要带病提交
- 不要 `#[allow(clippy::...)]` 来消音;若必须用,必须有内联注释解释原因
- 不要改 edition (2024) 或 MSRV (`clippy.toml` 里 `msrv = "1.85"`)

---

## 2. 文档注释模板 — 仅用于规模大的函数

公开函数参数 > 3 或返回 `Result` / 复杂类型时,严格按下面格式生成,**逐行照抄,包括空行与缩进**:

```rust
///
/// 
/// 
/// # Arguments 
/// 
/// * `timeout_secs`: 
/// * `json`: 
/// * `username`: 
/// * `password`: 
/// 
/// returns: Result<(), Error> 
/// 
/// # Examples 
/// 
/// ```
/// 
/// ```
```

小工具 / 显而易见的 getter / 单参数内部 helper **不**需要这个模板。自行判断。

---

## 3. Workspace 结构

```text
crates/
├── media_talk_rust/        # 二进制入口 (clap 子命令)
├── ipcam-core/         # 共享类型 (EncodedPacket / DecodedFrame / Decoder trait)
├── ipcam-discovery/    # ONVIF WS-Discovery + Device Management (SOAP)
├── ipcam-gst/          # GStreamer 拉流引擎 (rtspsrc→appsink)
├── hardware-decode/    # Rockchip MPP (hw-decode) + SoftwareDecoder (sw-decode)
├── web-display/        # axum + WebSocket + fMP4 muxer
├── ipcam-alsa/         # ALSA 设备枚举 (仅 Linux)
└── v4l2-device-cap/    # V4L2 capture 能力枚举 (仅 Linux)

openspec/                # OpenSpec change + baseline specs
media_talk_rust.service      # systemd unit (生产部署)
.github/workflows/       # CI: fmt / clippy / test / cross-check
```

依赖方向:`media_talk_rust` → 所有 `ipcam-*` / `hardware-decode` / `web-display`。库 crate 之间**禁止**互相依赖形成环。

---

## 4. Feature flags & 目标平台

| Flag | 何时用 |
| --- | --- |
| `--features sw-decode` | 开发机 / CI / 桌面 Linux / Windows (SoftwareDecoder stub) |
| `--features hw-decode` | 仅交叉编译到 `aarch64-unknown-linux-gnu.2.31` (rk356x 板) |

gst 拉流引擎 (ipcam-gst) 是**无条件依赖** (无 feature 门控), 要求构建机装有
GStreamer + pkg-config; 所有 gstreamer API 调用只允许出现在
`crates/ipcam-gst/src/` 的管线模块里 (`ingest.rs` 拉流进, `rtp_send.rs` 发出),
纯逻辑 (config/packet/stats) 保持独立可测。

交叉编译:
```bash
cargo zigbuild --target aarch64-unknown-linux-gnu.2.31 --release --features hw-decode
```

平台门控:`ipcam-alsa` / `v4l2-device-cap` / `hw-decode` 的 MPP 部分用 `#[cfg(target_os = "linux")]` 包,不要在 Windows dev 编译时炸。

---

## 5. 代码风格约定

- **标点符号**:所有中文文本 (回复、代码注释、文档、提交消息) 一律用英文标点,并在标点后加一个空格。例:用 `, ` 不用 `,`,用 `. ` 不用 `。`,用 `: ` 不用 `:`。数字/英文与中文之间同样加空格分隔
- **错误处理**:库 crate 用 `thiserror::Error` 派生枚举;二进制 (`media_talk_rust`) 用 `anyhow::Result` 在顶层边界
- **日志**:只用 `tracing` (`info!` / `warn!` / `error!` / `info_span!`)。**禁止** `println!` / `eprintln!` / `dbg!` 出现在生产代码
- **结构化字段**:`info!(user_id, ?err, "msg")`,把字段放前,消息放最后;`?` 用 Debug,`%` 用 Display
- **Span**:长时间事务 (一整个 RTSP session、一次 mux) 包一个 `info_span!`,用 `.instrument(span)` 绑定到 async future
- **Async**:tokio。`async fn` 内**禁止** `std::thread::sleep` / 阻塞 I/O / CPU 紧循环。CPU 紧循环用 `tokio::task::spawn_blocking`
- **库代码不 panic**:`unwrap` / `expect` 只允许在测试和 `const fn` 里出现。库公开 API 用 `?` + `thiserror` 映射
- **命名**:标准 Rust 约定 (`snake_case` 函数/变量、`PascalCase` 类型、`SCREAMING_SNAKE_CASE` 常量)
- **测试**:单元测试同行 (`#[cfg(test)] mod tests`);集成测试在 `tests/`;函数名 `tests::does_x_under_y`,**不要**叫 `test1` / `test_works`

---

## 6. 提交消息风格

沿用仓库已有约定,中文前缀 + 现在时 + 简洁:

```
[新增] <加了什么>
[修复] <修什么 bug>
[重构] <重构了什么>
[回滚] <回滚了什么>
```

正文 ≤ 5 行,除非是破坏性改动才展开。**禁止** `git commit -m "wip"` / `"fix"` / `"update"` 这种无意义消息。

---

## 7. OpenSpec 流程

下列变更**必须**先 `/opsx:propose <name>`,不允许直接改代码:

- 新 crate 或新模块边界
- 任一现有 crate 的公开 API 变更
- `media_talk_rust` CLI 子命令的行为变化
- 新 feature flag
- 跨 ≥ 2 个 crate 的改动

下列可跳过 propose:

- 拼写 / 注释 / 格式修正
- 单点 bug 修复且不引入新 API
- 测试用例补充
- `Cargo.toml` 依赖调整 (按本节"不要新依赖"规则)

完成后:**必须** `openspec archive <name>` 把 specs 沉淀到 `openspec/specs/` 基线,否则下个 change 会冲突。

---

## 8. 不要做 (Don'ts)

- ❌ 不要加 `#[allow(clippy::...)]` 来通过 lint,除非有内联 `// reason:` 注释
- ❌ 不要引入新 crate 依赖,先 `cargo tree | grep <name>` 确认是否已存在
- ❌ 不要提交后 `cargo test --workspace` 才报错 — 写完就测
- ❌ 不要写 `unsafe`,除非有明确 safety 注释 + 单元测试
- ❌ 不要提交 `target/` / `.repowise/` / `.idea/` / 根目录的 `mediatalk` 二进制
- ❌ 不要把 secrets (admin 密码、ONVIF 凭据) 写进日志或单元测试 fixture
- ❌ 不要硬编码路径 — 用 `StateDirectory=media_talk_rust` (systemd) / `std::env::var` (应用)
- ❌ **不要 `git commit` / `git push` 未经用户显式确认** — AI 代理不主动对外发布代码;用户说"提交 / 推上去"才能 commit。OpenSpec change 在 archive 之前也禁止 commit

---

## 9. 用户授权边界

AI 代理在本仓库**默认无权**做以下操作,要由用户显式触发:

- `git commit` / `git commit --amend` / `git reset --hard` / `git push` / `git push --force` / 任何对外可见的 git 写动作
- `gh repo create` / `gh repo edit` / 创建或合并 PR / 编辑 GitHub 仓库设置
- 任何跨 OpenSpec change 边界的修改 (例:`adopt-oxvif` 期间改动 `ipcam-discovery` 当前 active API)
- 删除已 tracked 的文件 / 目录
- 修改 `Cargo.toml` workspace 根的 `members` 列表

**自动可做** (无需确认): 修改源文件、新建本地文件、跑 cargo/clippy/test 等只读验证、运行 `git status` / `git diff` / `git log` 等只读命令、`openspec` / `repowise` 等只读查询。

---

## 10. 完成前自检清单

每次说"做完了"之前,逐项过:

- [ ] `cargo fmt --all` 已运行
- [ ] `cargo clippy --workspace --all-targets --features sw-decode -- -D warnings` 通过
- [ ] `cargo test --workspace --features sw-decode` 通过 (53 个 baseline,新增的不应减少)
- [ ] `git status` 无未追踪垃圾 (`?? mediatalk`、`?? target/...` 等)
- [ ] 提交消息符合 §6
- [ ] (如适用) OpenSpec change 已归档到 `openspec/specs/`
- [ ] (如改了 ABI/CLI) README.md 同步更新

完成消息里**必须**贴出三项 gate 的最后一行输出 (例如 `test result: ok. 53 passed`) 作为证据。