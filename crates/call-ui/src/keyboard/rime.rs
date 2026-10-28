//! librime 引擎实现 (仅 Linux 编译).
//!
//! TODO(Task 4): 当前为直通占位, Task 4 替换为 librime 1.5 FFI
//! (RimeTraits / RimeApi 绑定). 占位保证 linux 编译可用且不引入系统库
//! 依赖; 行为与 stub 一致 (中文模式回退英文直出), Task 4 之前 linux 上
//! zh_available() 报 true 但中文输入不可用, 属预期中间态.

use super::engine::{Engine, EngineOutput, Mode};

/// 创建引擎; Task 4 替换为 librime 实现, 当前为直通占位 (见模块文档).
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
