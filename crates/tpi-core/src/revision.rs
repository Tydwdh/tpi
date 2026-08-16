//! 内容寻址 revision 与 token 估算（P7-02 拆 crate：从 tool::edit / context
//! 下沉的纯函数；session 与主 crate 共用）。

/// revision 前缀（与 `read`/`edit`/`write` 共享，§2.2）。
pub const REVISION_PREFIX: &str = "b3:";

/// 计算文件原始字节的 revision（§10.1）。
pub fn revision_of(raw: &[u8]) -> String {
    let digest = blake3::hash(raw);
    format!("{REVISION_PREFIX}{}", digest.to_hex())
}

/// 估算文本 token 数（CJK 与 ASCII/code 分开的启发式）。
pub fn estimate_tokens(text: &str) -> u64 {
    let scalars = text.chars().count() as u64;
    let cjk_count = text
        .chars()
        .filter(|c| matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3000}'..='\u{303F}'))
        .count() as u64;
    let ascii_scalars = scalars.saturating_sub(cjk_count);
    (cjk_count + ascii_scalars.div_ceil(3)).max(1)
}

/// 可用输入预算（P7-02 拆 crate：从 context 下沉；config 与 agent 共用）。
pub fn usable_input(context_window: u64, max_output_tokens: u64, safety_reserve: u64) -> u64 {
    context_window
        .saturating_sub(max_output_tokens)
        .saturating_sub(safety_reserve)
}

/// 剪枝超长工具输出（projection 用；不改变消息数量是不变量）。
/// 完整版：digest + 结构化关键行（status/program/exit_code/artifact/error）
/// + 尾部 8 行，模型仍能获得完整输出入口。
pub fn prune_messages(
    messages: Vec<crate::message::ChatMessage>,
) -> Vec<crate::message::ChatMessage> {
    const MAX_TOOL_OUTPUT_TOKENS: u64 = 800;
    messages
        .into_iter()
        .map(|message| match message {
            crate::message::ChatMessage::Tool {
                tool_call_id,
                name,
                content,
            } => {
                if estimate_tokens(&content) > MAX_TOOL_OUTPUT_TOKENS {
                    crate::message::ChatMessage::Tool {
                        tool_call_id,
                        name,
                        content: prune_tool_output(&content),
                    }
                } else {
                    crate::message::ChatMessage::Tool {
                        tool_call_id,
                        name,
                        content,
                    }
                }
            }
            other => other,
        })
        .collect()
}

/// 单条工具输出的确定性缩略——digest + 结构化关键行 + 尾部 8 行。
/// 关键行（artifact/error/status/exit_code/program）即使不在尾部也保留。
fn prune_tool_output(content: &str) -> String {
    const MAX_PRUNED_TOKENS: u64 = 800;
    const MAX_KEY_TOKENS: u64 = 400;
    const MAX_LINES_PER_KEY: usize = 2;
    const MAX_LINE_CHARS: usize = 256;

    let digest = blake3::hash(content.as_bytes());
    let tail: Vec<String> = content
        .lines()
        .rev()
        .take(8)
        .map(|line| truncate_line(line, MAX_LINE_CHARS))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut key_lines: Vec<String> = Vec::new();
    let mut key_counts = [0usize; 5];
    let mut omitted_key_lines = 0usize;
    let mut line_count = 0usize;
    for line in content.lines() {
        line_count = line_count.saturating_add(1);
        let trimmed = line.trim_start();
        let key_kind = if trimmed.starts_with("status:") {
            Some(0)
        } else if trimmed.starts_with("program:") {
            Some(1)
        } else if trimmed.starts_with("exit_code:") {
            Some(2)
        } else if trimmed.starts_with("artifact:") {
            Some(3)
        } else if trimmed.starts_with("error:") {
            Some(4)
        } else {
            None
        };
        let Some(key_kind) = key_kind else { continue };
        let bounded = truncate_line(line, MAX_LINE_CHARS);
        if tail.contains(&bounded) || key_lines.contains(&bounded) {
            continue;
        }
        if key_counts[key_kind] >= MAX_LINES_PER_KEY {
            omitted_key_lines = omitted_key_lines.saturating_add(1);
            continue;
        }
        key_counts[key_kind] += 1;
        key_lines.push(bounded);
    }
    let mut out = format!(
        "[output pruned: {} tokens, digest {}]",
        estimate_tokens(content),
        &digest.to_hex()[..16]
    );
    if !key_lines.is_empty() {
        for line in key_lines {
            if !push_line_within_budget(&mut out, &line, MAX_KEY_TOKENS) {
                omitted_key_lines = omitted_key_lines.saturating_add(1);
            }
        }
    }
    if omitted_key_lines > 0 {
        let marker = format!("[{} additional key lines omitted]", omitted_key_lines);
        let _ = push_line_within_budget(&mut out, &marker, MAX_KEY_TOKENS);
    }
    if line_count > 8 {
        let _ = push_line_within_budget(&mut out, "--- tail ---", MAX_PRUNED_TOKENS);
    }
    for line in tail {
        if !push_line_within_budget(&mut out, &line, MAX_PRUNED_TOKENS) {
            let _ = push_line_within_budget(
                &mut out,
                "[remaining tail lines omitted]",
                MAX_PRUNED_TOKENS,
            );
            break;
        }
    }
    debug_assert!(estimate_tokens(&out) <= MAX_PRUNED_TOKENS);
    out
}

