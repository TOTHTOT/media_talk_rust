# 设计：用 Slint Kit 统一 call-ui 视觉层

## 背景

`crates/call-ui` 当前是 Slint 1.18.1 应用，使用 vendored `material-1.1.0`。`build.rs` 会下载这套组件，并给它的 `TextField` 打一个“光标移到末尾”的补丁，然后用 `slint_build` 编译 `ui/main.slint`。页面里同时存在 material 组件、手写 `Rectangle`/布局和各自的颜色、间距、圆角。

自定义 `ui/drop_down.slint` 的存在原因很具体：material 的 `DropDownMenu` 本体和弹层里各有一个只读 `TextField`；弹层打开后焦点会进入 `TextInput`，触发屏幕键盘，键盘正好盖住向下展开的选项。

文本输入是本项目特有的链路：

- `main.slint` 在 `TextInputInterface.text-input-focused && !KeyboardCtx.collapsed` 时显示键盘。
- `KbTextField` 是唯一消费 `KeyboardCtx.commit-seq` 并把文字追加到输入框的组件。
- Rust `KeyboardController` 负责英文/数字/librime 中文候选，并回写 `kb-*` 属性。
- 任何其他被聚焦的 `TextInput` 都会弹出键盘，但收不到上屏文字。

目标设备是 1024×600 的 RK3566 面板，运行 `SLINT_BACKEND=linuxkms-*` 和软件渲染器（`--features kms`）。当前构建已经在设备上跑通，所以迁移从已知可用的基线开始。

Slint Kit 0.9 是选定的组件库：要求 Rust 1.95（已满足）、edition 2024（工作区已使用）、Slint 1.19.0（比当前高一个小版本）。项目是非商业学习 demo，Kit 的 GPL-3.0-or-later 可以接受。Kit 没有完整屏幕键盘；它的文本组件使用原生 `TextInput` 和系统输入法。

工作区里已有未提交的休眠时间、熄屏、打包脚本等改动。本次实现只修改 UI 迁移相关文件，不提交或覆盖无关工作。

## 目标 / 非目标

**目标：**

- 让 Kit 成为唯一视觉来源：颜色、字体、间距、圆角、阴影、焦点环、禁用/错误状态和动效都来自 `KitTheme`。
- 用 Kit 组件替换手写卡片、按钮、下拉、列表、对话框、toast 和页面壳。
- 保持现有页面结构、导航方式和所有 SIP/ICE/媒体/存储行为不变。
- 让所有仍然存在的文本输入继续使用屏幕键盘和中文输入。
- 保证非文本控件永远不弹出屏幕键盘。
- 每个阶段都在 Windows 和 1024×600 linuxkms-software 上验证。
- 最终删除 `material-1.1.0`、它的下载/补丁逻辑和 `ui/drop_down.slint`。

**非目标：**

- 本次不把文本输入迁移到平台 IME，也不接入 Kit 原生文本框。
- 不修改 SIP、ICE、RTP、摄像头、天气、持久化或熄屏控制逻辑。
- 不重新设计导航或信息架构。
- 不做商业授权或闭源分发处理。
- 不追求使用 Kit 的所有组件，只迁移 call-ui 实际用到的部分。

## 决策

### D1：同时锁定 Slint 1.19.0 和 Slint Kit 0.9.0

使用精确版本：

```toml
[dependencies]
slint = { version = "=1.19.0", default-features = false, features = [
    "std",
    "backend-winit",
    "renderer-femtovg",
    "compat-1-2",
] }
i-slint-core = { version = "=1.19.0", features = ["svg"] }
slint-kit = "=0.9.0"

[build-dependencies]
slint-kit-build = "=0.9.0"
slint-build = "=1.19.0"
```

`kms` feature 保持不变：继续加 `slint/backend-linuxkms-noseat` 和 `slint/renderer-software`。桌面端不切换到 Skia；Kit 文档也建议保留应用自己的后端和渲染器。

