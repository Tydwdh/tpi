//! 行编辑器（§16.2、§21 M5；TUI v2 §22 起支持 logical line 导航）。
//!
//! 中文 IME 由终端负责；编辑器只维护文本与光标（Unicode 字符边界）。
//! 控制键：←/→ 移动、Home/End（logical line）、退格/删除、Enter 提交
//! （由 app 处理）、Shift+Enter/Alt+Enter 换行（多行输入）、
//! ↑/↓ 多行移动（到边界后由调用方进入历史浏览）。
//!
//! 历史由编辑器自身维护：submit 时入栈，↑/↓ 替换当前文本。

/// 编辑器输入状态。
#[derive(Debug, Clone, Default)]
pub struct Editor {
    pub text: String,
    pub cursor: usize, // 字节偏移，始终在字符边界
    /// 多行 ↑/↓ 移动时保持的目标列（display width；§21 Composer v2）。
    preferred_column: Option<usize>,
    history: Vec<String>,
    history_pos: Option<usize>,
    /// Draft saved when entering history browsing; restored when returning to newest.
    draft: Option<String>,
}

/// 历史条数上限（防长会话内存增长）。
const HISTORY_CAP: usize = 100;
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_char(&mut self, c: char) {
        if self.text.len().saturating_add(c.len_utf8()) > MAX_INPUT_BYTES {
            return;
        }
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        let room = MAX_INPUT_BYTES.saturating_sub(self.text.len());
        let keep = crate::tui::text::floor_char_boundary(s, room.min(s.len()));
        self.text.insert_str(self.cursor, &s[..keep]);
        self.cursor += keep;
    }

    /// 光标所在 logical line 的字节范围（[start, end)，不含换行符；§22）。
    pub fn logical_line_bounds(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let after = &self.text[self.cursor..];
        let end = self.cursor + after.find('\n').unwrap_or(after.len());
        (start, end)
    }

    /// 当前光标所在行的 display 列（宽度单位；§22）。
    fn cursor_col(&self) -> usize {
        let (start, _) = self.logical_line_bounds();
        unicode_width::UnicodeWidthStr::width(&self.text[start..self.cursor])
    }

    fn col_to_offset(&self, start: usize, end: usize, col: usize) -> usize {
        let mut width = 0usize;
        let mut offset = end;
        for (i, ch) in self.text[start..end].char_indices() {
            let w = crate::tui::text::char_cell_width(ch);
            if width + w > col {
                offset = start + i;
                break;
            }
            width += w;
            offset = start + i + ch.len_utf8();
        }
        offset
    }

    /// 上移一个 logical line（§22、§12：多行 cursor 优先）。
    /// 返回 false 表示已在第一行——调用方此时进入 prompt history。
    pub fn move_up(&mut self) -> bool {
        let (start, _) = self.logical_line_bounds();
        if start == 0 {
            self.preferred_column = None;
            return false;
        }
        let prev_end = start - 1; // 跳过行尾 \n
        let prev_start = self.text[..prev_end]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let column = self.preferred_column.unwrap_or_else(|| self.cursor_col());
        self.cursor = self.col_to_offset(prev_start, prev_end, column);
        self.preferred_column = Some(column);
        true
    }

    /// 下移一个 logical line（§22）。返回 false 表示已在最后一行。
    pub fn move_down(&mut self) -> bool {
        let (_, end) = self.logical_line_bounds();
        if end == self.text.len() {
            self.preferred_column = None;
            return false;
        }
        let next_start = end + 1; // 跳过行尾 \n
        let next_end = self.text[next_start..]
            .find('\n')
            .map(|i| next_start + i)
            .unwrap_or(self.text.len());
        let column = self.preferred_column.unwrap_or_else(|| self.cursor_col());
        self.cursor = self.col_to_offset(next_start, next_end, column);
        self.preferred_column = Some(column);
        true
    }

    /// 光标所在 logical line 的起始（§22：Home/Ctrl+A 不再是全文开头）。
    pub fn home(&mut self) {
        let (start, _) = self.logical_line_bounds();
        self.cursor = start;
        self.preferred_column = None;
    }

    /// 光标所在 logical line 的末尾（§22：End/Ctrl+E）。
    pub fn end(&mut self) {
        let (_, end) = self.logical_line_bounds();
        self.cursor = end;
        self.preferred_column = None;
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
        // §PointerHit：多行输入时删到当前逻辑行尾（Ctrl+K 语义），
        // 不是删到整个输入末尾。
        let (_, end) = self.logical_line_bounds();
        self.text.drain(self.cursor..end);
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

    /// 整行替换（历史浏览、命令补全用）；光标移到末尾。
    pub fn set_text(&mut self, text: String) {
        self.text = if text.len() <= MAX_INPUT_BYTES {
            text
        } else {
            crate::tui::text::truncate_middle_utf8(
                &text,
                MAX_INPUT_BYTES,
                "\n…[input truncated]…\n",
            )
        };
        self.cursor = self.text.len();
        self.preferred_column = None;
    }

    /// 清空输入。
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.preferred_column = None;
        self.draft = None;
    }

    /// 提交当前输入：trim 后返回，清空编辑区并入历史（去重）。
    pub fn submit(&mut self) -> String {
        let text = self.text.trim().to_string();
        self.clear();
        self.history_pos = None;
        self.draft = None;
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
        // First entry into history browsing: save the current draft so it can be
        // restored when the user navigates back to the newest slot.
        if self.history_pos.is_none() {
            self.draft = Some(self.text.clone());
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

    /// 历史向下浏览（更新的一条；到底后恢复进入历史前的草稿）。
    pub fn history_down(&mut self) {
        let next = match self.history_pos {
            None => return, // Not browsing history: Down must not clear the input.
            Some(i) if i + 1 < self.history.len() => Some(i + 1),
            Some(_) => None, // At newest: restore the pre-browsing draft.
        };
        match next {
            Some(i) => {
                self.history_pos = Some(i);
                self.set_text(self.history[i].clone());
            }
            None => {
                self.history_pos = None;
                let draft = self.draft.take().unwrap_or_default();
                self.set_text(draft);
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
        // §22：Home 是当前 logical line 首（光标在第二行 → 第二行首）。
        editor.home();
        assert_eq!(&editor.text[..editor.cursor], "第一行\n");
        editor.move_right();
        editor.insert_char('X');
        assert_eq!(editor.text, "第一行\n第X二行");
        editor.backspace();
        assert_eq!(editor.text, "第一行\n第二行");
        // §22：End 是当前 logical line 尾。
        editor.set_text("第一行\n第二行".into());
        editor.end();
        assert_eq!(editor.cursor, "第一行\n第二行".len(), "End = 当前行尾");
        editor.home();
        assert_eq!(&editor.text[..editor.cursor], "第一行\n", "Home = 当前行首");
        // 单行输入时 Home/End 与全文语义一致。
        editor.set_text("abc".into());
        editor.end();
        assert_eq!(editor.cursor, 3);
        editor.home();
        assert_eq!(editor.cursor, 0, "单行 Home = 全文开头");
    }

    #[test]
    fn move_up_down_navigates_logical_lines_with_preferred_column() {
        let mut editor = Editor::new();
        editor.insert_str("abc");
        editor.insert_char('\n');
        editor.insert_str("abcdef");
        // 光标在第二行 col 3 → 上移保持 col 3。
        editor.home();
        editor.move_right();
        editor.move_right();
        editor.move_right();
        assert_eq!(&editor.text[..editor.cursor], "abc\nabc");
        assert!(editor.move_up());
        assert_eq!(&editor.text[..editor.cursor], "abc", "上移保持列");
        // 到第一行后再上移 → false（调用方进入历史）。
        assert!(!editor.move_up());
        // 下移回到第二行同列。
        assert!(editor.move_down());
        assert_eq!(&editor.text[..editor.cursor], "abc\nabc");
        assert!(!editor.move_down(), "最后一行再下移 → false");
        // 短行 + 大列：clamp 到行尾。
        let mut editor = Editor::new();
        editor.insert_str("a");
        editor.insert_char('\n');
        editor.insert_str("xxxxx");
        editor.end();
        assert!(editor.move_up());
        assert_eq!(&editor.text[..editor.cursor], "a", "短行 clamp 到行尾");
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

    /// §PointerHit：多行输入 Ctrl+K 只删到当前行尾，保留后续行。
    #[test]
    fn delete_to_end_stops_at_line_end_in_multiline() {
        let mut e = Editor::new();
        e.set_text("line1 rest\nline2\nline3".to_string());
        // 光标在第一行 "rest" 中间（逻辑行尾 = 第一个 \n 前 = 索引 9）。
        e.cursor = 6;
        e.delete_to_end();
        assert_eq!(e.text, "line1 \nline2\nline3", "只删当前行后半段");
        assert_eq!(e.cursor, 6);
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

#[test]
fn history_browsing_preserves_draft() {
    let mut editor = Editor::new();
    editor.insert_str("cmd1");
    editor.submit();
    editor.insert_str("my draft");
    // Entering history browsing saves the draft.
    editor.history_up();
    assert_eq!(editor.text, "cmd1");
    // Returning to the newest slot restores the draft.
    editor.history_down();
    assert_eq!(editor.text, "my draft");
}

#[test]
fn history_down_without_browsing_keeps_input() {
    let mut editor = Editor::new();
    editor.insert_str("abc");
    editor.history_down();
    assert_eq!(
        editor.text, "abc",
        "Down without browsing must not clear the input"
    );
}

#[cfg(test)]
mod zero_width_tests {
    use super::Editor;

    /// 零宽字符（组合符/ZWJ）不推进列宽（此前 per-char max(1) 导致光标漂移）。
    #[test]
    fn zero_width_chars_do_not_advance_column() {
        let mut editor = Editor::new();
        editor.set_text("e\u{301}x".to_string()); // e + combining acute + x
        editor.end();
        assert_eq!(editor.cursor_col(), 2, "组合符不占列宽");
        // col 1 应定位到 x 起始（组合符不占列）。
        assert_eq!(editor.col_to_offset(0, editor.text.len(), 1), 3);
    }
}
