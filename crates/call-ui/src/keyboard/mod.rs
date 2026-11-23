//! 屏幕键盘控制器: 按键路由 + 焦点门控 + 提交派发.
//!
//! slint 回调 (*_tapped) 驱动引擎并把输出缓存为 pending; wiring 经
//! take_output 取走后回写 slint 属性. 控制器不碰输入框本体, 上屏由
//! KbTextField 监听属性拍振完成.
//!
//! 方案 D 后焦点语义只剩布尔: slint 官方 TextInputInterface.text-input-focused
//! 镜像 (wiring::wire_keyboard) 驱动 "有无聚焦框" — 逐字段 ID 登记随显隐/路由
//! 简化整体移除 (身份曾服务路由守卫, 现路由只问有无).
//!
//! 无聚焦输入框时丢弃上屏/删框指令 (没有输入框不该上屏), 但保留预编辑与
//! 候选, 保证键盘 UI 状态一致.

mod engine;
#[cfg(target_os = "linux")]
mod rime;
// linux 下也编译 stub: rime 初始化失败时降级为直通引擎 (见 rime.rs).
mod stub;

pub(crate) use engine::{Engine, EngineOutput, Mode};

#[cfg(target_os = "linux")]
use rime::create_engine;
#[cfg(not(target_os = "linux"))]
use stub::create_engine;

/// 键盘控制器: slint 回调 → 引擎 → 缓存输出, 供 wiring 拍振取走.
pub(crate) struct KeyboardController {
    engine: Box<dyn Engine>,
    focused: bool,
    pending: Option<EngineOutput>,
}

impl Default for KeyboardController {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyboardController {
    /// 用平台引擎创建控制器 (linux=librime, 初始化失败时 rime 内部降级直通;
    /// 其他平台=直通 stub).
    pub fn new() -> Self {
        Self {
            engine: create_engine(),
            focused: false,
            pending: None,
        }
    }

    /// 注入指定引擎, 仅测试用 (MockEngine 驱动控制器断言).
    #[cfg(test)]
    fn with_engine(engine: Box<dyn Engine>) -> Self {
        Self {
            engine,
            focused: false,
            pending: None,
        }
    }

    /// 当前引擎模式 (装配时回写 kb-mode 初值, 测试断言模式切换).
    pub(crate) fn mode(&self) -> Mode {
        self.engine.mode()
    }

    /// 字符键: 取首个字符交给引擎 (slint 一次只传一个键), 输出缓存为 pending.
    pub fn key_tapped(&mut self, key: &str) {
        let Some(k) = key.chars().next() else { return };
        let out = self.engine.key(k);
        self.dispatch(out);
    }

    /// 退格: 预编辑非空缩短拼音, 为空请求删输入框末字符.
    pub fn backspace_tapped(&mut self) {
        let out = self.engine.backspace();
        self.dispatch(out);
    }

    /// 候选点击: idx 为负或越界时忽略 (slint 传 int).
    pub fn candidate_tapped(&mut self, idx: i32) {
        if idx < 0 {
            return;
        }
        let out = self.engine.select(idx as usize);
        self.dispatch(out);
    }

    /// 空格: 有候选上首候选, 否则上空格.
    pub fn space_tapped(&mut self) {
        let out = self.engine.space();
        self.dispatch(out);
    }

    /// 模式切换: 0=zh 1=en 2=num, 其他值忽略; 中文不可用时 0 回退英文.
    /// 不产生上屏文本, 仅通知状态变更.
    pub fn mode_tapped(&mut self, mode: i32) {
        let mode = match mode {
            0 if self.zh_available() => Mode::Zh,
            // 中文引擎不可用 (非 linux): 回退英文直出
            0 | 1 => Mode::En,
            2 => Mode::Num,
            _ => return,
        };
        self.engine.set_mode(mode);
        // 置空输出通知 wiring 回写 kb-mode 等状态
        self.pending = Some(EngineOutput::default());
    }

    /// 注册 "有无聚焦输入框" (slint 官方 text-input-focused 镜像); false 时
    /// 控制器吞掉上屏与删框指令.
    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// 取走自上次调用后的最新输出; 取走即清空, 无新输出时返回 None.
    ///
    /// returns: Option<EngineOutput>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn take_output(&mut self) -> Option<EngineOutput> {
        self.pending.take()
    }

    /// 中文引擎是否可用: 仅 linux 接了 librime (Task 4), 其他平台隐藏中文模式键.
    pub fn zh_available(&self) -> bool {
        cfg!(target_os = "linux")
    }

    /// 缓存引擎输出; 无聚焦输入框时吞掉上屏与删框指令, 保留键盘自身状态.
    fn dispatch(&mut self, mut out: EngineOutput) {
        if !self.focused {
            out.commits.clear();
            out.delete_field = false;
        }
        self.pending = Some(out);
    }
}