`slint-build` 继续作为显式构建依赖，因为 `slint_kit_build::config()` 返回 `slint_build::CompilerConfiguration`，项目还需要使用 `DefaultTranslationContext::None`。编译器和运行时必须保持同一个补丁版本。

**理由：** Kit 0.9 要求 Slint 1.19.0；Kit 还是 pre-1.0，浮动版本会让构建不可复现。

**考虑过的替代方案：**

- 留在 Slint 1.18.1：无法使用 Kit 0.9。
- 用 Slint `std-widgets`：没有 Kit 的 typed choices、弹层定位和焦点管理，也不是用户想要的统一视觉系统。
- vendor Kit 源码：省掉版本升级，但增加很大的维护面，不适合学习 demo。
- 只把 Kit 当视觉参考：违背“不要再手写控件”的目标。

### D2：构建改用 `slint_kit_build`，material 已删除

material 已在本变更中删除，`build.rs` 只保留 `fetch_icons()`。最终编译步骤改成：

```rust
let config = slint_kit_build::config()
    .with_bundled_translations("lang")
    .with_default_translation_context(slint_build::DefaultTranslationContext::None);
slint_kit_build::compile_with_config("ui/main.slint", config)
    .expect("slint compile failed");
```

`fetch_material()`、`patch_material_text_field()` 和 `zip` 构建依赖已随最后一个 material import 一起删除。

在 `src/lib.rs` 中，每个窗口在 `MainWindow::new()` 之后、接线回调和 `ui.run()` 之前调用一次 `slint_kit::install(&ui)`；另一个窗口创建路径（截图/测试入口）也必须调用。

**理由：** `@kit` import 和共享的 Rust/Slint 契约需要 Kit 构建助手；Kit 的服务负责弹层定位、焦点、choices 和主题全局，文档要求每个窗口安装一次。

**风险：** `slint_kit_build::config()` 必须保留 bundled translations 和“无 context”的翻译设置。Windows 已通过中英文切换验证；如果后续升级 Kit 后失效，应先修构建配置或改用 Kit 支持的翻译路径，不能丢 i18n。

### D3：一个窗口主题，Kit 是唯一来源

使用 Kit 自带主题，不手写调色板：

- 窗口初始化时，从一个应用级设置决定 `KitThemeContext.current = KitThemes.light` 或 `KitThemes.dark`。
- 本次实现已经把 material 全部删除，因此不再需要 `Palette.color-scheme` 过渡同步；窗口只保留 `KitThemeContext`。
- 字体大小使用 `KitMetrics.font-size(theme, TypographyRole.body)` 和 Kit 的排版角色。不要 import `InterFont`；不导入时 Kit 会使用应用字体，能保留中文字形，也避免向设备塞 ~1.6MB 的拉丁字体。
- 不添加逐页颜色/圆角/间距覆盖。确需例外时，只改一次主题字段，或使用 Kit 组件公开的布局属性（`corner-radius`、`control-size`）。
- 初始使用 `KitThemes.light`。深色模式保留为一个开关，最终默认值在设备上看过再定。
- 如果 Kit 阴影或动效在软件渲染上太重，只在主题层改一次（降低 elevation 或设置 `reduced-motion`），不要逐页打补丁。

**理由：** 单一主题值就是这次变更的核心；material 删除后，Kit 主题是唯一视觉来源。

**替代方案：** 自定义 `KitTheme` 字面量做品牌色。暂时不做：需要复制所有字段，学习 demo 没必要；以后可以复制官方主题后改命名字段。

### D4：保留 `KbTextField` + `KeyboardCtx`，禁止原生 Kit 文本控件

本次迁移中，唯一的文本输入路径是：

```text
KbTextField（裸 TextInput + Kit 主题字段壳）
  -> TextInputInterface.text-input-focused
  -> 显示屏幕键盘
  -> KeyboardCtx.commit-seq
  -> KbTextField 追加文字
```

规则：

