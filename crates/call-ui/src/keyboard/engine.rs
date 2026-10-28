//! 键盘引擎抽象: 纯逻辑 trait, 与平台实现 (librime / 直通 stub) 解耦.
//!
//! 引擎只回答"这次按键产生了什么" (预编辑/候选/上屏), 不关心输入框;
//! 焦点判断与提交派发由控制器 (mod.rs) 负责.

/// 输入模式: 中文拼音 / 英文直出 / 数字符号.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// 中文拼音: 字母累积成预编辑串, 出候选.
    Zh,
    /// 英文: 按键原样上屏.
    En,
    /// 数字与符号: 按键原样上屏.
    Num,
}

/// 引擎输出快照: 一次按键/操作后的完整键盘状态.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EngineOutput {
    /// 预编辑拼音串 (中文模式).
    pub preedit: String,
    /// 当前候选词列表.
    pub candidates: Vec<String>,
    /// 需上屏文本, 按序追加到输入框末尾.
    pub commits: Vec<String>,
    /// 退格且预编辑为空: 请求删除输入框末字符.
    pub delete_field: bool,
}

/// 键盘引擎: 按键/退格/选候选/空格翻译成输出快照, 由控制器缓存派发.
pub(crate) trait Engine {
    /// 当前模式.
    fn mode(&self) -> Mode;
    /// 切换模式 (清空预编辑与候选).
    fn set_mode(&mut self, mode: Mode);
    /// 字符键: 拼音字母或直出字符.
    fn key(&mut self, k: char) -> EngineOutput;
    /// 退格: 预编辑非空则缩短, 为空则请求删输入框末字符.
    fn backspace(&mut self) -> EngineOutput;
    /// 选择第 idx 个候选并上屏.
    fn select(&mut self, idx: usize) -> EngineOutput;
    /// 空格: 有候选则上首候选, 否则上空格.
    fn space(&mut self) -> EngineOutput;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::KeyboardController;

    /// 模拟拼音引擎: 固定音节表, 其他字母只累积进预编辑不出候选.
    struct MockEngine {
        mode: Mode,
        preedit: String,
        candidates: Vec<String>,
    }

    impl MockEngine {
        fn new() -> Self {
            Self {
                mode: Mode::Zh,
                preedit: String::new(),
                candidates: Vec::new(),
            }
        }

        /// 按固定音节表刷新候选: 整串命中才出词, 否则无候选.
        fn refresh_candidates(&mut self) {
            self.candidates = match self.preedit.as_str() {
                "xi" => vec!["西".into(), "喜".into(), "细".into()],
                "an" => vec!["安".into(), "岸".into()],
                _ => Vec::new(),
            };
        }

        fn snapshot(&self) -> EngineOutput {
            EngineOutput {
                preedit: self.preedit.clone(),
                candidates: self.candidates.clone(),
                ..Default::default()
            }
        }
    }

    impl Engine for MockEngine {
        fn mode(&self) -> Mode {
            self.mode
        }

        fn set_mode(&mut self, mode: Mode) {
            self.mode = mode;
            self.preedit.clear();
            self.candidates.clear();
        }

        fn key(&mut self, k: char) -> EngineOutput {
            if self.mode == Mode::Zh {
                self.preedit.push(k);
                self.refresh_candidates();
                self.snapshot()
            } else {
                // 英文/数字: 按键原样上屏
                EngineOutput {
                    commits: vec![k.to_string()],
                    ..Default::default()
                }
            }
        }

        fn backspace(&mut self) -> EngineOutput {
            if self.preedit.pop().is_some() {
                self.refresh_candidates();
                self.snapshot()
            } else {
                // 预编辑已空: 请求控制器删输入框末字符
                EngineOutput {
                    delete_field: true,
                    ..Default::default()
                }
            }
        }

        fn select(&mut self, idx: usize) -> EngineOutput {
            let Some(text) = self.candidates.get(idx).cloned() else {
                return self.snapshot();
            };
            self.preedit.clear();
            self.candidates.clear();
            EngineOutput {
                commits: vec![text],
                ..Default::default()
            }
        }

