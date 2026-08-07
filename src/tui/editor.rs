//! 极简行编辑器（§16.2、§21 M5）。
//!
//! 中文 IME 由终端负责；编辑器只维护文本与光标（Unicode 字符边界）。
//! 控制键：←/→ 移动、Home/End、退格/删除、Enter 提交（由 app 处理）。

/// 编辑器输入状态。
#[derive(Debug, Clone, Default)]
pub struct Editor {
    pub text: String,
    pub cursor: usize, // 字节偏移，始终在字符边界
}

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

    pub fn end(&mut self) {
        self.cursor = self.text.len();
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
}