- 只使用不包含可编辑 `TextInput` 的 Kit 控件：`KitCard`、`KitButton`、`KitIconButton`、`KitSelect`（`searchable: false`）、`KitDropdownMenu`、`KitSlider`、`KitSwitch`、`KitTabs`、`KitBadge`、`KitAlert`、`KitToast`、`KitContentList`（非编辑行）、`KitDialogSurface` 等。
- 在 `KbKitTextField` 适配器验证通过前，不使用 `KitCombobox`、`KitTextField`、`KitSearchField`、`KitPasswordField`、`KitEmailField`、`KitOTPField` 等任何内嵌可编辑原生编辑器的 Kit 组件。
- 可能包含 `KbTextField` 的对话框必须使用窗口内的 `KitDialogSurface`/模态表面，不能用可能创建独立原生窗口的 `KitDialog`。屏幕键盘是主窗口内的覆盖层，盖不住原生对话框窗口。纯确认类对话框可以用 `KitDialog`/`KitAlertDialog`。
- 本次已把 `KbTextField` 重写成“Kit 主题字段壳 + 裸 `TextInput`”，它仍然是唯一的 OSK 专用控件，不是新的视觉体系；material `TextField` 和光标补丁已删除。
- 保留点击空白收起键盘（`KeyboardCtx.collapsed = true` + 焦点汇）和设置页 `lift-needed` 抬升逻辑。

**理由：** Kit 文本框会弹出我们的键盘，但收不到 `commit-seq`，等于死输入框；这比暂时的视觉不一致更糟。避免原生文本控件也从根本上消除 material `DropDownMenu` 那类“弹层焦点进 TextInput”的问题。

**考虑过的替代方案：**

- 包一层 `KitTextField` 并往它的 `text` 追加：部分组件暴露 `text`，但光标、选区、校验草稿和 combobox 的 query 语义各不相同，未验证且范围太大。
- 做平台级 IME：长期正确，但是一次独立的架构改造。
- 永远保留 material 文本框：与“删除 material”的目标冲突。

### D5：组件映射

| 当前元素 | Kit 替换 | 说明 |
|---|---|---|
| 页面背景/内容表面 | `KitAppShell`、`KitSurface` | 保留现有“顶部状态栏 + 页面区域”，不引入新的侧边栏/导航结构。 |
| 卡片（`FilledCard`、手写卡片） | `KitCard` + `KitCardHeader`/`KitCardBody`/`KitCardFooter` | 所有卡片使用同一套配方。 |
| 文本/描边按钮 | `KitButton` + `ButtonIntent` | 用语义 intent（`primary`、`secondary`、`outline`、`destructive`、`quiet`），不要写颜色。 |
| 图标按钮 | `KitIconButton` | 图标来自 `@kit-icons`；全部迁完前可继续用现有 `ui/icons`。 |
| 休眠时间下拉 | `KitSelect`，`searchable: false` | token 复用现有下标；`chosen(row)` 映射回 `set-sleep-timeout-index`。 |
| 后续菜单/操作 | `KitDropdownMenu` | 没有原生编辑器，OSK 安全。 |
| 亮度滑块 | `KitSlider` | 保留“松手才写硬件”的行为。 |
| 现有开关 | `KitSwitch` | 只在页面本来就有开关的地方使用。 |
| 联系人/通话记录列表 | `KitContentList`、`KitDataTable` | 不手写行背景；行高至少 48px。 |
| 删除/挂断确认 | `KitAlertDialog` 或 `KitDialogSurface` | 无文本框可用窗口级对话框。 |
| Toast 队列 | `KitToastHost` + `KitToast` | 保留现有队列和 3 秒节奏。 |
| 拨号盘 | API 合适时用 `KitKeypad`；否则用 `KitButton` 网格 | 不手绘按键表面。 |
| 视频/监控画面 | 现有 `Image`/`Rectangle` | 视频渲染不归 Kit 管；覆盖层控件用 Kit。 |
| 文本输入 | `KbTextField` | 唯一的 OSK 例外。 |

