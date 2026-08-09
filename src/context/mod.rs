//! 上下文管理（文档 §15）。
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
    messages.iter().map(estimate_message).sum()
}

/// 请求级 token 估算（P0-9：compaction 判断与用量条必须包含 system prompt、
/// 计划快照和工具 schema，不能只算 messages——否则实际请求超窗口而估算
/// 不触发，provider 直接报 length error）。
pub fn estimate_request(
    system_prompt: &str,
    messages: &[ChatMessage],
    tools: &[crate::provider::ToolDef],
) -> u64 {
    let mut total = estimate_tokens(system_prompt);
    total += estimate_messages(messages);
    for tool in tools {
        total += estimate_tokens(&tool.name);
        total += estimate_tokens(&tool.description);
        total += estimate_tokens(&tool.parameters.to_string());
    }
    // 每条消息的 role/tool_call_id/name 等 envelope 开销（§15.4 保守系数）。
    total += (messages.len() as u64) * 8;
    total
}

fn estimate_message(message: &ChatMessage) -> u64 {
    match message {
        ChatMessage::System(text) | ChatMessage::User(text) => estimate_tokens(text),
        ChatMessage::Assistant {
            content,
            tool_calls,
        } => {
            estimate_tokens(content)
                + tool_calls
                    .iter()
                    .map(|call| estimate_tokens(&call.arguments))
                    .sum::<u64>()
        }
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
    let digest = blake3::hash(content.as_bytes());
    let tail: Vec<&str> = content
        .lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut key_lines: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        let is_key = trimmed.starts_with("status:")
            || trimmed.starts_with("program:")
            || trimmed.starts_with("exit_code:")
            || trimmed.starts_with("artifact:")
            || trimmed.starts_with("error:");
        if is_key && !tail.contains(&line) && !key_lines.contains(&line) {
            key_lines.push(line);
        }
    }
    let mut out = format!(
        "[output pruned: {} tokens, digest {}]",
        estimate_tokens(content),
        &digest.to_hex()[..16]
    );
    if !key_lines.is_empty() {
        out.push('\n');
        out.push_str(&key_lines.join("\n"));
    }
    out.push('\n');
    out.push_str(&tail.join("\n"));
    if content.lines().count() > 8 {
        out.push_str("\n--- tail ---");
    }
    out
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
    // 保留结构化字段；压缩为可注入的 summary 文本。
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
    summary_tokens * required_ratio < original_tokens
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
}
