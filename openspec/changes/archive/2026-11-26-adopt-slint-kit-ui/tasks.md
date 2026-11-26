## 1. 基线与分支

- [x] 1.1 从当前设备已验证的提交创建 `ui/slint-kit` 分支或 worktree；如果工作区有未提交改动，先记录文件清单。
- [x] 1.2 检查现有未提交改动（`general_card.slint`、`drop_down.slint`、打包脚本、熄屏相关文件等），只修改 UI 迁移需要的文件，不提交无关工作。
- [x] 1.3 记录基线：提交 hash、Windows 构建命令、设备后端命令、测试数量和当前已知 UI 问题。
- [x] 1.4 确认 `openspec validate adopt-slint-kit-ui` 通过后再开始实现。

## 2. 依赖与构建接入

- [x] 2.1 更新 `crates/call-ui/Cargo.toml`：锁定 `slint = "=1.19.0"`、`i-slint-core = "=1.19.0"`，加入 `slint-kit = "=0.9.0"`；构建依赖锁定 `slint-kit-build = "=0.9.0"` 和 `slint-build = "=1.19.0"`；保留现有后端/渲染器 feature 和 `kms` feature。
- [x] 2.2 更新 `crates/call-ui/build.rs`：用 `slint_kit_build::config()` 配置 bundled translations 和 `DefaultTranslationContext::None`，再用 `slint_kit_build::compile_with_config("ui/main.slint", config)`；material/图标下载逻辑暂时保留。
- [x] 2.3 在 `src/lib.rs` 的每条 `MainWindow` 创建路径中调用一次 `slint_kit::install(&ui)`，位置在接线回调和 `ui.run()` 之前。
- [x] 2.4 运行 `cargo check -p call-ui --all-targets`，修复 Slint 1.19 / Kit 构建错误；确认 vendored material 和它的光标补丁仍能编译。
- [x] 2.5 确认 lockfile 只解析到 Slint 1.19.0、Kit 0.9.0 和匹配的编译器/运行时补丁版本。
- [x] 2.6 验证 Kit 构建路径下的 bundled translations 仍然生效；如果不生效，先解决翻译配置再继续。

## 3. 主题基础

- [x] 3.1 建立窗口级 `KitThemeContext` 浅色主题源；深色开关和 reduced-motion 留作后续设备调优项。
- [x] 3.2 把窗口背景、默认字号和排版角色改为 `KitTheme`/`KitMetrics`；已迁移表面不再使用页面级调色板常量。
- [x] 3.3 不引入 `InterFont`，继续使用应用/设备字体；在 Windows 和设备上检查中文字形。
- [x] 3.4 验证浅色主题下 Kit 页面风格一致（material 已删除，不再需要双主题同步）。
- [x] 3.5 提交主题基础。

## 4. 设置页迁移（首个真实验证页）

- [x] 4.1 用 `KitCard` + 头部/主体/底部替换设置页的 `FilledCard` 和手写分隔线。
- [x] 4.2 语言按钮改为 `KitButton`，保留 `set-language("zh"|"en")` 回调。
- [x] 4.3 亮度控件改为 `KitSlider`，保留松手才写硬件和提示 toast 的行为。
- [x] 4.4 休眠时间 `OptionDropDown` 改为 `KitSelect`（`searchable: false`），用现有下标作为 token，`chosen(row)` 映射到 `set-sleep-timeout-index`。
- [x] 4.5 天气城市/key、SIP/ICE 文本字段继续用 `KbTextField`；用 Kit 字段标签/说明包住它们，保留 input-type 和密码遮蔽。
- [x] 4.6 SIP/ICE 卡片周围的按钮、状态文字和反馈改为 Kit 组件，不改任何 Rust 属性或回调。
- [x] 4.7 删除 `OptionDropDown` 的 import/使用，并在确认对等后删除 `crates/call-ui/ui/drop_down.slint`。
- [x] 4.8 删除设置页不再使用的 material import；material 构建下载只保留给其他未迁移页面。
- [x] 4.9 Windows 检查：设置持久化、中英文切换、亮度提示、休眠下拉、每个文本字段的 OSK、非文本控件不弹键盘。
- [ ] 4.10 设备检查：同样的流程加触摸、中文字形、弹层边界和交互延迟；记录结果。设置页不通过则先修复，不继续下一页。
- [x] 4.11 提交设置页迁移。