fn truncate_line(line: &str, max_chars: usize) -> String {
    let mut chars = line.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}… [line truncated]")
    } else {
        prefix
    }
}

fn push_line_within_budget(out: &mut String, line: &str, max_tokens: u64) -> bool {
    let original_len = out.len();
    out.push('\n');
    out.push_str(line);
    if estimate_tokens(out) <= max_tokens {
        true
    } else {
        out.truncate(original_len);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ChatMessage;

    #[test]
    fn revision_prefix_and_hex() {
        let rev = revision_of(b"hello");
        assert!(rev.starts_with("b3:"));
        assert_eq!(rev.len(), "b3:".len() + 64);
    }

    #[test]
    fn prune_truncates_long_tool_output() {
        let long = "x".repeat(10_000);
        let pruned = prune_messages(vec![ChatMessage::Tool {
            tool_call_id: "c1".into(),
            name: "bash".into(),
            content: long.clone(),
        }]);
        match &pruned[0] {
            ChatMessage::Tool { content, .. } => {
                assert!(content.contains("output pruned"), "超长输出截断: {content}");
                assert!(content.len() < long.len());
            }
            _ => panic!("必须保留 Tool 消息"),
        }
    }

    #[test]
    fn short_tool_output_kept() {
        let pruned = prune_messages(vec![ChatMessage::Tool {
            tool_call_id: "c1".into(),
            name: "read".into(),
            content: "short".into(),
        }]);
        assert_eq!(
            pruned[0],
            ChatMessage::Tool {
                tool_call_id: "c1".into(),
                name: "read".into(),
                content: "short".into(),
            }
        );
    }

    #[test]
    fn estimate_tokens_cjk_vs_ascii() {
        // ASCII ~3 chars/token；CJK ~1 token/char。
        assert!(estimate_tokens("hello world") >= 1);
        let ascii = estimate_tokens("abcdefghijkl"); // 12 chars -> 4
        let cjk = estimate_tokens("你好世界"); // 4 chars -> 4
        assert!(ascii <= 4);
        assert_eq!(cjk, 4);
    }

    #[test]
    fn prune_keeps_structured_key_lines() {
        // 构造 >800 token 的输出：artifact 引用在中间（不在尾部）。
        let mut content = String::from(
            "status: succeeded\ntool: bash\nprogram: bash\nexit_code: 0\nartifact: @artifact/abc123\n\n",
        );
        content.push_str(&"line of filler content\n".repeat(200));
        content.push_str("final tail line\n");
        let pruned = prune_tool_output(&content);
        assert!(
            pruned.contains("artifact: @artifact/abc123"),
            "artifact 引用必须保留: {pruned}"
        );
        assert!(pruned.contains("status: succeeded"), "status 必须保留");
        assert!(pruned.contains("exit_code: 0"), "exit_code 必须保留");
        assert!(pruned.contains("final tail line"), "尾部诊断保留");
        assert!(
            pruned.matches("line of filler content").count() <= 7,
            "非关键内容只允许出现在 tail 8 行内（实际 {} 行）",
            pruned.matches("line of filler content").count()
        );
    }

    #[test]
    fn prune_result_stays_bounded_with_key_line_flood_and_huge_tail_line() {
        let mut content = String::new();
        for index in 0..10_000 {
            content.push_str(&format!("error: repeated diagnostic {index}\n"));
        }
        content.push_str("artifact: @artifact/session/id\n");
        content.push_str(&"界".repeat(20_000));

        let pruned = prune_tool_output(&content);
        assert!(estimate_tokens(&pruned) <= 800, "裁剪结果仍超限");
        assert!(pruned.contains("artifact: @artifact/session/id"));
        assert!(pruned.contains("additional key lines omitted"));
        assert!(pruned.contains("line truncated"));
    }
}
