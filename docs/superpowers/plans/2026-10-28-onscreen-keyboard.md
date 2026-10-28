# 屏幕键盘 + 中文输入法实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** call-ui 增加屏幕键盘 (QWERTY 全键盘, 纯触屏), 中文拼音输入内嵌 librime。

**Architecture:** engine trait 分层 (纯逻辑可测) + librime FFI (cfg linux) / 直通 stub (cfg not(linux)); 上屏走 slint 属性拍振通道, KbTextField 自改 text; Rust 控制器只维护引擎与键盘可视状态, 不碰输入框。

**Tech Stack:** slint 1.18, librime 1.5 (focal apt), 无新增 crate 依赖。

## Global Constraints

- 提交格式 `[类型] 模块: 标题`, 中文, 英文标点 + 空格
- 库代码禁 println!/unwrap/expect (测试除外), 只用 tracing
- 门禁: `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets --features sw-decode -- -D warnings` / `cargo clippy -p call-ui --all-targets -- -D warnings` / `cargo test --workspace --features sw-decode` (Windows 需先 `export PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/bin:$PATH" PKG_CONFIG_PATH="/d/Soft/gstreamer/1.0/msvc_x86_64/lib/pkgconfig"`)
- slint 文案必须 @tr; lucide 图标走 ui/icons.txt 清单
- 不新增 crate 依赖 (librime 是系统库, 不是 crate)
- 上屏 v1 语义: 提交文本**追加到输入框末尾**; 退格在预编辑空时删输入框末字符 (不跟踪光标位置, material TextField 不暴露 cursor)

## 文件结构

- 新建 `crates/call-ui/src/keyboard/mod.rs` — 控制器: 按键路由 + 焦点目标注册 + 提交派发
- 新建 `crates/call-ui/src/keyboard/engine.rs` — `Engine` trait / `Mode` / `EngineOutput` + 单测用 MockEngine
- 新建 `crates/call-ui/src/keyboard/rime.rs` — librime 实现 (cfg target_os = "linux")
- 新建 `crates/call-ui/src/keyboard/stub.rs` — 直通引擎 (cfg not(linux))
- 新建 `crates/call-ui/ui/keyboard.slint` — 键盘视图
- 新建 `crates/call-ui/ui/kb_text_field.slint` — KbTextField 组件
- 修改 `crates/call-ui/ui/main.slint` — 键盘属性/回调/挂载
- 修改 `crates/call-ui/src/lib.rs` — 控制器装配
- 修改 `crates/call-ui/src/wiring.rs` — 字段焦点/目标注册回调
- 修改 接入字段处: `ui/settings/general_card.slint` / 通讯录 / 手动摄像头对话框
- 修改 `Dockerfile.rk3566` / `.github/workflows/rust.yml` / `docs/cross-build.md`

## 任务间接口契约 (所有任务共用)

```rust
// engine.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode { Zh, En, Num }

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EngineOutput {
    pub preedit: String,          // 预编辑拼音串
    pub candidates: Vec<String>,  // 当前候选
    pub commits: Vec<String>,     // 需上屏文本 (按序)
    pub delete_field: bool,       // 退格且预编辑空: 删输入框末字符
}

pub(crate) trait Engine {
    fn mode(&self) -> Mode;
    fn set_mode(&mut self, mode: Mode);
    fn key(&mut self, k: char) -> EngineOutput;
    fn backspace(&mut self) -> EngineOutput;
    fn select(&mut self, idx: usize) -> EngineOutput;
    fn space(&mut self) -> EngineOutput;
}

// rime.rs / stub.rs
pub(crate) fn create_engine() -> Box<dyn Engine>;

// mod.rs (控制器)
pub(crate) type FieldId = u32;
pub(crate) struct KeyboardController { /* ... */ }
impl KeyboardController {
    pub fn new() -> Self;
    pub fn key_tapped(&mut self, key: &str);        // slint 回调
    pub fn backspace_tapped(&mut self);
    pub fn candidate_tapped(&mut self, idx: i32);
    pub fn space_tapped(&mut self);
    pub fn mode_tapped(&mut self, mode: i32);       // 0=zh 1=en 2=num
    pub fn set_focus(&mut self, field: Option<FieldId>);
    // 以下供 wiring 读取后写 slint 属性 (见 Task 2 属性表)
    pub fn take_output(&mut self) -> Option<EngineOutput>;  // 有变更才 Some
    pub fn zh_available(&self) -> bool;              // stub 返回 false
}
```

slint 属性表 (main.slint 根部):