## 5. 应用壳、状态栏与反馈

- [x] 5.1 用 Kit 表面、文字角色和图标/文字按钮替换状态栏背景、账户/时间/熄屏控制。
- [x] 5.2 用 Kit 的 canvas/surface 配方替换页面容器背景。
- [x] 5.3 用 `KitToastHost`/`KitToast` 和 `KitAlert` 替换 toast 队列和顶部错误条，保留现有 3 秒节奏和消息类型。
- [x] 5.4 保留页面切换、键盘抬升、点击空白收起键盘和通话全屏隐藏。
- [ ] 5.5 在 Windows 和设备上检查，提交壳层迁移。

## 6. 首页、拨号与通讯录

- [x] 6.1 首页卡片、统计块和头像改为 `KitCard`/`KitStat`/`KitAvatar`，保留数据绑定和回调。
- [x] 6.2 手绘拨号盘改为 `KitKeypad`（如果 API 满足拨号/退格/呼叫）或 `KitButton` 网格；保留拨号、退格和呼叫动作。
- [x] 6.3 联系人和通话记录行/列表外观改为 Kit 列表/表格组件和 `KitAvatar`，保留条目身份和回调。
- [x] 6.4 联系人搜索/编辑文本字段继续用 `KbTextField`；含文本的编辑框用窗口内 `KitDialogSurface`，验证 OSK。
- [ ] 6.5 在 Windows 和设备上检查新增/编辑/删除联系人、拨号和通话记录流程，提交本阶段。

## 7. 监控与通话覆盖层

- [x] 7.1 监控页卡片、状态标签和操作按钮改为 Kit 组件；视频 `Image`/帧接收链路不变。
- [x] 7.2 通话页覆盖按钮、标签和状态胶囊改为 Kit 组件；视频表面和全屏行为不变。
- [x] 7.3 验证来电、去电、接听、挂断、静音和摄像头操作仍然调用相同后端操作。
- [ ] 7.4 在 Windows 和设备上检查，提交本阶段。

## 8. 清理与最终验证

- [x] 8.1 删除 `crates/call-ui/ui` 中所有 `material-1.1.0` import。
- [x] 8.2 删除 `fetch_material()`、`patch_material_text_field()`、`zip` 构建依赖和 `build.rs` 中过时的 material 说明。
- [x] 8.3 删除 `ui/drop_down.slint` 和只有 material UI 使用的图标/资源。
- [x] 8.4 更新 `README`/UI 文档和设备构建说明，写明 Slint 1.19 + Slint Kit。
- [x] 8.5 运行 `cargo fmt --check`、`cargo clippy -p call-ui --all-targets -- -D warnings` 和 `cargo test -p call-ui --lib`。
- [x] 8.6 在不需要 material 下载步骤的情况下做一次干净构建，确认 UI 不再依赖它。
- [ ] 8.7 跑完整的 Windows 和 1024×600 linuxkms-software 检查清单，记录最终结果和接受的限制。
- [x] 8.8 请用户确认最终状态后再归档 OpenSpec 变更。

> 归档已知限制: 本次归档前在 Windows 上完成了构建与界面复验；1024×600 linuxkms-software 设备复验未执行，作为已接受限制记录。

## 9. 编辑器预览

- [x] 9.1 配置 Slint 编辑器预览的 `@kit` library paths：VS Code 用 `.vscode/settings.json`，Zed 用 `.zed/settings.json` 的 `-L kit=...` / `kit-icons` / `kit-fonts`，`binary.path` 使用项目相对路径 `target/slint-editor/bin/slint-lsp.exe`，并用 `cargo vendor --locked target/slint-editor/vendor` 生成可被预览器读取的 Kit 源码。
