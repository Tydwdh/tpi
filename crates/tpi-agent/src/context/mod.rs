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
/// P7-02 拆 crate：estimate_tokens 下沉 tpi-core；此处 re-export 保持
/// `crate::context::estimate_tokens` 路径兼容。
pub use tpi_core::revision::estimate_tokens;

/// P7-02 拆 crate：prune_messages 下沉 tpi-core（完整版）；此处 re-export
/// 保持 `crate::context::prune_messages` 路径兼容。
pub use tpi_core::revision::prune_messages;

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

/// 可用输入预算（§15.4）：`usable_input = context_window - max_output_tokens -
/// safety_reserve`。P7-02 拆 crate：实现下沉 tpi-core（revision::usable_input），
/// 此处 re-export 保持 `crate::context::usable_input` 路径兼容。
pub use tpi_core::revision::usable_input;

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
pub const SUMMARY_SCHEMA: &str = "\
请把下面的会话事实压缩为结构化摘要。

必须严格按以下格式逐行输出（字段名用英文，值用中文或原文语言）：

Goal: <本次任务目标>
Constraints: <约束条件>
Decisions: <已做的关键决策>
Completed: <已完成的事项>
In progress: <正在进行的事项>
Next exact action: <下一步要做的第一件事>
Relevant files and revisions: <涉及文件及关键 revision>
Verification status: <验证/测试状态>
Failed attempts and why: <失败的尝试及原因>

示例：
Goal: 修复侧边栏布局
Constraints: 不引入新依赖
Decisions: 改用 cell 宽度截断
Completed: 加宽侧边栏
In progress: 调整浮层布局
Next exact action: 运行 cargo test 验证
Relevant files and revisions: src/tui/mod.rs
Verification status: 测试通过
Failed attempts and why: 无

要求：只保留用户内容、已提交的 assistant 内容、工具证据和结构化状态；
不要从 reasoning 提炼事实；不要编造不存在的细节。输出纯文本，不要加
任何前后缀说明。如果某个字段确实没有内容，写“无”，不要省略字段。";

/// 构造 compaction 请求的消息（§15.4：无工具 schema、独立较小 output budget）。
pub fn compaction_request_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(messages.len() + 1);
    out.push(ChatMessage::System(SUMMARY_SCHEMA.to_string()));
    out.extend_from_slice(messages);
    out
}

/// 解析 compaction summary（§用户诉求：尽量容错，失败有兜底）。
///
/// 字段名支持别名（大小写不敏感 + 中文 + 相近措辞），行首允许列表符/粗体
/// 标记；核心字段齐全则输出结构化字段行。
///
/// 若模型完全没有按格式输出（核心字段缺失），**退化为使用全文**——模型
/// 的压缩输出即使非结构化也保留了大量上下文，总比压缩失败、上下文继续
/// 膨胀好；最终由 `is_significant_shrink` 把关（退化全文不够短会被拦下）。
///
/// 返回空串仅当：输入为空，或输入太短且不含任何字段线索（模型“好的我
/// 来压缩”这类没干活的话）。
fn normalize_field_name(name: &str) -> Option<&'static str> {
    let name = name.trim().to_ascii_lowercase();
    // (别名, 规范名)；别名按长度降序匹配（“next exact action”先于“next action”）。
    const ALIASES: &[(&str, &str)] = &[
        ("next exact action", "Next exact action"),
        ("next exact step", "Next exact action"),
        ("next action", "Next exact action"),
        ("next step", "Next exact action"),
        ("next exact", "Next exact action"),
        ("next", "Next exact action"),
        (
            "relevant files and revisions",
            "Relevant files and revisions",
        ),
        ("relevant files", "Relevant files and revisions"),
        ("files and revisions", "Relevant files and revisions"),
        ("failed attempts and why", "Failed attempts and why"),
        ("failed attempts", "Failed attempts and why"),
        ("verification status", "Verification status"),
        ("verification", "Verification status"),
        ("in progress", "In progress"),
        ("in-progress", "In progress"),
        ("inprogress", "In progress"),
        ("progress", "In progress"),
        ("constraints", "Constraints"),
        ("constraint", "Constraints"),
        ("decisions", "Decisions"),
        ("decision", "Decisions"),
        ("completed", "Completed"),
        ("done", "Completed"),
        ("goal", "Goal"),
        ("target", "Goal"),
        // 中文模型常用的中文字段名。
        ("下一步", "Next exact action"),
        ("进行中", "In progress"),
        ("已完成", "Completed"),
        ("约束", "Constraints"),
        ("决策", "Decisions"),
        ("目标", "Goal"),
    ];
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, canonical)| *canonical)
}

/// 剥掉行首的列表/粗体标记（`- `、`* `、`1. `、`**` 等），返回内容与是否剥过。
fn strip_line_prefix(line: &str) -> (&str, bool) {
    let line = line.trim_start();
    let stripped = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("• "))
        .or_else(|| {
            // 数字列表：`1. ` / `1) `
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            (i < bytes.len() && bytes[i] == b'.').then(|| &line[i + 1..])
        })
        .or_else(|| line.strip_prefix("**"))
        .unwrap_or(line);
    (stripped, stripped != line)
}