| 属性 | 类型 | 方向 | 含义 |
|---|---|---|---|
| `kb-visible` | bool | in | 键盘显隐 |
| `kb-mode` | int | in | 0=zh 1=en 2=num |
| `kb-zh-available` | bool | in | 中文可用 (Windows stub=false, 隐藏中模式键) |
| `kb-preedit` | string | in | 预编辑串 |
| `kb-candidates` | [string] | in | 候选词 |
| `kb-commit-seq` | int | in | 上屏拍振计数 |
| `kb-commit-text` | string | in | 上屏文本 (追加语义) |
| `kb-delete-field` | bool | in | 拍振: true=删输入框末字符 (配合 seq 用) |
| 回调 `kb-key-tapped(string)` / `kb-backspace()` / `kb-candidate(int)` / `kb-space()` / `kb-mode(int)` / `kb-dismiss()` / `kb-field-focused(int)` | - | out | Rust 侧装配 |

---

### Task 1: engine trait + 控制器 + MockEngine 单测

**Files:**
- Create: `crates/call-ui/src/keyboard/engine.rs`
- Create: `crates/call-ui/src/keyboard/mod.rs`
- Create: `crates/call-ui/src/keyboard/stub.rs` (直通引擎, cfg not(linux))
- Modify: `crates/call-ui/src/lib.rs` (挂 `mod keyboard;`)
- Test: `crates/call-ui/src/keyboard/engine.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: 上文契约中的 engine.rs 全部 + `stub::create_engine` + `KeyboardController`

- [ ] **Step 1: 写失败测试** (engine.rs tests): MockEngine 模拟拼音组合 ("xi" → 候选 ["西","喜","细"]), 用 KeyboardController 驱动:
  - `en 模式: key_tapped("a") → take_output().commits == ["a"]`
  - `zh 模式: key_tapped("x"); key_tapped("i") → preedit=="xi", candidates 非空, commits 空`
  - `zh 空格 → commits 含首候选, preedit 清空`
  - `zh backspace: 预编辑非空 → 缩短拼音; 空 → delete_field=true`
  - `set_focus(None) 时 take_output 无提交`
  - `set_focus(Some(id)) 后才派发 commits`
- [ ] **Step 2: 跑测试确认失败** (trait/控制器不存在, 编译错误即失败)
- [ ] **Step 3: 实现** engine.rs (trait + Mode + EngineOutput + MockEngine) / mod.rs (控制器: 按键→引擎, 缓存最新 output, take_output 只在有新输出时 Some; focus 为空时吞掉 commits) / stub.rs (En/Num 直出, Zh 模式回退 En 行为并 zh_available()=false)
- [ ] **Step 4: 跑测试确认通过**
- [ ] **Step 5: 门禁四连 + 提交** `[功能] call-ui: 键盘引擎抽象与控制器 (MockEngine 单测)`

### Task 2: slint 键盘视图 + KbTextField + 装配

**Files:**
- Create: `crates/call-ui/ui/keyboard.slint`
- Create: `crates/call-ui/ui/kb_text_field.slint`
- Modify: `crates/call-ui/ui/main.slint` (属性表 + Keyboard 挂载 + 回调透传)
- Modify: `crates/call-ui/src/lib.rs` (KeyboardController 装配: 回调→控制器→属性回写)
- Modify: `crates/call-ui/src/wiring.rs` (kb-field-focused 回调 → controller.set_focus)

**Interfaces:**
- Consumes: Task 1 控制器; KbTextField 复用 material TextField 全部绑定 (text/label/supporting-text/input-type/width, edited/accepted 回调)
- Produces: 可用的 `KbTextField` 组件 (签名: `text <=>`, `label:`, `supporting-text:`, `input-type:`, `edited(string)`, `has-focus` 透传) 供 Task 3 替换

- [ ] **Step 1: keyboard.slint** — 底部滑出面板 (高度 45% 屏高, 黑色 85% 透明底): 顶行候选栏 (预编辑 + 横向候选, 仅 zh 模式显候选列表; en/num 模式隐藏整行) + 键盘区三页 (zh/en 共用字母区, 数字符号页) + 底行 (模式切换 / 空格 / 退格 / 完成)。按键统一 `TouchArea` + `kb-key-tapped("a")` 等回调; 中文模式键 `visible: root.kb-zh-available`
- [ ] **Step 2: kb_text_field.slint** — `KbTextField inherits Rectangle`: 内含 material TextField (双向绑定 text), `has-focus` 变化时 `kb-field-focused(id)` / 全失焦时 `kb-field-focused(-1)`; 监听 `kb-commit-seq` 变化 (changed 处理器): 若自身聚焦且有提交 → `text = text + kb-commit-text` 并转发 `edited(text)`; 若 `kb-delete-field` → 删末字符 (按 char, 非 byte) 并转发 edited
- [ ] **Step 3: main.slint** — 根部声明契约属性表 + 7 个回调; 根部 overlay 层 (z 最高) 挂 `if root.kb-visible: Keyboard { ... }`; 回调体里调 root 回调 (透传给 Rust)
- [ ] **Step 4: lib.rs 装配** — `let mut kb = keyboard::KeyboardController::new();` 各 on_kb_* 回调驱动控制器, `take_output` 有值时经 Weak<MainWindow> invoke 回写 kb-preedit/kb-candidates/kb-commit-text/kb-commit-seq/kb-delete-field/kb-mode/kb-visible (焦点变化时 visible 切换)
- [ ] **Step 5: wiring.rs** — on_kb_field_focused(id) → kb.set_focus(id >= 0)
- [ ] **Step 6: 门禁四连 + 提交** `[功能] call-ui: 屏幕键盘视图与上屏通道`

### Task 3: 接入输入字段

**Files:**
- Modify: `crates/call-ui/ui/settings/general_card.slint` (城市 / 和风 Key)
- Modify: 通讯录姓名/号码输入处 + 手动添加摄像头对话框输入处 (按 Task 2 产出组件签名替换)

- [ ] **Step 1: 每处 TextField → KbTextField**, 绑定原样保留, 分配 FieldId 常量 (engine.rs 或 wiring 定义 `pub(crate) const FIELD_*`)
- [ ] **Step 2: 手测点检清单** (起 gui: 点城市输入框弹键盘; en 直出; 退格; 收起; Windows 无中文模式键) — 记录在 commit message
- [ ] **Step 3: 门禁四连 + 提交** `[功能] call-ui: 屏幕键盘接入设置页与通讯录输入`

### Task 4: librime 实现 (cfg linux)

**Files:**
- Create: `crates/call-ui/src/keyboard/rime.rs` (cfg target_os = "linux", lib.rs 条件挂载)

- [ ] **Step 1: 取 librime 1.5.3 `rime_api.h`/`rime_api_builtin_types.h`** (github librime tag 1.5.3), 翻译所需结构体: `RimeTraits`, `RimeApi` (前缀到 select_candidate 为止), `RimeContext`, `RimeComposition`, `RimeMenu`, `RimeCandidate`
- [ ] **Step 2: rime.rs 实现** — `#[link(name = "rime")] extern "C" { fn rime_get_api() -> *const RimeApi; }`; 单例 `static API: OnceLock<RimeApi 副本>`; create_engine() 时 initialize (traits: shared_data_dir 取 env RIME_DATA_DIR, 默认 /usr/share/rime-data, user_data_dir ~/.local/share/media_talk/rime); Engine impl: key=字母→RimeProcessKey(sym, 0), 刷新 preedit/candidates (RimeGetContext→composition.preedit+menu.candidates, 用完 RimeFreeContext); select→RimeSelectCandidate; space→select(0) 或提交空格; backspace→RimeProcessKey(XK_BackSpace=0xFF08) 后判断 composition 空→delete_field
- [ ] **Step 3: 编译门控验证** — Windows 上 `cargo check -p call-ui` (rime.rs 不参与); 交叉编译留给 Task 5 的 docker 验证
- [ ] **Step 4: 提交** `[功能] call-ui: librime 中文引擎实现 (linux)`

### Task 5: 交叉构建 + CI + 文档

**Files:**
- Modify: `Dockerfile.rk3566` (apt 加 librime-dev rime-data-luna-pinyin)
- Modify: `.github/workflows/rust.yml` (CI apt 加 librime-dev — rime.rs 在 linux 编译需要头文件/链接)
- Modify: `docs/cross-build.md` (librime 说明 + RIME_DATA_DIR)

- [ ] **Step 1: 改三个文件**
- [ ] **Step 2: 推送后 Actions 手动触发 rk3566-build 验证 (或下次 tag)** — 验证 aarch64 链接 librime 成功
- [ ] **Step 3: 门禁 + 提交** `[功能] call-ui: 键盘交叉构建支持 librime`

## Self-Review 记录

- spec 覆盖: 键盘 UI(T2)/引擎(T1,T4)/stub 门控(T1)/接入范围(T3)/构建(T5)/文档(T5) — 全覆盖; spec 中 systemd unit RIME_DATA_DIR 改为 rime.rs 代码内默认 + env 覆盖, 无需改 unit
- 占位符: 无
- 类型一致: EngineOutput/Mode/FieldId/控制器方法签名在 Task 1 定义, Task 2/3 仅消费; slint 属性表固定
