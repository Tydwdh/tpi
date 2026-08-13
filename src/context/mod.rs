//! 模型上下文预算、裁剪和压缩。
//!
//! - 事实源与投影分离（§15.1）：完整 session log 永远保留；发送给模型的是投影。
//! - 渐进检索（§15.2）：不预加载整个仓库；模型通过 bounded search 找入口。
//! - Tool result pruning（§15.3）：compaction 前先做确定性 pruning（只影响投影）。
//! - Compaction（§15.4）：显式 compaction 角色生成结构化 summary；失败不循环。
//! - Reasoning（§15.5）：私有 reasoning 不进入 durable facts，后续 context 不重发。

use crate::provider::ChatMessage;

/// token 保守估算（§15.4：tokenizer 不可用时）：
/// `max(ceil(utf8_bytes / 3), unicode_scalar_count)`。
pub fn estimate_tokens(text: &str) -> u64 {
    let scalars = text.chars().count() as u64;
    // §PointerHit 6：更贴近真实 tokenizer 的启发式——
    // CJK（每字符多字节，常见 ~1 token/字）与 ASCII/code（~4 chars/token）分开算。
    // 旧实现 `max(bytes/3, scalars)` 对 ASCII 给 1 char/token，过保守导致
    // compaction 过早触发。
    let cjk_count = text
        .chars()
        .filter(|c| matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3000}'..='\u{303F}'))
        .count() as u64;
    let ascii_scalars = scalars.saturating_sub(cjk_count);
    // ASCII/code：~3 chars/token（比旧 1 char/token 合理，比 4 保守些，
    // 避免过少导致 compaction 触发延迟）。
    (cjk_count + ascii_scalars.div_ceil(3)).max(1)
}

/// 估算一组消息的 token 数。
pub fn estimate_messages(messages: &[ChatMessage]) -> u64 {
    messages
        .iter()
        .map(estimate_message)
        .fold(0, u64::saturating_add)
}

/// 请求级 token 估算（P0-9：compaction 判断与用量条必须包含 system prompt、
/// 计划工具轮和工具 schema，不能只算裸对话文本——否则实际请求超窗口而估算
/// 不触发，provider 直接报 length error）。
pub fn estimate_request(
    system_prompt: &str,
    messages: &[ChatMessage],
    tools: &[crate::provider::ToolDef],
) -> u64 {
    let mut total = estimate_tokens(system_prompt);
    total = total.saturating_add(estimate_messages(messages));
    for tool in tools {
        total = total.saturating_add(estimate_tokens(&tool.name));
        total = total.saturating_add(estimate_tokens(&tool.description));
        total = total.saturating_add(estimate_tokens(&tool.parameters.to_string()));
    }
    // 每条消息的 role/tool_call_id/name 等 envelope 开销（§15.4 保守系数）。
    let message_count = u64::try_from(messages.len()).unwrap_or(u64::MAX);
    total = total.saturating_add(message_count.saturating_mul(8));
    total
}

fn estimate_message(message: &ChatMessage) -> u64 {
    match message {
        ChatMessage::System(text) | ChatMessage::User(text) => estimate_tokens(text),
        ChatMessage::Assistant {
            content,
            tool_calls,
        } => tool_calls
            .iter()
            .map(|call| estimate_tokens(&call.arguments))
            .fold(estimate_tokens(content), u64::saturating_add),
        ChatMessage::Tool { content, .. } => estimate_tokens(content),
    }
}

/// 可用输入预算（§15.4）：
/// `usable_input = context_window - max_output_tokens - safety_reserve`。
pub fn usable_input(context_window: u64, max_output_tokens: u64, safety_reserve: u64) -> u64 {
    context_window
        .saturating_sub(max_output_tokens)
        .saturating_sub(safety_reserve)
}

/// 是否触发 compaction（§15.4：`projected_input > usable_input`）。
pub fn should_compact(projected: u64, usable: u64) -> bool {
    projected > usable
}

/// 保留最近原始上下文的比例（§15.4：约 25%，且不小于两个完整 turns）。
pub const KEEP_RECENT_RATIO: f64 = 0.25;
/// 最小保留 turns（§15.4）。
pub const MIN_KEEP_TURNS: usize = 2;

