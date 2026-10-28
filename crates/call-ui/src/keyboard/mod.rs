//! 屏幕键盘控制器: 按键路由 + 焦点注册 + 提交派发.
//!
//! slint 回调 (*_tapped) 驱动引擎并把输出缓存为 pending; wiring 经
//! take_output 取走后回写 slint 属性 (见计划 Task 2 属性表). 控制器不碰
//! 输入框本体, 上屏由 KbTextField 监听属性拍振完成.
//!
//! 无聚焦输入框时丢弃上屏/删框指令 (没有输入框不该上屏), 但保留预编辑与
//! 候选, 保证键盘 UI 状态一致.

mod engine;
#[cfg(target_os = "linux")]
mod rime;
#[cfg(not(target_os = "linux"))]
mod stub;

pub(crate) use engine::{Engine, EngineOutput, Mode};

#[cfg(target_os = "linux")]
use rime::create_engine;
#[cfg(not(target_os = "linux"))]
use stub::create_engine;

/// 输入框标识: 聚焦时由 wiring 注册, 控制器只关心有无焦点.
pub(crate) type FieldId = u32;

/// 已接入屏幕键盘的输入框标识 (Task 3), 与 slint 侧 KbTextField field-id
/// 字面量一一对应; 控制器不区分子段, 这里只为日志可读性维护命名表.
pub(crate) const FIELD_CITY: FieldId = 1;
pub(crate) const FIELD_WEATHER_KEY: FieldId = 2;
pub(crate) const FIELD_CONTACT_NAME: FieldId = 3;
pub(crate) const FIELD_CONTACT_NUMBER: FieldId = 4;
pub(crate) const FIELD_CAM_NAME: FieldId = 5;
pub(crate) const FIELD_CAM_URI: FieldId = 6;
pub(crate) const FIELD_CAM_USER: FieldId = 7;
pub(crate) const FIELD_CAM_PASS: FieldId = 8;

/// field-id 的标识名, 仅聚焦登记日志用 (见 wiring.rs).
pub(crate) fn field_name(id: FieldId) -> &'static str {
    match id {
        FIELD_CITY => "city",
        FIELD_WEATHER_KEY => "weather-key",
        FIELD_CONTACT_NAME => "contact-name",
        FIELD_CONTACT_NUMBER => "contact-number",
        FIELD_CAM_NAME => "cam-name",
        FIELD_CAM_URI => "cam-uri",
        FIELD_CAM_USER => "cam-user",
        FIELD_CAM_PASS => "cam-pass",
        _ => "unknown",
    }
}

/// 键盘控制器: slint 回调 → 引擎 → 缓存输出, 供 wiring 拍振取走.
pub(crate) struct KeyboardController {
    engine: Box<dyn Engine>,
    focus: Option<FieldId>,
    pending: Option<EngineOutput>,
}

impl Default for KeyboardController {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyboardController {
    /// 用平台引擎创建控制器 (linux=rime 占位, 其他平台=直通 stub).
    pub fn new() -> Self {
        Self {
            engine: create_engine(),
            focus: None,
            pending: None,
        }
    }

    /// 注入指定引擎, 仅测试用 (MockEngine 驱动控制器断言).
    #[cfg(test)]
    fn with_engine(engine: Box<dyn Engine>) -> Self {
        Self {
            engine,
            focus: None,
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

    /// 注册当前聚焦输入框; None 表示无输入框聚焦 (键盘不收提交).
    pub fn set_focus(&mut self, field: Option<FieldId>) {
        self.focus = field;
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
        if self.focus.is_none() {
            out.commits.clear();
            out.delete_field = false;
        }
        self.pending = Some(out);
    }
}
