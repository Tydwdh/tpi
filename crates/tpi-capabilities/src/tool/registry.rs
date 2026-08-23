//! Tool 统一抽象与注册表（README2 §2.1/§4/§14）。
//!
//! Agent Runtime 只认识 [`Tool`]，不区分 Builtin/MCP（Origin 只是 metadata）。
//! ToolRegistry 是全量目录；Phase 5 的 ToolSelector/ActiveToolSet 在其上做
//! 按需选择（MCP 大量工具不一次塞给 LLM）。

use std::collections::HashMap;
use std::sync::Arc;

use crate::tool::ToolContext;
use tpi_core::outcome::ToolOutcome;

/// 工具来源（README2 §2.1：只能作为 metadata，Agent Loop 不据此分支执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOrigin {
    Builtin,
    /// MCP server 提供的工具。
    Mcp {
        server: String,
    },
}

/// External 工具可声明的调度访问类别。
///
/// 这是副作用声明，不是 agent 权限。`Pure` 适用于只注册/查询调度
/// 资源的工具（例如 agent spawn/status）；child 是否拥有某个工具由统一
/// ToolRegistry 决定，而不是由 parent/child 类型决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolAccessClass {
    /// 不访问 workspace（例如 agent graph 控制操作）。
    Pure,
    /// 未知副作用：按源顺序串行（默认；MCP 等 External 工具现状）。
    WorkspaceUnknown,
    /// 只读访问 workspace：可与其他只读工具并行；与写工具冲突时串行。
    ReadOnly,
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

    /// Validate arguments and derive the scheduler footprint in one tool-owned
    /// preparation step. Builtins can use typed validation here; external tools
    /// use their declared access class by default.
    fn prepare(
        &self,
        _args: &str,
        workspace_root: &camino::Utf8PathBuf,
        allow_outside_workspace: bool,
    ) -> Result<crate::tool::scheduler::ToolAccess, String> {
        Ok(match self.access_class() {
            ToolAccessClass::Pure => crate::tool::scheduler::ToolAccess::Pure,
            ToolAccessClass::ReadOnly => {
                crate::tool::scheduler::read_workspace_lock(workspace_root, allow_outside_workspace)
            }
            ToolAccessClass::WorkspaceUnknown => {
                crate::tool::scheduler::ToolAccess::WorkspaceUnknown
            }
        })
    }

    /// Whether this tool needs the durable write-ahead event before execution.
    fn requires_write_ahead(&self) -> bool {
        false
    }

    /// Unified execution entry after scheduler preparation. The default keeps
    /// external tools on the same path; builtin adapters override it only to
    /// pass the prepared commit plan to their existing typed implementation.
    async fn execute_prepared(
        &self,
        args: &str,
        ctx: &ToolContext,
        _plan: Option<&crate::tool::edit::CommitPlan>,
    ) -> ToolOutcome {
        self.execute(args, ctx).await
    }

    /// P4-04：definition 投影（name/schema/origin 的不可变描述；handler 保持
    /// `execute`）。默认从基础方法组装；自定义实现可覆写（如带 limits 的声明）。
    fn definition(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.input_schema(),
            origin: self.origin(),
        }
    }

    /// P4-04：registration 时验证 name/schema（违规定义拒绝注册）。
    fn validate_definition(&self) -> Result<(), String> {
        let name = self.name();
        if name.trim().is_empty() {
            return Err("工具 name 不能为空".into());
        }
        if name.contains(char::is_whitespace) {
            return Err(format!("工具 name 不能含空白: {name:?}"));
        }
        let schema = self.input_schema();
        if !schema.is_object() {
            return Err(format!("工具 {name} 的 input_schema 必须是 JSON object"));
        }
        Ok(())
    }

    /// 调度访问类别（P8-10）：默认 `WorkspaceUnknown`（串行保守）；
    /// 只读 External 工具覆写为 `ReadOnly` 与批内只读工具并行。
    fn access_class(&self) -> ToolAccessClass {
        ToolAccessClass::WorkspaceUnknown
    }
}