设置页迁移并通过设备验证后，删除 `ui/drop_down.slint`。它之前的 P0/P1 修复对过渡期有用，但不应该活到迁移结束。

### D6：直接替换，按页面分阶段落地

不做一次性 spike 页面。设置页就是第一个真实验证页；它编译、上 Windows、上设备都通过后，再继续后面的页面。

1. **依赖、构建、主题基础 + 设置页。** 升级 Slint/Kit，改 `build.rs`，安装 Kit，建立窗口主题，迁移设置页的卡片、按钮、滑块、休眠下拉和 SIP/ICE 卡片外观。文本字段继续 `KbTextField`。这一阶段同时验证 KitSelect 弹层、CJK、翻译和 KMS 渲染。
2. **应用壳、状态栏、toast。** 替换状态栏背景、账户/时间/熄屏按钮、页面容器、toast 和顶部错误条；保留页面切换、键盘抬升和通话全屏隐藏。
3. **首页、拨号、通讯录。** 替换卡片、统计块、拨号盘、联系人/通话记录行和编辑表单；文本字段继续 `KbTextField`。
4. **监控、通话浮层。** 视频画面不变，只替换覆盖层按钮、状态标签和对话框。
5. **清理。** 删除 material import、下载/补丁、`zip` 依赖、`drop_down.slint` 和不再使用的图标；更新文档。

每个阶段单独提交，并且都要保持 `cargo check`、`clippy -D warnings` 和现有 63 个库测试通过。每个阶段结束后应用都必须能启动；不允许出现“迁移到一半跑不起来”的长期分支。

**理由：** 一次性全改无法判断回归来自 Slint 1.19、Kit 主题、OSK 还是页面重写。分阶段提交也方便设备回滚。

### D7：验证是变更的一部分

每个阶段运行：

```text
cargo fmt --check -p call-ui
cargo clippy -p call-ui --all-targets -- -D warnings
cargo test -p call-ui --lib
```

Windows 手工检查：

- 所有页面能打开，1024×600 下无裁切。
- 中英文切换正常，无缺字。
- 每个文本字段都能弹出屏幕键盘；输入、退格、中文候选和“完成”正常；文字落到当前字段。
- 非文本 Kit 控件（下拉、菜单、滑块、开关、标签页、对话框按钮）不弹键盘。
- `KitSelect` 在触发器上方/下方正确展开，保持在窗口内，触摸和键盘都能选，token 映射正确。
- toast/错误条仍然出现并自动消失。
- 通话、监控、SIP/ICE 设置行为不变。

KMS 设备检查：

- 用 `--features kms` 构建，按现有部署方式以 `SLINT_BACKEND=linuxkms-software` 运行。
- 除 Windows 项外，增加触摸输入检查。
- 记录启动时间、页面切换响应、弹层延迟和内存，与迁移前对比。
- 确认中文字形来自设备字体。
- 确认 Kit 阴影/动效不会卡住软件渲染；如果会，按 D3 在主题层统一处理。
- 确认 `KitSelect` 弹层在 KMS 上可用；如果不可用，设置页改用内联 `KitSelectSurface`，不要重新引入自定义下拉。

设置页第一次上板的结果决定后续页面是否继续；有问题先在这一页解决。

### D8：回滚靠 revert，不做运行时开关

工作留在 `ui/slint-kit` 分支或 worktree，每个阶段一个提交。本次实现已一次性完成页面迁移和 material 清理，因此回滚以提交为单位；后端代码不参与 UI 提交。没有运行时 feature flag：二进制要么是新 UI，要么是旧 UI。如果 KMS 上 Kit 弹层/对话框确实不可用，回滚对应 UI 提交并保留当前基线；SIP/ICE 代码不参与。

## 风险 / 取舍

