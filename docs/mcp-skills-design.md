# MCP & Skills 架构设计（README2 Phase 0 审计）

> 目标：为现有 Coding Agent 增加 MCP Client 与 Agent Skills。
> 本文档是**写代码之前**的边界说明（README2 §27 Phase 0）。

---

## 1. 现有 Tool 架构分析（README2 §3 十问）

### 当前 Tool 如何定义

`BuiltinTool` enum（`src/tool/mod.rs:154`）+ 每工具一个类型化 Args struct：

```rust
pub enum BuiltinTool { Read, List, Search, Glob, Edit, Write, Bash, UpdatePlan, WebSearch, WebFetch }
```

- `schema()` → `ToolDef`（name/description/parameters，schemars 从 Args struct 生成）
- `parse_args(&str)` → `ValidatedArgs`（enum，schema 校验 + 类型化）
- `implemented_tools()` → `Vec<BuiltinTool>`（静态列表）

### Tool schema 如何暴露给模型

`BuiltinTool::schema()` → `ToolDef`；agent loop 收集 `tool_defs`（`src/agent/mod.rs:376`）
放入 `ModelRequest.tools`。模型看到 10 个工具的完整 schema。

### Tool 如何执行

`tool::execute(tool, args: ValidatedArgs, ctx: &ToolContext, plan)`（`src/tool/mod.rs:700`）：
- **异步工具**（bash/web_search/web_fetch）留在 async 上下文直接跑；
- **同步工具**（read/list/search/glob/edit/write/update_plan）`spawn_blocking` 到阻塞池；
- 结果统一 `ToolOutcome`（model_payload/display_payload/session_metadata/artifacts/timing）。

### Tool 是否支持 async

支持（`execute` 是 `async fn`）；同步工具经 `spawn_blocking` 不阻塞 Tokio worker。

### Tool timeout 在哪里实现

- `bash`：`BashArgs.timeout_ms` → process-host 超时；
- web_search/web_fetch：reqwest 超时；
- 其他工具：无显式 timeout（本地 IO 受 `SCAN_DEADLINE` 等内部预算约束）；
- MCP 需要自己的 timeout 层（Phase 2）。

### Tool error 如何返回模型

统一 `ToolOutcome.model_payload.output` 文本 envelope：

```text
status: succeeded|failed|timed_out|cancelled|rejected
tool: <name>
error: <code>
```

+ `ToolStatus` enum（结构化）+ `ToolMetadata`（session/评测用）。错误不 panic，全部转 ToolOutcome。

### ToolRegistry 是否存在

**不存在**。`implemented_tools()` 返回静态 `Vec<BuiltinTool>`，无注册表、无动态工具概念。
这是 MCP 的最大缺口：MCP 工具无法用 BuiltinTool enum 表达。

### Tool name 是否要求全局唯一

当前天然唯一（enum）。MCP 引入后需命名空间（`mcp::<server>::<tool>`，README2 §5）。

### Tool 是否直接依赖某个模型 Provider

否。工具只依赖 `ToolContext`（workspace/shell/artifacts/cancel 等），Provider 完全解耦。

### Tool 与 Agent Loop 是否耦合

部分耦合：
- scheduler（`src/agent/scheduler.rs`）按 `BuiltinTool` 做资源访问声明（read lock/write lock/WorkspaceUnknown）——**MCP 工具无法进入此类型化路径**；
- execute 的 `plan` 参数（edit/write 的 commit plan）是 builtin 专用概念。

---

## 2. 设计决策

### D1：Tool 是核心抽象（README2 §2.1）

新增统一 trait，builtin 与 MCP 都是 Tool：

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    fn origin(&self) -> ToolOrigin;              // Builtin | Mcp{server}（仅 metadata）
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome;
}
```

- `ToolResult` = 现有 `ToolOutcome`（不重复造轮子；模型/UI/session 已消费其结构）。
- `ToolError` 统一映射（README2 §11）：MCP transport/protocol/server/timeout/process-died
  全部折叠为模型可理解的 `ToolStatus` + 简洁文本；完整错误进 debug log。

### D2：注册表与执行分层（README2 §14）

```text
ToolRegistry (全量目录)          ← builtin + MCP adapter 都注册
    ↓
ToolSelector (Phase 5)           ← 按需选择（MCP 200 个工具不一次塞给 LLM）
    ↓
ActiveToolSet (Phase 5)          ← 本轮真正发给模型的工具
    ↓
LLM
```

Phase 1 只做 ToolRegistry（全量）；ToolSelector/ActiveToolSet 是 Phase 5。

### D3：McpManager 独立（README2 §6）

MCP 生命周期（config/spawn/initialize/capability/tools-list/call/reconnect/shutdown）
放独立 `mcp::` 模块；ToolRegistry 只持有已适配的 Tool。**不把 MCP 逻辑放进 registry/agent loop。**

### D4：SkillManager 独立（README2 §16-§21）

- Skill 是 Instructions/Workflow/Knowledge，**不是 Tool**；
- 内置 `activate_skill` Tool 返回完整 SKILL.md（模型选择后注入 context）；
- Progressive Disclosure：启动只读 name/description；激活才读 body；references 按需读。

### D5：builtin 执行路径保持类型化（无回归）

scheduler 的 `tool_access` 基于 BuiltinTool enum（类型安全资源声明）不重写；
MCP 工具在 scheduler 统一按 `WorkspaceUnknown`（保守串行）执行。

### D6：命名空间（README2 §5）

内部唯一名：`mcp::<server>::<tool>`；模型可见可简化（Phase 3 决定）。

### D7：配置（README2 §8）

`[mcp.servers.<name>]`（command/args/env/enabled/timeout）从 `~/.tpi/config.toml` 读，
不写死源码。

---

## 3. 模块边界（README2 §26，适配当前结构）

```text
src/tool/          现有工具层（ToolContext/ToolOutcome/BuiltinTool）
  ├── registry.rs  ToolRegistry + Tool trait + ToolOrigin + BuiltinToolAdapter   ← Phase 1
src/mcp/           MCP Client（manager/client/config/error）                     ← Phase 2
src/skills/        SkillManager（parser/discovery/activation）                   ← Phase 4
src/agent/         agent loop 经 ToolRegistry 取工具（保持 execute 类型化）      ← 渐进
```

不重命名现有模块、不移动现有文件（README2 §26：优先适配当前结构）。

---

## 4. 阶段计划

| 阶段 | 内容 | 验收 |
|---|---|---|
| Phase 1 | Tool trait + ToolRegistry + BuiltinToolAdapter | builtin 全走统一接口，现有测试无回归 |
| Phase 2 | MCP V1（stdio/initialize/tools-list/call/adapter/lifecycle/timeout/error）+ 测试 MCP Server | 连接 ≥2 个 MCP Server |
| Phase 3 | MCP UX（/mcp 状态页） | 显示 server/status/tools |
| Phase 4 | Skills V1（parser/discovery/metadata-only/activate_skill/references） | 3 个测试 Skill 闭环 |
| Phase 5 | ToolSelector/ActiveToolSet + progressive disclosure | Context 不随工具/技能数量线性膨胀 |

## 5. 禁止事项（README2 §29）

- 不为 MCP 重写 Agent Loop；
- 不在 Agent Loop 加 MCP 特判（Origin 只是 metadata）；
- 不一次实现整个 MCP Spec；
- 不一次加载所有 MCP Tools + Skills 全文；
- 不重造 Skill 标准；不建 Plugin Framework；不为此大规模重构。

---

*审计基准：master @ bea4ffd（README 全部阶段完成）。*