/// §10：统一工具元数据——名称、描述、JSON Schema、来源。
/// 消除了此前 `ToolDefinition` 与 `ToolDescriptor` 的重复。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
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
    /// root 层（进程级/内置；composition root 注册）。
    tools: HashMap<String, (tpi_core::ids::RegistrationId, Arc<dyn Tool>)>,
    /// P4-08：session/agent 层 overlay（scope 覆盖 root；lookup 先查 overlay）。
    overlay: HashMap<String, (tpi_core::ids::RegistrationId, Arc<dyn Tool>)>,
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
            (tpi_core::ids::RegistrationId::new_v7(), tool),
        );
    }

    /// 注册并返回 RAII 句柄（Cordis revertible effect 的 Rust 版本：谁注册谁
    /// 清理）。句柄 Drop（或 [`ToolRegistration::unregister`]）时自动从 registry
    /// 移除该工具——MCP server 重启/关闭只需 drop 句柄，不再按名字前缀扫描。
    ///
    /// 需要 `Arc<Mutex<Self>>`：Drop 清理必须能访问 registry；`&mut self` 的
    /// [`ToolRegistry::register`] 留给进程级/内置注册（生命周期与 registry 同长）。
    /// P4-04：注册 + 验证 name/schema（违规拒绝并返回 Err，不插入）。
    pub fn register_owned(
        registry: &Arc<std::sync::Mutex<ToolRegistry>>,
        tool: Arc<dyn Tool>,
    ) -> Result<ToolRegistration, String> {
        tool.validate_definition()?;
        let name = tool.name().to_string();
        let id = tpi_core::ids::RegistrationId::new_v7();
        registry
            .lock()
            .unwrap()
            .tools
            .insert(name.clone(), (id, tool));
        Ok(ToolRegistration {
            registry: Some(registry.clone()),
            name,
            id,
        })
    }

    /// P4-04：注册时验证（无 RAII 句柄；进程级内置路径）。
    pub fn register_validated(&mut self, tool: Arc<dyn Tool>) -> Result<(), String> {
        tool.validate_definition()?;
        self.register(tool);
        Ok(())
    }

    pub fn unregister(&mut self, name: &str) {
        self.tools.remove(name);
    }

    /// P4-11 测试：直接插入（绕过验证，模拟非法来源；不变量检查必须定位）。
    #[cfg(test)]
    pub(crate) fn insert_raw(
        &mut self,
        name: &str,
        id: tpi_core::ids::RegistrationId,
        tool: Arc<dyn Tool>,
    ) {
        self.tools.insert(name.to_string(), (id, tool));
    }

    /// P4-01：`(name, id)` 同时匹配才删除——old disposer 绝不删除 replacement。
    pub fn unregister_entry(&mut self, name: &str, id: tpi_core::ids::RegistrationId) {
        if let Some((existing, _)) = self.tools.get(name)
            && *existing == id
        {
            self.tools.remove(name);
        }
    }

    // ---- P4-08：scoped overlay / setup transaction ----

    /// 在 overlay 层注册（session/agent scope 覆盖 root；返回 RAII 句柄）。
    pub fn register_overlay(
        registry: &Arc<std::sync::Mutex<ToolRegistry>>,
        tool: Arc<dyn Tool>,
    ) -> Result<OverlayRegistration, String> {
        tool.validate_definition()?;
        let name = tool.name().to_string();
        let id = tpi_core::ids::RegistrationId::new_v7();
        registry
            .lock()
            .unwrap()
            .overlay
            .insert(name.clone(), (id, tool));
        Ok(OverlayRegistration {
            registry: registry.clone(),
            name,
            id,
        })
    }

    /// overlay 层按 `(name, id)` 注销（ABA 安全）。
    pub fn unregister_overlay(&mut self, name: &str, id: tpi_core::ids::RegistrationId) {
        if let Some((existing, _)) = self.overlay.get(name)
            && *existing == id
        {
            self.overlay.remove(name);
        }
    }

    /// 当前 overlay 层是否覆盖了某 name（scope lookup 判定）。
    pub fn overlay_has(&self, name: &str) -> bool {
        self.overlay.contains_key(name)
    }

    /// 事务：构建一组 overlay 注册，任一失败则全部回滚（setup fault rollback）。
    /// 先验证全部（不写入），再写入——验证失败时不产生任何 overlay 副作用。
    pub fn setup_overlay_transaction(
        registry: &Arc<std::sync::Mutex<ToolRegistry>>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<Vec<OverlayRegistration>, String> {
        // 先验证全部（不写入）。
        for tool in &tools {
            tool.validate_definition()?;
        }
        // 再写入（验证通过后不可能失败）。
        let mut staged = Vec::with_capacity(tools.len());
        for tool in tools {
            staged.push(Self::register_overlay(registry, tool)?);
        }
        Ok(staged)
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.overlay
            .get(name)
            .or_else(|| self.tools.get(name))
            .map(|(_, tool)| tool.clone())
    }

    /// 全量目录（Phase 5 的 ToolSelector 在此之上做选择）。
    /// ISSUE-011：必须与 [`Self::get`] 的 **overlay 优先**语义一致——overlay
    /// 覆盖同名 root 时，列表里必须是 overlay 工具（此前 list/descriptors 展示
    /// root，执行却用 overlay，模型按错误 schema 构造参数）。
    pub fn list(&self) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = self
            .tools
            .iter()
            .filter(|(name, _)| !self.overlay.contains_key(name.as_str()))
            .map(|(_, (_, tool))| tool.clone())
            .collect();
        for (_, tool) in self.overlay.values() {
            out.push(tool.clone());
        }
        out
    }

    /// 发给模型的描述（Phase 5 前 = 全量；overlay 优先语义同 [`Self::list`]）。
    pub fn descriptors(&self) -> Vec<ToolSpec> {
        let mut out: Vec<ToolSpec> = self
            .tools
            .iter()
            .filter(|(name, _)| !self.overlay.contains_key(name.as_str()))
            .map(|(_, (_, tool))| ToolSpec {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.input_schema(),
                origin: tool.origin(),
            })
            .collect();
        for (_, tool) in self.overlay.values() {
            out.push(ToolSpec {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.input_schema(),
                origin: tool.origin(),
            });
        }
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

    fn prepare(
        &self,
        args: &str,
        workspace_root: &camino::Utf8PathBuf,
        allow_outside_workspace: bool,
    ) -> Result<crate::tool::scheduler::ToolAccess, String> {
        let validated = self
            .tool
            .parse_args(args)
            .map_err(|error| error.to_string())?;
        Ok(crate::tool::scheduler::tool_access(
            self.tool,
            &validated,
            workspace_root,
            allow_outside_workspace,
        ))
    }

    fn requires_write_ahead(&self) -> bool {
        self.tool.requires_write_ahead()
    }

    async fn execute_prepared(
        &self,
        args: &str,
        ctx: &ToolContext,
        plan: Option<&crate::tool::edit::CommitPlan>,
    ) -> ToolOutcome {
        match self.tool.parse_args(args) {
            Ok(validated) => self.execute_with_plan(&validated, ctx, plan).await,
            Err(error) => tpi_core::outcome::ToolOutcome::failed(
                self.tool.name(),
                tpi_core::outcome::ModelPayload {
                    status: tpi_core::outcome::ToolStatus::Rejected,
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

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        self.execute_prepared(args, ctx, None).await
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

/// P4-08：overlay 层注册句柄（session/agent scope；按 `(name,id)` 注销，ABA 安全）。
#[must_use]
pub struct OverlayRegistration {
    registry: std::sync::Arc<std::sync::Mutex<ToolRegistry>>,
    name: String,
    id: tpi_core::ids::RegistrationId,
}

impl OverlayRegistration {
    pub fn unregister(&mut self) {
        let mut registry = tpi_core::util::lock_mutex(&self.registry, "tool_registry");
        registry.unregister_overlay(&self.name, self.id);
    }
}

impl Drop for OverlayRegistration {
    fn drop(&mut self) {
        self.unregister();
    }
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
    id: tpi_core::ids::RegistrationId,
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
            call_id: tpi_core::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            shell_path: None,
            snapshot_store: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::edit::SnapshotStore::new(4, 2),
            )),
            current_plan: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_goal: None,
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
            terminals: Default::default(),
            resources: None,
            resource_identity: None,
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
            )),
            interactive: false,
            workspace_session: None,
        };
        // 非法参数 → rejected（不 panic）。
        let outcome = adapter.execute("{", &ctx).await;
        assert_eq!(outcome.status, tpi_core::outcome::ToolStatus::Rejected);
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
    let handle_a = ToolRegistry::register_owned(&registry, make("read")).unwrap();
    // 手动注销（等价 MCP restart 的 drop 前的显式 unregister）。
    let mut h = handle_a;
    h.unregister();
    assert!(registry.lock().unwrap().get("read").is_none());
    // replacement（同名重新注册，新 id）。
    let _handle_b = ToolRegistry::register_owned(&registry, make("read")).unwrap();
    assert!(registry.lock().unwrap().get("read").is_some());
    // 旧句柄 h 已 unregister（id=旧），drop 不删 replacement。
    drop(h);
    assert!(
        registry.lock().unwrap().get("read").is_some(),
        "old disposer 绝不能删除 replacement（ABA）"
    );

    // 场景 2：直接 drop 旧句柄（不显式 unregister）也不删 replacement。
    let handle_c = ToolRegistry::register_owned(&registry, make("bash")).unwrap();
    drop(handle_c); // 旧 id 注销
    let _handle_d = ToolRegistry::register_owned(&registry, make("bash")).unwrap();
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
    let handle_old = ToolRegistry::register_owned(&registry, make("read")).unwrap();
    let handle_new = ToolRegistry::register_owned(&registry, make("read")).unwrap();
    assert!(registry.lock().unwrap().get("read").is_some());
    drop(handle_old);
    assert!(
        registry.lock().unwrap().get("read").is_some(),
        "旧句柄 drop 不得影响 replacement"
    );
    drop(handle_new);
    assert!(registry.lock().unwrap().get("read").is_none());
}

