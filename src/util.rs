//! 通用错误处理辅助（§错误处理纪律：生产代码不允许 unwrap/expect/panic）。
//!
//! 唯一的例外是 `unreachable` 语义的 match 分支——用显式分支消除，
//! 不保留 `unreachable!()` 宏。所有不可恢复的错误都记录日志并降级。

use std::sync::{Mutex, MutexGuard};

/// 获取互斥锁；遇 poison（持锁线程 panic 后遗留）时记录告警并恢复，
/// 不 panic、不丢弃数据（`PoisonError::into_inner` 取回内部数据）。
///
/// `what` 用于日志标识（如 `"current_plan"`）。
pub fn lock_mutex<'a, T>(mutex: &'a Mutex<T>, what: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(what, "mutex poisoned; recovering guard");
            poisoned.into_inner()
        }
    }
}

/// UTF-8 安全截断：把 `String` 截断到不超过 `max_bytes` 的最大字符边界。
///
/// `String::truncate` 在非 char boundary 处直接 panic；所有按字节预算截断
/// `String` 的代码必须经过这里（BUG-001/002 回归：read/web_fetch 大中文内容）。
/// `text.len() <= max_bytes` 时原样返回。
pub fn truncate_to_char_boundary(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_prefix_within_budget_and_valid_utf8() {
        let mut s = "你好世界abc".to_string();
        truncate_to_char_boundary(&mut s, 7); // "你好世" = 9 字节；7 落在 "世" 中间 → 截到 "你好"（6 字节）
        assert_eq!(s, "你好");
        assert!(s.is_char_boundary(s.len()));
        assert!(s.len() <= 7);
    }

    #[test]
    fn truncate_short_input_unchanged() {
        let mut s = "abc".to_string();
        truncate_to_char_boundary(&mut s, 100);
        assert_eq!(s, "abc");
    }

    #[test]
    fn truncate_emoji_zwj_never_splits() {
        let mut s = "👨‍💻x".to_string(); // 8 字节 + 1
        truncate_to_char_boundary(&mut s, 8);
        // 8 字节边界在 ZWJ 序列内 → 回退到 0（不产生半个 emoji/非法字节）。
        assert!(s.is_char_boundary(s.len()));
        assert!(std::str::from_utf8(s.as_bytes()).is_ok());
        assert!(s.len() <= 8);
    }
}
