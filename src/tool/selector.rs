//! ToolSelector / ActiveToolSet（README2 §14：MCP + Skills 数量增加 →
//! Context 爆炸；按需选择本轮相关工具）。
//!
//! ```text
//! ToolRegistry (全量目录)
//!     ↓
//! ToolSelector (按上下文相关性选择)
//!     ↓
//! ActiveToolSet (本轮发给模型)
//!     ↓
//! LLM
//! ```
//!
//! 策略：builtin 工具始终保留（核心能力）；MCP 工具按「上下文关键词」匹配
//! name/description 选择；总量有上限，防止 Context 随工具数线性膨胀。

use super::registry::{ToolDescriptor, ToolOrigin};

/// 单轮发给模型的工具数上限（builtin 10 + MCP 精选）。
pub const MAX_ACTIVE_TOOLS: usize = 32;

/// ToolSelector：从全量目录选择本轮相关工具。
#[derive(Debug, Clone, Copy)]
pub struct ToolSelector {
    pub max_tools: usize,
}

impl Default for ToolSelector {
    fn default() -> Self {
        Self {
            max_tools: MAX_ACTIVE_TOOLS,
        }
    }
}

impl ToolSelector {
    /// 选择 ActiveToolSet（README2 §14）。
    ///
    /// `context`：用户消息 + 最近上下文文本（用于关键词匹配）。
    pub fn select(&self, descriptors: Vec<ToolDescriptor>, context: &str) -> Vec<ToolDescriptor> {
        let keywords = extract_keywords(context);
        let mut active: Vec<ToolDescriptor> = Vec::new();
        let mut mcp_candidates: Vec<ToolDescriptor> = Vec::new();

        // builtin 全保留（核心能力，README2 §14：不能因为 MCP 膨胀挤掉）。
        for desc in descriptors {
            match &desc.origin {
                ToolOrigin::Builtin => active.push(desc),
                ToolOrigin::Mcp { .. } => {
                    if relevance(&desc, &keywords) > 0 {
                        mcp_candidates.push(desc);
                    }
                }
            }
        }
        // MCP 按相关度降序，直到上限。
        mcp_candidates.sort_by(|a, b| {
            relevance(b, &keywords)
                .cmp(&relevance(a, &keywords))
                .then_with(|| a.name.cmp(&b.name))
        });
        let budget = self.max_tools.saturating_sub(active.len());
        active.extend(mcp_candidates.into_iter().take(budget));
        // 保持稳定顺序（builtin 原序 + MCP 相关序）。
        active
    }
}

/// 上下文 → 关键词集合（简单分词：非字母数字分隔，长度 > 2）。
fn extract_keywords(context: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for word in context.split(|c: char| !c.is_alphanumeric()) {
        let word = word.to_lowercase();
        if word.len() > 2 && seen.insert(word.clone()) {
            out.push(word);
        }
    }
    out
}

/// 工具与关键词的相关度（name 命中权重高；description 命中计数）。
fn relevance(desc: &ToolDescriptor, keywords: &[String]) -> usize {
    let name = desc.name.to_lowercase();
    let desc_text = desc.description.to_lowercase();
    let mut score = 0usize;
    for keyword in keywords {
        if name.contains(keyword) {
            score += 3;
        } else if desc_text.contains(keyword) {
            score += 1;
        }
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(name: &str, origin: ToolOrigin) -> ToolDescriptor {
        ToolDescriptor {
            name: name.into(),
            description: format!("{name} 工具：用于相关操作"),
            parameters: serde_json::json!({"type": "object"}),
            origin,
        }
    }

    #[test]
    fn builtin_always_kept() {
        let selector = ToolSelector::default();
        let mut all = vec![
            desc("read", ToolOrigin::Builtin),
            desc("bash", ToolOrigin::Builtin),
            desc("mcp::github::create_issue", ToolOrigin::Mcp { server: "github".into() }),
        ];
        // 无关上下文：builtin 全保留，MCP 被过滤。
        let active = selector.select(std::mem::take(&mut all), "just hello world");
        let names: Vec<&str> = active.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["read", "bash"], "builtin 必须保留：{names:?}");
    }

    #[test]
    fn mcp_tools_selected_by_context_keywords() {
        let selector = ToolSelector::default();
        let all = vec![
            desc("mcp::github::create_issue", ToolOrigin::Mcp { server: "github".into() }),
            desc("mcp::db::query", ToolOrigin::Mcp { server: "db".into() }),
        ];
        // 上下文提到 issue → github 工具相关，db 不相关。
        let active = selector.select(all, "帮我创建一个 issue 描述这个 bug");
        let names: Vec<&str> = active.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"mcp::github::create_issue"), "含相关 MCP 工具：{names:?}");
        assert!(!names.contains(&"mcp::db::query"), "无关 MCP 工具被过滤：{names:?}");
    }

    #[test]
    fn total_capped_at_max_tools() {
        let selector = ToolSelector { max_tools: 4 };
        let mut all: Vec<ToolDescriptor> = (0..10)
            .map(|i| desc(&format!("mcp::s::t{i}"), ToolOrigin::Mcp { server: "s".into() }))
            .collect();
        // 上下文命中所有工具名（t0..t9 关键词 t0 等无意义；用通用词）。
        let context = "t0 t1 t2 t3 t4 t5 t6 t7 t8 t9 tool 操作 相关";
        let active = selector.select(std::mem::take(&mut all), context);
        assert!(active.len() <= 4, "总工具数受上限约束：{}", active.len());
    }
}