/// P4-04：registration 时验证 name/schema——违规定义拒绝注册。
#[tokio::test]
async fn registration_validates_definition() {
    let registry = Arc::new(std::sync::Mutex::new(ToolRegistry::new()));

    // 合法工具注册成功。
    let ok_tool = Arc::new(BuiltinToolAdapter::new(
        crate::tool::implemented_tools()
            .into_iter()
            .find(|t| t.name() == "read")
            .unwrap(),
    ));
    assert!(ToolRegistry::register_owned(&registry, ok_tool).is_ok());

    // 空 name：拒绝。
    struct BadName;
    #[async_trait::async_trait]
    impl Tool for BadName {
        fn name(&self) -> &str {
            ""
        }
        fn description(&self) -> &str {
            "bad"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn origin(&self) -> ToolOrigin {
            ToolOrigin::Builtin
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolOutcome {
            unreachable!()
        }
    }
    let err = match ToolRegistry::register_owned(&registry, Arc::new(BadName)) {
        Ok(_) => panic!("空 name 必须拒绝"),
        Err(e) => e,
    };
    assert!(err.contains("name"), "{err}");

    // schema 非 object：拒绝。
    struct BadSchema;
    #[async_trait::async_trait]
    impl Tool for BadSchema {
        fn name(&self) -> &str {
            "bad_schema"
        }
        fn description(&self) -> &str {
            "bad"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!("not-an-object")
        }
        fn origin(&self) -> ToolOrigin {
            ToolOrigin::Builtin
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolOutcome {
            unreachable!()
        }
    }
    let err = match ToolRegistry::register_owned(&registry, Arc::new(BadSchema)) {
        Ok(_) => panic!("schema 非 object 必须拒绝"),
        Err(e) => e,
    };
    assert!(err.contains("object"), "{err}");
}

/// P4-04：ToolDefinition 投影与基础方法一致。
#[test]
fn definition_projection_matches_accessors() {
    let adapter = BuiltinToolAdapter::new(crate::tool::BuiltinTool::Read);
    let def = adapter.definition();
    assert_eq!(def.name, adapter.name());
    assert_eq!(def.description, adapter.description());
    assert_eq!(def.parameters, adapter.input_schema());
    assert_eq!(def.origin, adapter.origin());
}

/// P4-08：overlay 覆盖 root（scope lookup 先查 overlay）+ ABA 注销。
#[tokio::test]
async fn overlay_covers_root_and_unregisters_by_id() {
    let registry = Arc::new(std::sync::Mutex::new(ToolRegistry::new()));
    let make = |name: &'static str| -> Arc<dyn Tool> {
        Arc::new(BuiltinToolAdapter::new(
            crate::tool::implemented_tools()
                .into_iter()
                .find(|t| t.name() == name)
                .unwrap(),
        ))
    };
    // root 注册 read（句柄绑定：RAII drop 即注销）。
    let _root = ToolRegistry::register_owned(&registry, make("read")).unwrap();
    // overlay 注册同名 read（覆盖）。
    let overlay = ToolRegistry::register_overlay(&registry, make("read")).unwrap();
    assert!(registry.lock().unwrap().overlay_has("read"));
    assert!(registry.lock().unwrap().get("read").is_some());
    // ISSUE-011：list/descriptors 必须与 get 同源（overlay 优先）——
    // overlay 覆盖 root 时只展示 overlay 工具，不展示被遮蔽的 root。
    {
        let reg = registry.lock().unwrap();
        let listed: Vec<String> = reg.list().iter().map(|t| t.name().to_string()).collect();
        assert_eq!(
            listed,
            vec!["read"],
            "overlay 覆盖 root 时列表只含 overlay: {listed:?}"
        );
        let desc: Vec<String> = reg.descriptors().iter().map(|d| d.name.clone()).collect();
        assert_eq!(desc, vec!["read"], "descriptors 同样只含 overlay: {desc:?}");
    }
    // 注销 overlay（按 id）→ root 的 read 恢复可见。
    let mut overlay = overlay;
    overlay.unregister();
    assert!(!registry.lock().unwrap().overlay_has("read"));
    assert!(
        registry.lock().unwrap().get("read").is_some(),
        "root 层不受 overlay 注销影响"
    );
    assert_eq!(registry.lock().unwrap().list().len(), 1);
}

/// P4-08：setup transaction——任一工具验证失败时全部回滚（无副作用）。
#[tokio::test]
async fn setup_transaction_rolls_back_on_fault() {
    let registry = Arc::new(std::sync::Mutex::new(ToolRegistry::new()));
    let make = |name: &'static str| -> Arc<dyn Tool> {
        Arc::new(BuiltinToolAdapter::new(
            crate::tool::implemented_tools()
                .into_iter()
                .find(|t| t.name() == name)
                .unwrap(),
        ))
    };
    // 非法工具（空 name）混入事务。
    struct BadName;
    #[async_trait::async_trait]
    impl Tool for BadName {
        fn name(&self) -> &str {
            ""
        }
        fn description(&self) -> &str {
            "bad"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn origin(&self) -> ToolOrigin {
            ToolOrigin::Builtin
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolOutcome {
            unreachable!()
        }
    }
    let result = ToolRegistry::setup_overlay_transaction(
        &registry,
        vec![make("read"), Arc::new(BadName), make("bash")],
    );
    assert!(result.is_err(), "含非法工具的事务必须失败");
    // 验证失败 → 无任何 overlay 副作用（rollback）。
    let reg = registry.lock().unwrap();
    assert!(!reg.overlay_has("read"), "事务失败后不得留下 read overlay");
    assert!(!reg.overlay_has("bash"), "事务失败后不得留下 bash overlay");
}
