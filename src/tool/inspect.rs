//! `runtime_inspect` 工具（AGENTS.md §15：Runtime Introspection）。
//!
//! Agent 可以查询自己拥有的能力——而不是只凭 system prompt 猜：
//! 当前工具（含 provider/origin）、可用 skills、workspace 类型与根、
//! 后台进程数量。Runtime 是事实来源；本工具是它的只读投影。
//!
//! 无参数；输出纯文本快照（工具目录 + skills + workspace + processes）。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::ToolContext;
use crate::tool::outcome::ToolOutcome;

/// `runtime_inspect` 参数（无）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct InspectArgs {}

/// 工具入口：返回运行时能力快照。
pub async fn runtime_inspect(_args: InspectArgs, ctx: &ToolContext) -> ToolOutcome {
    // 1. 工具目录（含 origin：builtin / mcp::server）。
    let mut tool_lines: Vec<String> = Vec::new();
    {
        let registry = crate::util::lock_mutex(&ctx.registry, "registry");
        let mut descriptors = registry.descriptors();
        descriptors.sort_by(|a, b| a.name.cmp(&b.name));
        for descriptor in descriptors {
            let origin = match &descriptor.origin {
                crate::tool::registry::ToolOrigin::Builtin => "builtin".to_string(),
                crate::tool::registry::ToolOrigin::Mcp { server } => {
                    format!("mcp::{server}")
                }
            };
            tool_lines.push(format!("  {} ({origin})", descriptor.name));
        }
    }

    // 2. Skills（进程级 SkillManager；只列已发现，不激活）。
    let skill_lines: Vec<String> = {
        let manager = crate::skills::manager::SkillManager::global();
        let guard = crate::util::lock_mutex(&manager, "skill_manager");
        guard
            .available_names()
            .into_iter()
            .map(|n| format!("  {n}"))
            .collect()
    };

    // 3. Workspace（kind + identity：local:path / ssh:host:root）。
    let (ws_kind, ws_identity) = {
        let ws = crate::util::lock_mutex(&ctx.workspace, "workspace");
        let kind = match ws.kind() {
            crate::workspace::WorkspaceKind::Local => "local".to_string(),
            crate::workspace::WorkspaceKind::Remote => "remote".to_string(),
        };
        let identity = ws.id().to_string();
        (kind, identity)
    };

    // 4. Managed processes（数量 + active 行）。
    let process_lines: Vec<String> = {
        let reg = crate::util::lock_mutex(&ctx.processes, "process_registry");
        reg.snapshot_lines(&[])
    };

    let mut out = String::from("runtime introspection\n\n[tools]\n");
    out.push_str(&tool_lines.join("\n"));
    if tool_lines.is_empty() {
        out.push_str("  (none)");
    }
    out.push_str("\n\n[skills]\n");
    out.push_str(&skill_lines.join("\n"));
    if skill_lines.is_empty() {
        out.push_str("  (none)");
    }
    out.push_str(&format!(
        "\n\n[workspace]\n  kind: {ws_kind}\n  identity: {ws_identity}\n\n[processes]\n"
    ));
    if process_lines.is_empty() {
        out.push_str("  (none running)");
    } else {
        out.push_str(&process_lines.join("\n"));
    }
    out.push_str("\n\n（model/provider 由会话配置决定，见 /settings；runtime_inspect 只报告能力）");

    ToolOutcome::succeeded("runtime_inspect", out)
}