        fn space(&mut self) -> EngineOutput {
            if self.mode == Mode::Zh && !self.candidates.is_empty() {
                self.select(0)
            } else {
                EngineOutput {
                    commits: vec![" ".to_string()],
                    ..Default::default()
                }
            }
        }
    }

    /// 注入 MockEngine 的控制器 (默认已聚焦输入框), 全部用例的起点.
    fn controller() -> KeyboardController {
        let mut c = KeyboardController::with_engine(Box::new(MockEngine::new()));
        c.set_focus(Some(1));
        c
    }

    #[test]
    fn en_key_commits_directly() {
        let mut c = controller();
        c.mode_tapped(1);
        c.key_tapped("a");
        let out = c.take_output().unwrap_or_default();
        assert_eq!(out.commits, vec!["a"]);
        assert!(out.preedit.is_empty());
    }

    #[test]
    fn zh_keys_build_preedit_and_candidates() {
        let mut c = controller();
        c.key_tapped("x");
        let out = c.take_output().unwrap_or_default();
        assert_eq!(out.preedit, "x");
        assert!(out.candidates.is_empty());
        c.key_tapped("i");
        let out = c.take_output().unwrap_or_default();
        assert_eq!(out.preedit, "xi");
        assert_eq!(out.candidates, vec!["西", "喜", "细"]);
        assert!(out.commits.is_empty());
    }

    #[test]
    fn zh_space_commits_first_candidate() {
        let mut c = controller();
        c.key_tapped("x");
        c.key_tapped("i");
        c.take_output();
        c.space_tapped();
        let out = c.take_output().unwrap_or_default();
        assert_eq!(out.commits, vec!["西"]);
        assert!(out.preedit.is_empty());
        assert!(out.candidates.is_empty());
    }

    #[test]
    fn zh_backspace_shortens_preedit_then_requests_field_delete() {
        let mut c = controller();
        c.key_tapped("x");
        c.key_tapped("i");
        c.take_output();
        c.backspace_tapped();
        let out = c.take_output().unwrap_or_default();
        assert_eq!(out.preedit, "x");
        assert!(!out.delete_field);
        // 清空末个拼音字母的退格被预编辑消耗, 不删输入框字符
        c.backspace_tapped();
        let out = c.take_output().unwrap_or_default();
        assert!(out.preedit.is_empty());
        assert!(!out.delete_field);
        // 预编辑已空再退格: 请求删输入框末字符
        c.backspace_tapped();
        let out = c.take_output().unwrap_or_default();
        assert!(out.delete_field);
    }

    #[test]
    fn no_focus_swallows_commits_and_field_delete() {
        let mut c = controller();
        c.mode_tapped(1); // 英文直出才有 commits 可吞
        c.set_focus(None);
        c.key_tapped("a");
        let out = c.take_output().unwrap_or_default();
        assert!(out.commits.is_empty());
        c.backspace_tapped();
        let out = c.take_output().unwrap_or_default();
        assert!(!out.delete_field);
        // 聚焦后同样按键正常派发
        c.set_focus(Some(7));
        c.key_tapped("b");
        let out = c.take_output().unwrap_or_default();
        assert_eq!(out.commits, vec!["b"]);
    }

    #[test]
    fn mode_switch_to_en_outputs_direct() {
        let mut c = controller();
        c.mode_tapped(1);
        assert_eq!(c.mode(), Mode::En);
        c.key_tapped("x");
        c.key_tapped("i");
        let out = c.take_output().unwrap_or_default();
        assert_eq!(out.commits, vec!["i"]); // 非拼音累积, 末键直出
        assert!(out.preedit.is_empty());
    }

    #[test]
    fn candidate_tap_commits_selected_and_negative_ignored() {
        let mut c = controller();
        c.key_tapped("x");
        c.key_tapped("i");
        c.take_output();
        c.candidate_tapped(-1);
        assert!(c.take_output().is_none()); // 负 idx 不产生输出
        c.candidate_tapped(1);
        let out = c.take_output().unwrap_or_default();
        assert_eq!(out.commits, vec!["喜"]);
    }

    #[test]
    fn take_output_is_idempotent() {
        let mut c = controller();
        c.key_tapped("x");
        assert!(c.take_output().is_some());
        assert!(c.take_output().is_none());
    }
}
