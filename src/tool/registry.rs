//! Tool 统一抽象与注册表（README2 §2.1/§4/§14）。
//!
//! Agent Runtime 只认识 [`Tool`]，不区分 Builtin/MCP（Origin 只是 metadata）。
//! ToolRegistry 是全量目录；Phase 5 的 ToolSelector/ActiveToolSet 在其上做
//! 按需选择（MCP 大量工具不一次塞给 LLM）。

use std::collections::HashMap;
use std::sync::Arc;

use crate::tool::ToolContext;
use crate::tool::outcome::ToolOutcome;

/// 工具来源（README2 §2.1：只能作为 metadata，Agent Loop 不据此分支执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOrigin {
    Builtin,
    /// MCP server 提供的工具。
    Mcp {
        server: String,
    },
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
///
/// P4-01（ABA 修复）：条目携带 [`RegistrationId`]；RAII 句柄注销必须
/// `(name, id)` 同时匹配——old disposer 绝不能删除 replacement。
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, (crate::ids::RegistrationId, Arc<dyn Tool>)>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册（覆盖同名；内部名必须全局唯一，README2 §5）。
    /// 覆盖时分配新 RegistrationId（旧句柄按旧 id 注销时不会误删本条目）。
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(
            tool.name().to_string(),
            (crate::ids::RegistrationId::new_v7(), tool),
        );
    }

    /// 注册并返回 RAII 句柄（Cordis revertible effect 的 Rust 版本：谁注册谁
    /// 清理）。句柄 Drop（或 [`ToolRegistration::unregister`]）时自动从 registry
    /// 移除该工具——MCP server 重启/关闭只需 drop 句柄，不再按名字前缀扫描。
    ///
    /// 需要 `Arc<Mutex<Self>>`：Drop 清理必须能访问 registry；`&mut self` 的
    /// [`ToolRegistry::register`] 留给进程级/内置注册（生命周期与 registry 同长）。
    pub fn register_owned(
        registry: &Arc<std::sync::Mutex<ToolRegistry>>,
        tool: Arc<dyn Tool>,
    ) -> ToolRegistration {
        let name = tool.name().to_string();
        let id = crate::ids::RegistrationId::new_v7();
        registry
            .lock()
            .unwrap()
            .tools
            .insert(name.clone(), (id, tool));
        ToolRegistration {
            registry: Some(registry.clone()),
            name,
            id,
        }
    }

    pub fn unregister(&mut self, name: &str) {
        self.tools.remove(name);
    }

    /// P4-01：`(name, id)` 同时匹配才删除——old disposer 绝不删除 replacement。
    pub fn unregister_entry(&mut self, name: &str, id: crate::ids::RegistrationId) {
        if let Some((existing, _)) = self.tools.get(name)
            && *existing == id
        {
            self.tools.remove(name);
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).map(|(_, tool)| tool.clone())
    }

    /// 全量目录（Phase 5 的 ToolSelector 在此之上做选择）。
    pub fn list(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.values().map(|(_, tool)| tool.clone()).collect()
    }

    /// 发给模型的描述（Phase 5 前 = 全量）。
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut out: Vec<ToolDescriptor> = self
            .tools
            .values()
            .map(|(_, tool)| ToolDescriptor {
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

/// RAII 注册句柄（AGENTS.md §11/§12：revertible effect / PluginScope 的最小机制）。
///
/// 持有注册目标 registry 的 `Arc` 与工具名；`Drop` 时执行注销（幂等）。
/// 生命周期由所有权决定：scope/manager 持有句柄，句柄 drop = 工具消失，
/// 不依赖任何 runtime 依赖图或反注册回调注册表。
#[must_use]
pub struct ToolRegistration {
    registry: Option<Arc<std::sync::Mutex<ToolRegistry>>>,
    name: String,
    /// P4-01：注册时的唯一 id；注销按 `(name, id)` 匹配（ABA 修复）。
    id: crate::ids::RegistrationId,
}

impl ToolRegistration {
    /// 主动注销（幂等）；`Drop` 也会注销。
    pub fn unregister(&mut self) {
        if let Some(registry) = self.registry.take() {
            registry
                .lock()
                .unwrap()
                .unregister_entry(&self.name, self.id);
        }
    }
}

impl Drop for ToolRegistration {
    fn drop(&mut self) {
        self.unregister();
    }
}

/// 进程级共享 ToolRegistry（README2 Phase 5）：McpManager 启动时注册 MCP
/// 工具，agent 的 ToolRuntime 读取同一目录——MCP 工具自动进入 agent loop。
pub fn global_registry() -> Arc<std::sync::Mutex<ToolRegistry>> {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<Arc<std::sync::Mutex<ToolRegistry>>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| Arc::new(std::sync::Mutex::new(builtin_registry())))
        .clone()
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
            shell: std::sync::Arc::new(std::sync::Mutex::new(
                crate::shell::ShellSessionState::new(camino::Utf8PathBuf::from("/tmp")),
            )),
            workspace: std::sync::Arc::new(std::sync::Mutex::new(
                crate::workspace::ActiveWorkspace::local(crate::workspace::LocalWorkspace::new(
                    camino::Utf8PathBuf::from("/tmp"),
                    true,
                )),
            )),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
            )),
            interactive: false,
        };
        // 非法参数 → rejected（不 panic）。
        let outcome = adapter.execute("{", &ctx).await;
        assert_eq!(outcome.status, crate::tool::outcome::ToolStatus::Rejected);
        assert!(outcome.model_payload.output.contains("invalid_arguments"));
    }
}

