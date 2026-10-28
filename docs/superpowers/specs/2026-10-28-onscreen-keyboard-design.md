# 屏幕键盘 + 中文输入法设计 (librime 内嵌)

日期: 2026-10-28
状态: 已与用户对齐 (全键盘 QWERTY / 纯触屏 / librime 内嵌 / Windows cfg 门控)

## 需求

call-ui 增加屏幕键盘 (OSK), 支持中文拼音输入:
- 全键盘 QWERTY, 纯触屏交互 (无物理按键导航)
- 中文输入走 librime (Rime 核心 C API, 为嵌入而生)
- 板子 (RK3566 armbian, KMS 无桌面) 上可用 — fcitx5/ibus 在无桌面
  环境没有可用前端, 不能走标准输入法栈

## 非目标 (YAGNI)

- 九宫格 T9 (后续如需要再加, 引擎层不变)
- 物理按键/遥控器导航
- Windows 开发机中文输入 (cfg 门控, 见下)
- 自学习/用户词库同步 (rime 默认配置即可)

## 总体架构

```
ui/keyboard.slint                 键盘视图 (纯展示 + 按键/候选回调)
  ├─ 候选栏: 预编辑串 + 横向候选词 (点击上屏)
  ├─ QWERTY 字母区 + 模式键 (中/EN/123) + 退格/空格/回车/完成(收起)
  └─ 数字符号页
        ▼ 回调: key-tapped(string) / candidate-tapped(int) / mode-switch / backspace ...
keyboard/ 模块 (Rust, crates/call-ui/src/keyboard/)
  ├─ engine.rs — 输入法引擎抽象 (纯逻辑, 全部单测在这层)
  │   trait Engine {
  │       fn key(&mut self, k: char) -> EngineOutput;
  │       fn select(&mut self, idx: usize) -> EngineOutput; // 点候选
  │       fn backspace(&mut self) -> EngineOutput;
  │   }
  │   struct EngineOutput { preedit: String, candidates: Vec<String>,
  │                           commits: Vec<String> }
  ├─ rime.rs   — librime 实现 (cfg linux), 进程内单例懒初始化
  └─ stub.rs   — 直通引擎 (cfg not(linux)): 字母/数字直出, 中文模式键隐藏
        ▼ 只更新键盘可视状态: 预编辑串/候选列表/模式 → slint 属性
KbTextField (slint 组件, 包装 material TextField)
  └─ 持有焦点时消费上屏文本, 直接改自身 text/cursor-position
```

**关键取舍: 上屏走 slint 属性通道**。焦点框自己消费上屏文本并改自己的
`text` / `cursor-position` (TextInput 的这两个属性是双向绑定, 官方
virtual keyboard 示例同款做法)。Rust 不碰输入框, 键盘状态是 UI 局部
状态, 不走 UiHandle 事件漏斗 (那条漏斗是后端失败/通知上报的纪律)。

上屏传递机制: root 级 `kb-commit-seq` (计数器) + `kb-commit-text` 属性
对——候选点击时 seq+1 并写 text, 当前聚焦的 KbTextField 发现 seq 变化
时把 text 应用到自身光标处。slint 惯用的事件拍振 (event flap) 模式。

## librime 集成

- 交叉构建 (Dockerfile.rk3566): apt 加 `librime-dev` +
  `rime-data-luna-pinyin` (月拼音词库); rime-sys 薄封装手工声明 C API
  (~30 函数, 不引 bindgen)
- 会话为进程内单例, 首次切到中文模式时初始化 (词库加载数百 ms, 懒加载);
  librime 需要 `RIME_DATA_DIR` 环境变量指向词库目录, systemd unit 与
  cross-build.md 同步更新
- 内存预算: rime + 词库约 30-60MB, RK3566 够用
- focal 的 librime 1.5 API 稳定, 无版本风险

## 平台门控

- `#[cfg(target_os = "linux")]`: rime.rs, 完整中文
- `#[cfg(not(target_os = "linux"))]`: stub.rs, 字母/数字直出, 中文模式键
  隐藏; 键盘 UI 与全部交互逻辑 (退格/光标/候选栏布局) 照常可测
- 板子与 CI (aarch64-linux) 均走 rime 路径, 中文验证在设备上进行

## 键盘 UX (纯触屏)

- 焦点进入任何 KbTextField → 键盘从底部滑出 (约屏高 40%), 全部失焦收起
- 中文模式: 顶行候选栏 = 预编辑串 + 候选词列表; 点候选上屏
- EN 模式: 字母直出到输入框
- 123 模式: 数字 + 常用符号
- 退格: 预编辑非空先删拼音, 空则删输入框文字
- 空格: 预编辑非空 = 选首候选, 否则输入空格
- 完成键 (收起键盘) 与系统返回等价

## 接入范围 (第一期)

设置页 (城市 / 和风 Key)、通讯录 (姓名 / 号码)、手动添加摄像头对话框
(名称 / URI / 账号 / 密码)。拨号盘已有数字键盘, 不动。

## 测试

- engine trait 层: 纯逻辑单测 — 按键流 → (预编辑, 候选, 上屏) 断言
  (mock engine 驱动); 退格边界 (删拼音 vs 删文字)、模式切换
- stub/rime 实现层: 不做单测 (FFI / 平台层)
- slint 视图: 不测试 (项目惯例)

## 文档同步

- `docs/cross-build.md`: apt 包清单 + RIME_DATA_DIR 说明
- `docs/call-ui.md`: 键盘架构一节 (上屏属性通道 + engine 分层)
- `media_talk_rust.service`: Environment=RIME_DATA_DIR