/// 大 tool output 缩略（§15.3：pruning 只影响投影）。
///
/// 超过 800 token 的工具输出替换为 `digest + 尾部 8 行`（tail 保留错误相关诊断）；
/// 失败诊断、实际 diff、用户约束和当前计划保留更高权重。
/// P1-5：结构化关键行（status/program/exit_code/artifact/error）不在 tail 时
/// 显式保留——否则模型会失去 artifact 引用等恢复入口。
pub fn prune_messages(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    const MAX_TOOL_OUTPUT_TOKENS: u64 = 800;
    messages
        .into_iter()
        .map(|message| match message {
            ChatMessage::Tool {
                tool_call_id,
                name,
                content,
            } => {
                if estimate_tokens(&content) > MAX_TOOL_OUTPUT_TOKENS {
                    ChatMessage::Tool {
                        tool_call_id,
                        name,
                        content: prune_tool_output(&content),
                    }
                } else {
                    ChatMessage::Tool {
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

/// P1-5：单条工具输出的确定性缩略——digest + 结构化关键行 + 尾部 8 行。
/// 关键行（artifact/error/status/exit_code/program）即使不在尾部也保留，
/// 避免模型失去完整输出入口（如 `artifact: @artifact/...`）。
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

/// Compaction summary schema（§15.4 固定字段）。
pub const SUMMARY_SCHEMA: &str = "\
请把下面的会话事实压缩为结构化摘要，严格按以下字段：

Goal
Constraints
Decisions
Completed
In progress
Next exact action
Relevant files and revisions
Verification status
Failed attempts and why

要求：只保留用户内容、已提交的 assistant 内容、工具证据和结构化状态；
不要从 reasoning 提炼事实；不要编造不存在的细节。输出纯文本，字段用
'Field: value' 格式。";

/// 构造 compaction 请求的消息（§15.4：无工具 schema、独立较小 output budget）。
pub fn compaction_request_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(messages.len() + 1);
    out.push(ChatMessage::System(SUMMARY_SCHEMA.to_string()));
    out.extend_from_slice(messages);
    out
}

/// 解析 compaction summary（§15.4：解析失败属于 compaction failure，不修正重试）。
pub fn parse_summary(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    const FIELDS: [&str; 9] = [
        "Goal",
        "Constraints",
        "Decisions",
        "Completed",
        "In progress",
        "Next exact action",
        "Relevant files and revisions",
        "Verification status",
        "Failed attempts and why",
    ];
    let mut seen = [false; FIELDS.len()];
    for line in trimmed.lines().filter(|line| !line.trim().is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return String::new();
        };
        let Some(index) = FIELDS.iter().position(|field| *field == name.trim()) else {
            return String::new();
        };
        if seen[index] || value.trim().is_empty() {
            return String::new();
        }
        seen[index] = true;
    }
    if seen.iter().any(|present| !present) {
        return String::new();
    }
    trimmed.to_string()
}

/// Compaction summary 是否"足够缩小"值得提交（§15.4 第 5 条）。
///
/// §PointerHit 7：阈值随原文规模自适应，而非固定 4 倍。
/// - 上下文越大，压缩空间越大，允许较小比例（2 倍）就够；
/// - 小上下文要求更显著缩小（避免无意义压缩）；
/// - 否则固定 4 倍会导致"只需压缩 20% 即可继续"的会话被 ContextOverflow。
pub fn is_significant_shrink(original_tokens: u64, summary_tokens: u64) -> bool {
    if summary_tokens == 0 || summary_tokens >= original_tokens {
        return false;
    }
    // 原文 ≥ 64k token：2 倍即可（大上下文压缩空间充裕）；
    // 原文 16k~64k：3 倍；更小：4 倍（保守）。
    let required_ratio = if original_tokens >= 64_000 {
        2u64
    } else if original_tokens >= 16_000 {
        3u64
    } else {
        4u64
    };
    summary_tokens <= original_tokens.saturating_sub(1) / required_ratio
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolDef;

    /// P0-9：请求级估算必须包含 system prompt 与工具 schema。
    /// 此前只估算 messages，实际请求明显超窗口时 compaction 不触发。
    #[test]
    fn estimate_request_includes_system_and_tool_schemas() {
        let system = "s".repeat(3000);
        let messages = vec![ChatMessage::User("hello".into())];
        let tool = ToolDef {
            name: "bash".into(),
            description: "run a shell command".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"cmd": {"type": "string"}}
            }),
        };
        let estimate = estimate_request(&system, &messages, std::slice::from_ref(&tool));
        // system prompt（3000 ASCII 字符 ≈ 1000 token，§PointerHit 6 启发式）必须计入。
        assert!(
            estimate > estimate_messages(&messages),
            "system prompt 必须计入: {estimate}"
        );
        assert!(estimate >= 1000, "system prompt 估算过低: {estimate}");
        // 工具 schema 数量影响估算。
        let many_tools = vec![tool; 20];
        let with_many = estimate_request(&system, &messages, &many_tools);
        assert!(
            with_many > estimate,
            "tool schema 必须计入: {with_many} vs {estimate}"
        );
    }

    /// P1-5：prune 后结构化关键行（artifact 引用/error/status）必须保留，
    /// 即使它们不在尾部 8 行内——否则模型失去完整输出入口。
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

    #[test]
    fn significant_shrink_handles_extreme_token_counts_without_overflow() {
        assert!(!is_significant_shrink(u64::MAX, u64::MAX - 1));
        assert!(is_significant_shrink(u64::MAX, u64::MAX / 3));
    }

    #[test]
    fn summary_parser_requires_each_schema_field_once_with_a_value() {
        let valid = "Goal: g\nConstraints: c\nDecisions: d\nCompleted: c\nIn progress: i\nNext exact action: n\nRelevant files and revisions: r\nVerification status: v\nFailed attempts and why: f";
        assert_eq!(parse_summary(valid), valid);
        assert!(parse_summary("Goal: only one field").is_empty());
        assert!(parse_summary(&format!("{valid}\nGoal: duplicate")).is_empty());
        assert!(parse_summary(&valid.replace("Constraints: c", "Constraints:")).is_empty());
        assert!(parse_summary(&format!("{valid}\nUnknown: x")).is_empty());
    }
}
