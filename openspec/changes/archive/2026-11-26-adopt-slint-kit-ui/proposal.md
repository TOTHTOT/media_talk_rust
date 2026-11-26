## 为什么

call-ui 目前混用了 vendored `material-1.1.0`、手写卡片/下拉/对话框/列表行，以及各页面自己的颜色、间距和圆角。结果是视觉不统一、交互状态不一致，每个控件都要单独维护。自定义的 `OptionDropDown` 本质上只是为了绕开 material `DropDownMenu` 内部隐藏的 `TextField` 触发屏幕键盘的问题。

Slint Kit 提供了一套统一的主题、组件和交互行为；这个项目是非商业学习 demo，GPL-3.0-or-later 可以接受。当前 UI 已经在 1024×600 的 linuxkms-software 设备上验证可用，适合作为迁移基线。

## 变更内容

- 采用 Slint Kit 0.9（`slint-kit`、`slint-kit-build`）作为统一组件库。
- **破坏性变更**：Slint 1.18.1 升级到 1.19.0，同步 `i-slint-core`；`build.rs` 改用 `slint_kit_build::compile_with_config`，每个窗口调用一次 `slint_kit::install`。
- 用 Kit 组件替换手写的卡片、按钮、下拉、滑块、开关、列表、对话框、toast 和页面壳。
- 用 `KitTheme` 作为唯一视觉来源，不再逐页手调颜色、圆角、间距和阴影。
- 直接按页面迁移：设置页 → 应用壳/状态栏 → 首页/拨号/通讯录 → 监控/通话。不再单独做一次性 spike 页面，设置页就是第一个真实验证页。
- 文本输入继续使用 `KbTextField` + `KeyboardCtx`；只使用不包含原生 `TextInput` 的 Kit 控件。`KitSelect` 必须 `searchable: false`，暂不使用 `KitCombobox`/`KitTextField`。
- 用 Kit 主题值重绘 `KbTextField` 的外观，保证唯一保留的自定义输入控件不破坏整体风格。
- 每个阶段都在 Windows 和 1024×600 linuxkms-software 上验证；迁移完成后删除 `material-1.1.0`、下载/补丁逻辑和 `drop_down.slint`。

**非目标**

- 不修改 SIP/ICE/媒体/存储/后端逻辑。
- 不把屏幕键盘改成平台级 IME，也不在本次迁移中接入 Kit 的原生文本框。
- 不做商业授权相关处理（项目为非商业 demo）。
- 不在 Kit 主题之外做额外的视觉重设计。

## 能力

### 新增能力

- `call-ui-design-system`：统一 Slint Kit 组件、主题令牌、分阶段页面迁移、屏幕键盘兼容性和目标设备验证。

### 修改能力

<!-- 无：现有 specs 覆盖流媒体/SIP/媒体，不涉及 call-ui 视觉要求。 -->

## 影响

- 影响代码：`crates/call-ui/Cargo.toml`、`crates/call-ui/build.rs`、`crates/call-ui/ui/**/*.slint`、`crates/call-ui/src/lib.rs`（Kit 安装）、`crates/call-ui/ui/kb_text_field.slint` 和 UI 文档。
- 依赖：Slint 1.19.0、`slint-kit`/`slint-kit-build` 0.9.0；Rust 1.95.0 已满足要求。
- 构建：`slint_kit_build` 替换现有 `slint_build` 编译入口；material 的下载/补丁只在最后一个 material import 消失后删除。
- 风险：Slint Kit 仍是 pre-1.0；linuxkms-software 不在 Kit 的官方验证范围内；文本输入仍是唯一需要特殊适配的 OSK 集成点。