pub fn parse_summary(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    const CORE_FIELDS: [&str; 3] = ["Goal", "In progress", "Next exact action"];

    let mut out_lines: Vec<String> = Vec::new();
    for raw_line in trimmed.lines() {
        let raw = raw_line.trim();
        if raw.is_empty() {
            continue;
        }
        let (line, _) = strip_line_prefix(raw);
        let Some((name, value)) = line.split_once(':') else {
            continue; // 非字段行（引导语等）：忽略
        };
        let Some(canonical) = normalize_field_name(name) else {
            continue; // 未知字段行：忽略
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if out_lines
            .iter()
            .any(|l| l.starts_with(&format!("{canonical}:")))
        {
            continue; // 重复字段：取第一个
        }
        out_lines.push(format!("{canonical}: {value}"));
    }
    let present: Vec<&str> = out_lines
        .iter()
        .filter_map(|l| l.split_once(':').map(|(n, _)| n.trim()))
        .collect();
    if CORE_FIELDS.iter().all(|core| present.contains(core)) {
        return out_lines.join("\n");
    }

    // 核心字段缺失：退化为全文。模型压缩输出即使非结构化也保留了大量
    // 上下文；极短的“好的，我来压缩…”（<60 字节）判无效，最终由
    // is_significant_shrink 把关（退化全文不够短会被拦下）。
    if trimmed.len() >= 60 {
        trimmed.to_string()
    } else {
        String::new()
    }
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

// ---------------------------------------------------------------------------
// P5-04：context policy pipeline + request cache key。
//
// 顺序显式：domain messages → policies（plan/compaction/token 测量按序应用）→
// provider request。cache key 纳入 tool definition revision、plan、compaction
// 与 token measurement（任何变化 → 不同 key → 不复用缓存）。

use std::fmt::Write as _;

/// 请求级 cache key（纳入 tool revision / plan / compaction / token 测量）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestCacheKey {
    pub tool_revision: u64,
    pub plan_revision: u64,
    pub compaction_seq: u64,
    pub token_measurement: u64,
}

impl RequestCacheKey {
    /// 稳定字符串（调试/审计）。
    pub fn to_key_string(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            "tools={};plan={};compact={};tokens={}",
            self.tool_revision, self.plan_revision, self.compaction_seq, self.token_measurement
        );
        out
    }
}

/// 显式 context policy（顺序执行；每项有明确输入/输出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextPolicy {
    /// 注入 plan snapshot（模型看到剩余计划）。
    InjectPlan,
    /// 应用 compaction 决策（should_compact → prune）。
    ApplyCompaction,
    /// token 测量（estimate_request；纳入 cache key）。
    MeasureTokens,
}

/// 默认策略顺序（roadmap：顺序显式）。
pub const DEFAULT_POLICY_ORDER: &[ContextPolicy] = &[
    ContextPolicy::InjectPlan,
    ContextPolicy::ApplyCompaction,
    ContextPolicy::MeasureTokens,
];

#[cfg(test)]
mod tests {
    use super::*;
    /// cache key：tool revision / plan / compaction / token 任一变化 → key 变。
    #[test]
    fn cache_key_changes_with_any_dimension() {
        let base = RequestCacheKey {
            tool_revision: 1,
            plan_revision: 1,
            compaction_seq: 0,
            token_measurement: 100,
        };
        let tool_bump = RequestCacheKey {
            tool_revision: 2,
            ..base.clone()
        };
        let plan_bump = RequestCacheKey {
            plan_revision: 2,
            ..base.clone()
        };
        let compact_bump = RequestCacheKey {
            compaction_seq: 1,
            ..base.clone()
        };
        let token_bump = RequestCacheKey {
            token_measurement: 200,
            ..base.clone()
        };
        assert_ne!(base, tool_bump);
        assert_ne!(base, plan_bump);
        assert_ne!(base, compact_bump);
        assert_ne!(base, token_bump);
        assert_eq!(base.to_key_string(), "tools=1;plan=1;compact=0;tokens=100");
    }

    /// 策略顺序显式且确定。
    #[test]
    fn policy_order_is_explicit() {
        assert_eq!(
            DEFAULT_POLICY_ORDER,
            &[
                ContextPolicy::InjectPlan,
                ContextPolicy::ApplyCompaction,
                ContextPolicy::MeasureTokens,
            ]
        );
    }

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
    fn significant_shrink_handles_extreme_token_counts_without_overflow() {
        assert!(!is_significant_shrink(u64::MAX, u64::MAX - 1));
        assert!(is_significant_shrink(u64::MAX, u64::MAX / 3));
    }

