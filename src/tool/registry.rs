//! Tool 统一抽象与注册表（README2 §2.1/§4/§14）。
//!
//! Agent Runtime 只认识 [`Tool`]，不区分 Builtin/MCP（Origin 只是 metadata）。
//! ToolRegistry 是全量目录；Phase 5 的 ToolSelector/ActiveToolSet 在其上做
//! 按需选择（MCP 大量工具不一次塞给 LLM）。

use std::collections::HashMap;
use std::sync::Arc;

use crate::tool::outcome::ToolOutcome;
use crate::tool::ToolContext;

/// 工具来源（README2 §2.1：只能作为 metadata，Agent Loop 不据此分支执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOrigin {
    Builtin,
    /// MCP server 提供的工具。
    Mcp { server: String },
}

impl std::fmt::Display for ToolOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolOrigin::Builtin => write!(f, "builtin"),
            ToolOrigin::Mcp { server } => write!(f, "mcp::{server}"),
        }
    }
}

/// 统一工具接口（README2 §2.1/§10）。
///
/// `args` 是 JSON 字符串（与现有 `parse_args(&str)` 输入格式一致）；
/// 返回 [`ToolOutcome`]（模型/UI/session 已消费的结构，不重复造 ToolResult）。
/// `#[async_trait]`：async fn in trait 的对象安全（dyn Tool 需要）。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema（schemars 生成；与现有 `BuiltinTool::schema().parameters` 同构）。
    fn input_schema(&self) -> serde_json::Value;
    fn origin(&self) -> ToolOrigin;
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome;
}

/// 工具描述（发给模型的 schema，对齐现有 `ToolDef`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub origin: ToolOrigin,
}

/// 工具注册表（README2 §4：只管理 Tool，不管 MCP 生命周期）。
/// 不 derive Debug：`Arc<dyn Tool>` 不实现 Debug。
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册（覆盖同名；内部名必须全局唯一，README2 §5）。
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn unregister(&mut self, name: &str) {
        self.tools.remove(name);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// 全量目录（Phase 5 的 ToolSelector 在此之上做选择）。
    pub fn list(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.values().cloned().collect()
    }

    /// 发给模型的描述（Phase 5 前 = 全量）。
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut out: Vec<ToolDescriptor> = self
            .tools
            .values()
            .map(|tool| ToolDescriptor {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.input_schema(),
                origin: tool.origin(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// BuiltinTool 的 Tool 适配（README2 §2.1：builtin 与 MCP 统一走 Tool 接口）。
///
/// 执行复用现有类型化路径：parse_args（schema 校验）→ 内部 `tool::execute`
///（含 commit plan 语义）。plan 参数由 scheduler 层传入（`execute_with_plan`），
/// `Tool::execute` 默认走无 plan 路径（edit/write 的 commit plan 由调用方
/// 通过 `execute_with_plan` 提供，保持现有无回归语义）。
pub struct BuiltinToolAdapter {
    tool: crate::tool::BuiltinTool,
}

impl BuiltinToolAdapter {
    pub fn new(tool: crate::tool::BuiltinTool) -> Self {
        Self { tool }
    }

    pub fn tool(&self) -> crate::tool::BuiltinTool {
        self.tool
    }

    /// 带 commit plan 的类型化执行（agent scheduler 专用，保留 edit/write
    /// backup journal 语义）。
    pub async fn execute_with_plan(
        &self,
        args: &crate::tool::ValidatedArgs,
        ctx: &ToolContext,
        plan: Option<&crate::tool::edit::CommitPlan>,
    ) -> ToolOutcome {
        crate::tool::execute(self.tool, args.clone(), ctx, plan).await
    }
}

#[async_trait::async_trait]
impl Tool for BuiltinToolAdapter {
    fn name(&self) -> &str {
        self.tool.name()
    }

    fn description(&self) -> &str {
        self.tool.description()
    }

    fn input_schema(&self) -> serde_json::Value {
        self.tool.schema().parameters
    }

    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match self.tool.parse_args(args) {
            Ok(validated) => crate::tool::execute(self.tool, validated, ctx, None).await,
            Err(error) => crate::tool::outcome::ToolOutcome::failed(
                self.tool.name(),
                crate::tool::outcome::ModelPayload {
                    status: crate::tool::outcome::ToolStatus::Rejected,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: rejected\ntool: {}\nerror: invalid_arguments\n\n{error}",
                        self.tool.name()
                    ),
                    effect: None,
                    artifact: None,
                },
            ),
        }
    }
}

/// 注册所有 builtin 工具（agent 启动时构建全量目录）。
#[allow(dead_code)] // Phase 2 接线 MCP adapter 后由 agent 使用
pub fn builtin_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in crate::tool::implemented_tools() {
        registry.register(Arc::new(BuiltinToolAdapter::new(tool)));
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn builtin_adapter_exposes_all_builtin_tools() {
        let registry = builtin_registry();
        assert_eq!(registry.len(), crate::tool::implemented_tools().len());
        for tool in crate::tool::implemented_tools() {
            assert!(
                registry.get(tool.name()).is_some(),
                "builtin {} 必须注册",
                tool.name()
            );
        }
    }

    #[tokio::test]
    async fn builtin_adapter_executes_read_with_invalid_args_rejected() {
        let adapter = BuiltinToolAdapter::new(crate::tool::BuiltinTool::Read);
        let ctx = crate::tool::ToolContext {
            workspace_root: camino::Utf8PathBuf::from("/tmp"),
            allow_outside_workspace: true,
            cancel: tokio_util::sync::CancellationToken::new(),
            artifacts_root: std::path::PathBuf::from("/tmp/art"),
            session_id: "test".into(),
            call_id: crate::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            shell_path: None,
            snapshot_store: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::edit::SnapshotStore::new(4, 2),
            )),
            current_plan: std::sync::Arc::new(std::sync::Mutex::new(None)),
            shell: std::sync::Arc::new(std::sync::Mutex::new(crate::shell::ShellSessionState::new(
                camino::Utf8PathBuf::from("/tmp"),
            ))),
            workspace: std::sync::Arc::new(std::sync::Mutex::new(
                crate::workspace::ActiveWorkspace::local(crate::workspace::LocalWorkspace::new(
                    camino::Utf8PathBuf::from("/tmp"),
                    true,
                )),
            )),
            interactive: false,
        };
        // 非法参数 → rejected（不 panic）。
        let outcome = adapter.execute("{", &ctx).await;
        assert_eq!(outcome.status, crate::tool::outcome::ToolStatus::Rejected);
        assert!(outcome.model_payload.output.contains("invalid_arguments"));
    }
}