| 风险 | 缓解 |
|---|---|
| Slint Kit 是 pre-1.0，API 可能变化。 | 精确锁版本，提交 `Cargo.lock`；升级必须单独做。 |
| Slint 1.19 与旧 material 不兼容。 | 本次已经删除 material 和光标补丁，`KbTextField` 改为裸 `TextInput`，不再承担这套兼容性。 |
| Kit 弹层可能使用独立原生窗口，KMS 不支持。 | Windows 已验证 `KitSelect` 弹层；KMS 上板时验证，失败则用页面内联的 `KitSelectSurface`，不写自定义下拉。 |
| Kit 对话框可能是独立窗口，窗口内 OSK 盖不住。 | 含文本的对话框用 `KitDialogSurface`；纯确认用 `KitDialog`，两种都测。 |
| Kit 阴影/模糊/动效在软件渲染上开销大。 | 设置页上板时测量；必要时在主题层设置 `reduced-motion` 或降低 elevation，不逐页调。 |
| 不引入 Inter 会导致拉丁字体和 Kit 截图不完全一致。 | 有意为之：优先中文字形覆盖。设备字体缺字时再打包 CJK 字体，而不是 import Inter。 |
| `slint_kit_build` 可能不保留 bundled translations。 | Windows 已验证中英文切换；`休眠时间: 秒` 和 `永不` 已补 en.po。 |
| material 删除后仍有旧样式残留。 | 已全局搜索并移除 material import；`cargo clean -p call-ui` 后重建通过。 |
| Kit 的文本组件与当前 OSK 不兼容。 | D4 强制不使用原生 Kit 文本控件；`KbTextField` 是唯一文本路径。 |
| GPL-3.0-or-later 改变二进制分发条款。 | 项目是非商业 demo；记录许可证和第三方声明。不要分发基于 Kit 的闭源二进制。 |
| 手工 UI 检查容易漏。 | 每阶段任务里包含 Windows 和设备的检查项；没记录结果不算完成。 |

## 迁移计划

1. 从当前设备已验证的提交创建 `ui/slint-kit` 分支/worktree。
2. 执行 D1/D2：锁 Slint 1.19、加 Kit、改 `build.rs`、在两条窗口创建路径加 `slint_kit::install`。
3. 执行 D3：建立窗口主题，不引入 Inter，验证中文。本次已直接用 `KitThemeContext`，未保留 `Palette` 过渡。
4. 迁移设置页（D5/D6）：KitCard、KitButton、KitSlider、KitSelect(`searchable:false`)、SIP/ICE 卡片外观；`KbTextField` 已改为 Kit 字段壳 + 裸 `TextInput`。
5. 迁移壳/状态栏/toast。
6. 迁移首页/拨号/通讯录，再迁移监控/通话浮层。
7. 删除 material、下载/补丁、`zip`、`drop_down.slint` 和无用图标；更新文档。
8. 跑完整 Windows + 设备清单，确认后归档变更。

当前实现状态：步骤 1-7 已完成 Windows 构建、UI 截图检查、弹层/OSK 验证和 63 个库测试；KMS 设备实机和最终提交尚未完成。

回滚：revert 对应阶段提交。如果依赖升级本身不可行，revert 设置页提交并保留当前 UI；不牵连后端。

## 待确认问题

- 最终默认主题用浅色还是深色？当前固定浅色；上板后决定是否加窗口级开关。
- `KitSelect` 在 Windows 上已验证可弹出并选中；linuxkms-software 实机仍待验证，不可用则改用内联 `KitSelectSurface`。
- 翻译已通过 Windows 中英文切换验证；`休眠时间: 秒` 和 `永不` 已补 en.po。
- 含 `KbTextField` 的摄像头对话框已改用窗口内 `KitDialogSurface`；设备上的 OSK 覆盖仍待实机确认。
- `KbTextField` 已重写为“裸 `TextInput` + Kit 字段壳”，material `TextField` 和光标补丁已删除。
- 页面迁移已完成，建议先评审 Windows 效果，再安排 KMS 实机验证和提交。