    /// 全字段 + 值合法：原样保留。
    #[test]
    fn summary_parser_full_valid_roundtrip() {
        let valid = "Goal: g\nConstraints: c\nDecisions: d\nCompleted: c\nIn progress: i\nNext exact action: n\nRelevant files and revisions: r\nVerification status: v\nFailed attempts and why: f";
        assert_eq!(parse_summary(valid), valid);
    }

    /// 缺核心字段且内容极短（<60 字节，像“好的，我来压缩”）：判无效。
    #[test]
    fn summary_parser_rejects_empty_and_tiny_output() {
        assert!(parse_summary("").is_empty());
        assert!(parse_summary("Goal: only one field").is_empty());
        assert!(parse_summary("好的，我来压缩这段历史").is_empty());
    }

    /// §用户诉求：核心字段缺失但有实质内容（≥60 字节）→ 退化返回全文，
    /// 不再卡死（压缩失败比继续膨胀好；is_significant_shrink 仍会把关）。
    #[test]
    fn summary_parser_falls_back_to_full_text_when_core_missing() {
        let no_next = "Goal: g\nIn progress: i\nConstraints: c\nDecisions: d\nCompleted: c";
        let parsed = parse_summary(no_next);
        assert!(!parsed.is_empty(), "有实质内容必须兜底返回");
        assert_eq!(parsed, no_next, "兜底返回原文");
    }

    /// §用户诉求：字段名支持别名（大小写不敏感、相近措辞、中文）。
    #[test]
    fn summary_parser_accepts_field_aliases() {
        let input = "Goal: 修 bug\nnext action: 跑测试\nIn-progress: 调代码";
        let parsed = parse_summary(input);
        assert!(
            parsed.contains("Next exact action: 跑测试"),
            "别名 Next action 应归一化: {parsed:?}"
        );
        assert!(
            parsed.contains("In progress: 调代码"),
            "In-progress 应归一化: {parsed:?}"
        );
        assert!(parsed.contains("Goal: 修 bug"));
    }

    /// §用户诉求：行首列表符/粗体标记不判死。
    #[test]
    fn summary_parser_strips_list_prefixes() {
        let input = "- Goal: 目标\n* In progress: 进行中\n1. Next exact action: 下一步";
        let parsed = parse_summary(input);
        assert!(
            parsed.contains("Goal: 目标") && parsed.contains("Next exact action: 下一步"),
            "列表前缀应剥除后解析: {parsed:?}"
        );
    }

    /// 中文模型输出中文字段名：也解析为规范字段。
    #[test]
    fn summary_parser_accepts_chinese_field_names() {
        let input = "目标: 修复菜单\n进行中: 调整渲染\n下一步: 验证";
        let parsed = parse_summary(input);
        assert!(
            parsed.contains("Goal: 修复菜单") && parsed.contains("In progress: 调整渲染"),
            "中文字段名应归一化: {parsed:?}"
        );
    }

    /// §用户诉求：容错——非字段行、未知字段、重复字段、空值辅助字段都不判死；
    /// 核心字段齐全即有效。
    #[test]
    fn summary_parser_tolerates_noise_and_missing_aux_fields() {
        let base = "Goal: 修复侧边栏\nIn progress: 加宽边栏\nNext exact action: 改宽度常量";
        // 非字段引导语 + 未知字段 + 重复核心字段 + 空值辅助字段：全部忽略，仍有效。
        let noisy =
            format!("以下是摘要：\n{base}\nGoal: 重复（忽略）\nConstraints:\nUnknown: x\n尾部说明");
        let parsed = parse_summary(&noisy);
        assert!(!parsed.is_empty(), "核心字段齐全必须有效: {parsed:?}");
        assert!(parsed.contains("修复侧边栏"), "保留核心字段值: {parsed:?}");
        assert!(!parsed.contains("重复"), "重复字段取第一个: {parsed:?}");
        assert!(!parsed.contains("Unknown"), "未知字段被忽略: {parsed:?}");
        assert!(!parsed.contains("以下是摘要"), "引导语被忽略: {parsed:?}");
    }

    /// §用户诉求：字段值允许含冒号（split 只切第一个冒号）。
    #[test]
    fn summary_parser_keeps_colons_inside_values() {
        let with_colon = "Goal: 修复 src/main.rs: 让缓存生效\nIn progress: i\nNext exact action: n";
        let parsed = parse_summary(with_colon);
        assert!(
            parsed.contains("src/main.rs: 让缓存生效"),
            "冒号后的值必须保留: {parsed:?}"
        );
    }

    /// §用户诉求：辅助字段缺失（只剩核心三个）仍有效。
    #[test]
    fn summary_parser_accepts_core_fields_only() {
        let core_only = "Goal: g\nIn progress: i\nNext exact action: n";
        let parsed = parse_summary(core_only);
        assert!(!parsed.is_empty(), "只有核心字段也必须有效");
        assert_eq!(parsed, core_only);
    }

    #[test]
    fn summary_parser_empty_input_is_invalid() {
        assert!(parse_summary("").is_empty());
        assert!(parse_summary("   \n  ").is_empty());
    }
}