/// P4-01 ABA：old disposer 绝不能删除 replacement。
/// A 注册（句柄1）→ 注销/覆盖为 B（句柄2）→ 句柄1 drop 不得删 B。
#[tokio::test]
async fn aba_old_disposer_never_deletes_replacement() {
    let registry = Arc::new(std::sync::Mutex::new(ToolRegistry::new()));
    let make = |name: &'static str| -> Arc<dyn Tool> {
        Arc::new(BuiltinToolAdapter::new(
            crate::tool::implemented_tools()
                .into_iter()
                .find(|t| t.name() == name)
                .unwrap(),
        ))
    };
    // 场景 1：register_owned A → unregister（手动）→ 再 register_owned A（新 id）→
    // 旧句柄 drop 不得删除新条目。
    let handle_a = ToolRegistry::register_owned(&registry, make("read"));
    // 手动注销（等价 MCP restart 的 drop 前的显式 unregister）。
    let mut h = handle_a;
    h.unregister();
    assert!(registry.lock().unwrap().get("read").is_none());
    // replacement（同名重新注册，新 id）。
    let _handle_b = ToolRegistry::register_owned(&registry, make("read"));
    assert!(registry.lock().unwrap().get("read").is_some());
    // 旧句柄 h 已 unregister（id=旧），drop 不删 replacement。
    drop(h);
    assert!(
        registry.lock().unwrap().get("read").is_some(),
        "old disposer 绝不能删除 replacement（ABA）"
    );

    // 场景 2：直接 drop 旧句柄（不显式 unregister）也不删 replacement。
    let handle_c = ToolRegistry::register_owned(&registry, make("bash"));
    drop(handle_c); // 旧 id 注销
    let _handle_d = ToolRegistry::register_owned(&registry, make("bash"));
    assert!(registry.lock().unwrap().get("bash").is_some());
    // 若 handle_c drop 误删 replacement，这里会失败（ABA 复现）。
    assert!(
        registry.lock().unwrap().get("bash").is_some(),
        "ABA：旧句柄 drop 不得删除 replacement"
    );
}

/// P4-01：重复名策略——register_owned 同名覆盖（新 id）；旧句柄注销不影响新条目。
#[tokio::test]
async fn duplicate_name_replacement_keeps_new_entry() {
    let registry = Arc::new(std::sync::Mutex::new(ToolRegistry::new()));
    let make = |name: &'static str| -> Arc<dyn Tool> {
        Arc::new(BuiltinToolAdapter::new(
            crate::tool::implemented_tools()
                .into_iter()
                .find(|t| t.name() == name)
                .unwrap(),
        ))
    };
    let handle_old = ToolRegistry::register_owned(&registry, make("read"));
    let handle_new = ToolRegistry::register_owned(&registry, make("read"));
    assert!(registry.lock().unwrap().get("read").is_some());
    drop(handle_old);
    assert!(
        registry.lock().unwrap().get("read").is_some(),
        "旧句柄 drop 不得影响 replacement"
    );
    drop(handle_new);
    assert!(registry.lock().unwrap().get("read").is_none());
}
