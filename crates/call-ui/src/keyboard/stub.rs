//! 非 Linux 平台直通引擎: 无 librime, 中文模式回退为英文直出行为.
//!
//! 控制器按平台报告 zh_available()=false, UI 隐藏中文模式键, 因此 Zh
//! 分支实际不可达; 保留与 En/Num 相同的直出逻辑只为防御误调用.

use super::engine::{Engine, EngineOutput, Mode};

/// 创建直通引擎: 按键原样上屏, 无预编辑与候选.
pub(crate) fn create_engine() -> Box<dyn Engine> {
    Box::new(PassthroughEngine::new())
}

struct PassthroughEngine {
    mode: Mode,
}

impl PassthroughEngine {
    fn new() -> Self {
        Self { mode: Mode::En }
    }

    /// 直出: 单字符原样上屏.
    fn direct(&self, k: char) -> EngineOutput {
        EngineOutput {
            commits: vec![k.to_string()],
            ..Default::default()
        }
    }
}

impl Engine for PassthroughEngine {
    fn mode(&self) -> Mode {
        self.mode
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    fn key(&mut self, k: char) -> EngineOutput {
        self.direct(k)
    }

    fn backspace(&mut self) -> EngineOutput {
        // 预编辑恒空: 退格直接请求删输入框末字符
        EngineOutput {
            delete_field: true,
            ..Default::default()
        }
    }

    fn select(&mut self, _idx: usize) -> EngineOutput {
        // 无候选可选
        EngineOutput::default()
    }

    fn space(&mut self) -> EngineOutput {
        EngineOutput {
            commits: vec![" ".to_string()],
            ..Default::default()
        }
    }
}
