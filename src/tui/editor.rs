//! 行编辑器（§16.2、§21 M5）。
//!
//! 中文 IME 由终端负责；编辑器只维护文本与光标（Unicode 字符边界）。
//! 控制键：←/→ 移动、Home/End、退格/删除、Enter 提交（由 app 处理）、
//! Alt+Enter 换行（多行输入）、↑/↓ 历史浏览。
//!
//! 历史由编辑器自身维护：submit 时入栈，↑/↓ 替换当前文本。

/// 编辑器输入状态。
#[derive(Debug, Clone, Default)]
pub struct Editor {
    pub text: String,
    pub cursor: usize, // 字节偏移，始终在字符边界
    history: Vec<String>,
    history_pos: Option<usize>,
}

/// 历史条数上限（防长会话内存增长）。
const HISTORY_CAP: usize = 100;

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            // 光标前一个字符的起始字节（字符边界，§21 M5：中文 IME 安全）。
            let prev = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let next = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.text.drain(self.cursor..next);
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            let prev = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.cursor = prev;
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            let next = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.cursor = next;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// P2（编辑器增强）：删除光标前一个“词”（空白或标点分隔的连续非空白段）。
    pub fn delete_word_back(&mut self) {
        let before = &self.text[..self.cursor];
        let chars: Vec<(usize, char)> = before.char_indices().collect();
        let mut end = chars.len();
        // 先跳过尾部空白。
        while end > 0 && chars[end - 1].1.is_whitespace() {
            end -= 1;
        }
        // 再删除一个非空白词。
        while end > 0 && !chars[end - 1].1.is_whitespace() {
            end -= 1;
        }
        let new_cursor = chars.get(end).map(|(i, _)| *i).unwrap_or(0);
        self.text.drain(new_cursor..self.cursor);
        self.cursor = new_cursor;
    }

    /// P2（编辑器增强）：删除光标到行尾。
    pub fn delete_to_end(&mut self) {
        self.text.truncate(self.cursor);
    }

    /// P2（编辑器增强）：按“词”向左移动（跳过词间空白，停在词首）。
    pub fn move_word_left(&mut self) {
        let before = &self.text[..self.cursor];
        let chars: Vec<(usize, char)> = before.char_indices().collect();
        let mut i = chars.len();
        while i > 0 && chars[i - 1].1.is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].1.is_whitespace() {
            i -= 1;
        }
        self.cursor = chars.get(i).map(|(pos, _)| *pos).unwrap_or(0);
    }

    /// P2（编辑器增强）：按“词”向右移动（停在下一个词首）。
    pub fn move_word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let chars: Vec<(usize, char)> = after.char_indices().collect();
        let mut i = 0;
        // 跳过当前词。
        while i < chars.len() && !chars[i].1.is_whitespace() {
            i += 1;
        }
        // 跳过词间空白，停在下一个词首。
        while i < chars.len() && chars[i].1.is_whitespace() {
            i += 1;
        }
        self.cursor += chars.get(i).map(|(pos, _)| *pos).unwrap_or(after.len());
    }

    pub fn end(&mut self) {
        self.cursor = self.text.len();
    }

    /// 整行替换（历史浏览、命令补全用）；光标移到末尾。
    pub fn set_text(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.len();
    }

    /// 清空输入。
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// 提交当前输入：trim 后返回，清空编辑区并入历史（去重）。
    pub fn submit(&mut self) -> String {
        let text = self.text.trim().to_string();
        self.clear();
        self.history_pos = None;
        if !text.is_empty() && self.history.last().map(String::as_str) != Some(text.as_str()) {
            self.history.push(text.clone());
            if self.history.len() > HISTORY_CAP {
                let drop = self.history.len() - HISTORY_CAP;
                self.history.drain(..drop);
            }
        }
        text
    }

    /// 历史向上浏览（更旧的一条）。
    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_pos {
            None => Some(self.history.len() - 1),
            Some(i) if i > 0 => Some(i - 1),
            _ => None,
        };
        if let Some(i) = next {
            self.history_pos = Some(i);
            self.set_text(self.history[i].clone());
        }
    }

    /// 历史向下浏览（更新的一条；到底后回到空输入）。
    pub fn history_down(&mut self) {
        let next = match self.history_pos {
            None => None,
            Some(i) if i + 1 < self.history.len() => Some(i + 1),
            Some(_) => None, // 到达最新一条之后再按 → 空输入
        };
        match next {
            Some(i) => {
                self.history_pos = Some(i);
                self.set_text(self.history[i].clone());
            }
            None => {
                self.history_pos = None;
                self.clear();
            }
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_ime_text_edits_at_char_boundaries() {
        let mut editor = Editor::new();
        editor.insert_str("你好世界");
        assert_eq!(editor.text, "你好世界");
        editor.move_left();
        editor.move_left();
        editor.insert_char('中');
        assert_eq!(editor.text, "你好中世界");
        editor.backspace();
        assert_eq!(editor.text, "你好世界");
    }

    #[test]
    fn cursor_moves_never_split_utf8() {
        let mut editor = Editor::new();
        editor.insert_str("a中文b");
        editor.home();
        editor.move_right();
        editor.move_right();
        assert_eq!(&editor.text[..editor.cursor], "a中");
        editor.backspace();
        assert_eq!(editor.text, "a文b");
    }

    #[test]
    fn multi_line_input_survives_edits() {
        let mut editor = Editor::new();
        editor.insert_str("第一行");
        editor.insert_char('\n');
        editor.insert_str("第二行");
        editor.home();
        editor.move_right();
        editor.insert_char('X');
        assert_eq!(editor.text, "第X一行\n第二行");
        editor.backspace();
        assert_eq!(editor.text, "第一行\n第二行");
    }

    #[test]
    fn submit_trims_clears_and_records_history() {
        let mut editor = Editor::new();
        editor.insert_str("  修复这个测试  ");
        assert_eq!(editor.submit(), "修复这个测试");
        assert!(editor.text.is_empty());
        editor.insert_str("再来一条");
        editor.submit();
        assert_eq!(editor.history.len(), 2);
    }

    #[test]
    fn history_navigates_up_and_down() {
        let mut editor = Editor::new();
        editor.insert_str("第一条");
        editor.submit();
        editor.insert_str("第二条");
        editor.submit();

        editor.history_up();
        assert_eq!(editor.text, "第二条");
        editor.history_up();
        assert_eq!(editor.text, "第一条");
        editor.history_down();
        assert_eq!(editor.text, "第二条");
        editor.history_down();
        assert!(editor.text.is_empty());
    }

    #[test]
    fn duplicate_history_entries_are_merged() {
        let mut editor = Editor::new();
        editor.insert_str("重复");
        editor.submit();
        editor.insert_str("重复");
        editor.submit();
        assert_eq!(editor.history.len(), 1);
    }
}

#[cfg(test)]
mod p2_word_edit_tests {
    use super::*;

    #[test]
    fn delete_word_back_removes_last_word() {
        let mut e = Editor::new();
        e.set_text("修复 bug 并提交".to_string());
        e.end();
        // 中文连续段（无空白分隔）整体是一个词。
        e.delete_word_back();
        assert_eq!(e.text, "修复 bug ");
        e.delete_word_back();
        assert_eq!(e.text, "修复 ");
        e.delete_word_back();
        assert_eq!(e.text, "");
        // 空输入不再变化（不 panic）。
        e.delete_word_back();
        assert_eq!(e.text, "");
    }

    #[test]
    fn delete_to_end_keeps_prefix() {
        let mut e = Editor::new();
        e.set_text("abcdef".to_string());
        e.cursor = 2;
        e.delete_to_end();
        assert_eq!(e.text, "ab");
        assert_eq!(e.cursor, 2);
    }

    #[test]
    fn move_word_left_right_respects_boundaries() {
        let mut e = Editor::new();
        e.set_text("aaa bbb ccc".to_string());
        e.end();
        e.move_word_left();
        assert_eq!(e.cursor, 8, "停在下一个词首: {}", e.text);
        e.move_word_left();
        assert_eq!(e.cursor, 4);
        e.move_word_left();
        assert_eq!(e.cursor, 0);
        e.move_word_right();
        assert_eq!(e.cursor, 4);
        e.move_word_right();
        assert_eq!(e.cursor, 8);
    }
}
