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
    /// 撤销栈（成熟化：Ctrl+Z / Ctrl+Y）。快照是 (text, cursor)；同类
    /// 连续编辑（打字 / 连续退格）合并为同一 undo 单元，光标移动不产生单元。
    undo_stack: Vec<EditSnapshot>,
    redo_stack: Vec<EditSnapshot>,
    /// 上一编辑操作类别（连续同类编辑合并为一个 undo 单元）。
    last_op: Option<EditOp>,
}

/// 一次编辑前的完整快照（undo/redo 单元）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct EditSnapshot {
    text: String,
    cursor: usize,
}

/// 编辑操作类别（决定 undo 单元合并边界）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditOp {
    /// 字符/文本插入（连续打字合并）。
    Insert,
    /// 字符删除（连续退格/删除合并）。
    Delete,
    /// 词/行级删除（每次独立 undo 单元）。
    Discrete,
}

/// 历史条数上限（防长会话内存增长）。
const HISTORY_CAP: usize = 100;
/// undo/redo 栈深度上限（每条快照 ≤256 KiB 输入，栈有界防长会话内存增长）。
const EDIT_HISTORY_CAP: usize = 200;
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录当前状态为 undo 快照（去重：与栈顶相同则不压栈）。
    fn push_undo_snapshot(&mut self) {
        let snap = EditSnapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        };
        if self.undo_stack.last() != Some(&snap) {
            self.undo_stack.push(snap);
            if self.undo_stack.len() > EDIT_HISTORY_CAP {
                let drop = self.undo_stack.len() - EDIT_HISTORY_CAP;
                self.undo_stack.drain(..drop);
            }
        }
        // 新编辑使 redo 分支失效。
        self.redo_stack.clear();
    }

    /// 文本修改的统一入口：同类连续编辑只记一次快照（打字/退格 run），
    /// 换类、Discrete 或非编辑操作后下一次修改必记快照。
    fn edit(&mut self, op: EditOp, apply: impl FnOnce(&mut Self)) {
        if op == EditOp::Discrete || self.last_op != Some(op) {
            self.push_undo_snapshot();
        }
        apply(self);
        self.last_op = Some(op);
    }

    /// 光标移动/历史/整行替换等非编辑操作：打断 undo 合并 run。
    fn break_undo_run(&mut self) {
        self.last_op = None;
    }

    pub fn insert_char(&mut self, c: char) {
        if self.text.len().saturating_add(c.len_utf8()) > MAX_INPUT_BYTES {
            return;
        }
        self.edit(EditOp::Insert, |editor| {
            editor.text.insert(editor.cursor, c);
            editor.cursor += c.len_utf8();
        });
    }

    pub fn insert_str(&mut self, s: &str) {
        let room = MAX_INPUT_BYTES.saturating_sub(self.text.len());
        let keep = crate::text::floor_char_boundary(s, room.min(s.len()));
        self.edit(EditOp::Insert, |editor| {
            editor.text.insert_str(editor.cursor, &s[..keep]);
            editor.cursor += keep;
        });
    }

    /// 仅当光标前的文本精确等于 `suffix` 时，将其原子替换为 `replacement`。
    ///
    /// 用于把旧终端逐键注入、已经即时显示的长粘贴折叠成占位符。精确后缀校验
    /// 可确保光标移动、截断或其他编辑发生时绝不误删用户原有内容。
    pub fn replace_suffix_before_cursor(&mut self, suffix: &str, replacement: &str) -> bool {
        let Some(start) = self.cursor.checked_sub(suffix.len()) else {
            return false;
        };
        if !self.text.is_char_boundary(start)
            || self.text.get(start..self.cursor) != Some(suffix)
            || self
                .text
                .len()
                .saturating_sub(suffix.len())
                .saturating_add(replacement.len())
                > MAX_INPUT_BYTES
        {
            return false;
        }
        let end = self.cursor;
        self.edit(EditOp::Discrete, |editor| {
            editor.text.replace_range(start..end, replacement);
            editor.cursor = start + replacement.len();
            editor.preferred_column = None;
        });
        true
    }

    /// 撤销上一次编辑（Ctrl+Z）。无可撤销历史时静默无操作。
    pub fn undo(&mut self) {
        if let Some(snap) = self.undo_stack.pop() {
            self.redo_stack.push(EditSnapshot {
                text: self.text.clone(),
                cursor: self.cursor,
            });
            self.text = snap.text;
            self.cursor = snap.cursor;
            self.preferred_column = None;
            self.last_op = None;
        }
    }

    /// 重做被撤销的编辑（Ctrl+Y）。
    pub fn redo(&mut self) {
        if let Some(snap) = self.redo_stack.pop() {
            self.undo_stack.push(EditSnapshot {
                text: self.text.clone(),
                cursor: self.cursor,
            });
            self.text = snap.text;
            self.cursor = snap.cursor;
            self.preferred_column = None;
            self.last_op = None;
        }
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
            let w = crate::text::char_cell_width(ch);
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
        self.break_undo_run();
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
        self.break_undo_run();
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
        self.break_undo_run();
        let (start, _) = self.logical_line_bounds();
        self.cursor = start;
        self.preferred_column = None;
    }

    /// 光标所在 logical line 的末尾（§22：End/Ctrl+E）。
    pub fn end(&mut self) {
        self.break_undo_run();
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
            self.edit(EditOp::Delete, |editor| {
                editor.text.drain(prev..editor.cursor);
                editor.cursor = prev;
            });
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let next = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.edit(EditOp::Delete, |editor| {
                editor.text.drain(editor.cursor..next);
            });
        }
    }

    pub fn move_left(&mut self) {
        self.break_undo_run();
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
        self.break_undo_run();
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
        self.edit(EditOp::Discrete, |editor| {
            editor.text.drain(new_cursor..editor.cursor);
            editor.cursor = new_cursor;
        });
    }

    /// P2（编辑器增强）：删除光标到行尾。
    pub fn delete_to_end(&mut self) {
        // §PointerHit：多行输入时删到当前逻辑行尾（Ctrl+K 语义），
        // 不是删到整个输入末尾。
        let (_, end) = self.logical_line_bounds();
        self.edit(EditOp::Discrete, |editor| {
            editor.text.drain(editor.cursor..end);
        });
    }

    /// P2（编辑器增强）：按“词”向左移动（跳过词间空白，停在词首）。
    pub fn move_word_left(&mut self) {
        self.break_undo_run();
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
        self.break_undo_run();
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
        self.break_undo_run();
        self.text = if text.len() <= MAX_INPUT_BYTES {
            text
        } else {
            crate::text::truncate_middle_utf8(&text, MAX_INPUT_BYTES, "\n…[input truncated]…\n")
        };
        self.cursor = self.text.len();
        self.preferred_column = None;
    }

    /// 清空输入。
    pub fn clear(&mut self) {
        self.break_undo_run();
        self.text.clear();
        self.cursor = 0;
        self.preferred_column = None;
        self.draft = None;
    }

    /// 提交当前输入：清空编辑区并入历史（去重）。
    ///
    /// ISSUE-017：多行内容**不得**整体 trim——首行前导缩进是代码块/续行命令的
    /// 有效内容（此前 `trim()` 把粘贴代码块的首行缩进整段剥掉）。单行输入保持
    /// 原有 trim（首尾空白对单行命令无意义）；多行只去尾部换行与末行尾随空白。
    pub fn submit(&mut self) -> String {
        self.break_undo_run();
        let raw = self.text.clone();
        let text = if raw.contains('\n') {
            raw.trim_end_matches([' ', '\t', '\n', '\r']).to_string()
        } else {
            raw.trim().to_string()
        };
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
        self.break_undo_run();
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
        self.break_undo_run();
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

    /// ISSUE-017：多行提交不得剥掉首行前导缩进（缩进代码块/续行命令是有效内容）；
    /// 只去尾部换行与末行尾随空白。
    #[test]
    fn submit_keeps_leading_indent_in_multiline() {
        let mut editor = Editor::new();
        editor.insert_str("    foo() {\n        bar();\n    }\n");
        assert_eq!(editor.submit(), "    foo() {\n        bar();\n    }");
        // 首行缩进必须保留。
        let mut editor2 = Editor::new();
        editor2.insert_str("  git commit -m \"x\"\n  git push\n");
        assert_eq!(
            editor2.submit(),
            "  git commit -m \"x\"\n  git push",
            "首行前导空格不得被剥掉"
        );
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
fn undo_restores_previous_text_and_cursor() {
    let mut editor = Editor::new();
    editor.insert_str("你好");
    editor.insert_char('世');
    editor.insert_char('界');
    assert_eq!(editor.text, "你好世界");
    // 连续打字合并为一个 undo 单元：一次 undo 回到输入前（空）。
    editor.undo();
    assert!(editor.text.is_empty(), "连续打字合并为一个 undo 单元");
    editor.redo();
    assert_eq!(editor.text, "你好世界");
    editor.undo();
    editor.undo();
    assert!(editor.text.is_empty(), "undo 到初始空输入");
    editor.undo();
    assert!(editor.text.is_empty(), "无可撤销历史时静默无操作");
}

#[test]
fn cursor_moves_break_typing_run() {
    let mut editor = Editor::new();
    editor.insert_str("ab");
    editor.move_left();
    editor.insert_char('X');
    assert_eq!(editor.text, "aXb");
    // 光标移动后插入是新单元：undo 一次回到 "ab"，再 undo 回到 ""。
    editor.undo();
    assert_eq!(editor.text, "ab", "光标移动打断 typing run");
    editor.undo();
    assert!(editor.text.is_empty());
}

#[test]
fn backspace_run_undoes_as_one_unit() {
    let mut editor = Editor::new();
    editor.insert_str("abcd");
    editor.backspace();
    editor.backspace();
    assert_eq!(editor.text, "ab");
    editor.undo();
    assert_eq!(editor.text, "abcd", "连续退格合并为一个 undo 单元");
}

#[test]
fn new_edit_clears_redo_branch() {
    let mut editor = Editor::new();
    editor.insert_str("abc");
    editor.undo();
    assert_eq!(editor.text, "");
    editor.insert_char('z');
    editor.redo();
    assert_eq!(editor.text, "z", "新编辑使 redo 分支失效");
}

#[test]
fn word_and_line_deletes_are_discrete_undo_units() {
    let mut editor = Editor::new();
    editor.insert_str("hello world");
    editor.delete_word_back();
    assert_eq!(editor.text, "hello ");
    // 光标定位到词中（pub 字段）再删到行尾——每次离散删除独立 undo 单元。
    editor.cursor = 2;
    editor.delete_to_end();
    assert_eq!(editor.text, "he");
    editor.undo();
    assert_eq!(editor.text, "hello ", "delete_to_end 独立 undo 单元");
    editor.undo();
    assert_eq!(
        editor.text, "hello world",
        "delete_word_back 独立 undo 单元"
    );
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

/// P6-05：grapheme 边界函数（emoji ZWJ / 组合字符按用户感知字符处理）。
/// 返回 text 的 grapheme 簇边界字节偏移（0..len）。
pub fn grapheme_boundaries(text: &str) -> Vec<usize> {
    use unicode_segmentation::UnicodeSegmentation;
    let mut boundaries = vec![0usize];
    for g in text.graphemes(true) {
        let next = boundaries.last().unwrap() + g.len();
        boundaries.push(next);
    }
    boundaries
}

/// P6-05：光标前的 grapheme 数（用户感知字符数；与字节 offset 区分）。
pub fn grapheme_count_before(text: &str, byte_offset: usize) -> usize {
    let bounds = grapheme_boundaries(text);
    // 完整越过的 grapheme 数 = 边界 b（b <= offset）数 - 起始边界 0；
    // 光标在 grapheme 内部（offset 非边界）时不加一。
    bounds
        .iter()
        .filter(|&&b| b <= byte_offset)
        .count()
        .saturating_sub(1)
}

#[cfg(test)]
mod grapheme_tests {
    use super::*;

    /// emoji ZWJ 序列是一个 grapheme（多字节多 char）。
    #[test]
    fn zwj_emoji_is_one_grapheme() {
        let text = "👨‍👩‍👧‍👦"; // 家庭 emoji：4 个 char，1 个 grapheme
        let bounds = grapheme_boundaries(text);
        assert_eq!(bounds.len(), 2, "一个 grapheme = 2 个边界（0 与 len）");
        assert_eq!(bounds[1], text.len());
    }

    /// 组合字符（é = e + combining accent）是一个 grapheme。
    #[test]
    fn combining_sequence_is_one_grapheme() {
        let text = "e\u{301}"; // e + combining acute
        let bounds = grapheme_boundaries(text);
        assert_eq!(bounds.len(), 2, "组合序列 = 1 grapheme");
        assert_eq!(
            grapheme_count_before(text, 1),
            0,
            "第一个字节处仍是 0 个 grapheme"
        );
    }

    /// ASCII 与 grapheme 一一对应。
    #[test]
    fn ascii_graphemes_equal_chars() {
        let text = "hello";
        assert_eq!(grapheme_boundaries(text).len(), text.len() + 1);
        assert_eq!(grapheme_count_before(text, 3), 3);
    }
}
