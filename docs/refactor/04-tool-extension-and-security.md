# 04. 工具系统、扩展性与安全边界

## 1. 目标

工具系统必须同时满足：

- builtin、MCP、未来 Wasm/remote tool 使用同一 resolve/validate/authorize/schedule/result pipeline；
- 每个来源仍可以有不同 transport 和 trust level；“统一”不等于抹平安全差异；
- 模型看到的 definition 与实际执行 handler 来自同一 immutable snapshot；
- crash-sensitive effect 在副作用前写 durable recovery；
- tool result 同时能服务模型、session、UI 和诊断，而不让任何一个 consumer 支配 contract；
- register/override/dispose 精确、有作用域、可测试；
- runtime 不靠工具名称发现语义。

## 2. 目标对象模型

以下伪 Rust 表达职责，实际实现可因 object safety 调整。

```rust
struct ToolDefinition {
    id: ToolId,                    // 稳定内部 ID，不等于显示名
    name: ToolName,
    description: String,
    input_schema: JsonSchema,
    output_schema: Option<JsonSchema>,
    execution: ExecutionPolicy,
    effect: EffectClass,
    exposure: ExposurePolicy,
    presentation: PresentationHint,
    origin: CapabilityOrigin,
}

trait ToolHandler: Send + Sync {
    async fn invoke(
        &self,
        input: CanonicalToolInput,
        ctx: ToolInvocationContext,
    ) -> Result<CanonicalToolOutput, ToolInvokeError>;
}

struct ToolEntry {
    definition: Arc<ToolDefinition>,
    handler: Arc<dyn ToolHandler>,
    registration_id: RegistrationId,
}
```

### 2.1 名称、ID、registration identity

- `ToolId`：能力的逻辑稳定身份，可含 namespace，例如 `builtin:edit@1`。
- `ToolName`：发给模型的当前名称，在一个 capability snapshot 内唯一。
- `RegistrationId`：每次注册生成的新 identity，只用于 ownership/disposal，永不复用。
- `ToolCallId`：每次调用 identity，已有强类型应保留。

禁止用 `String` 在这些语义间互换。

## 3. Registry 与 scope

### 3.1 数据结构

```text
RootRegistry (application-owned, mostly immutable)
  +-- SessionOverlay (session-owned)
        +-- AgentOverlay (agent-owned)
```

lookup 从最具体 scope 向 root 查找。规则：

1. 同一 scope 重复名称默认返回 `DuplicateRegistration`；
2. override 必须显式 `register_override(expected_current_id, entry)`；
3. disposer 只删除 `(scope, name, registration_id)` 全匹配 entry；
4. snapshot 在 Step 开始时冻结，Step 中途 reload 不改变当前模型 definition/handler；
5. policy 在 snapshot 上过滤，不能只在 prompt 隐藏后仍允许按名执行；
6. registry 锁只保护小型 map mutation/snapshot，不在锁内 await/执行 handler；
7. list 顺序稳定，保证 prompt/cache/snapshot 可重复。

### 3.2 Setup transaction

```text
begin AgentBuild
  create private overlay
  install registrations, collect handles
  start required providers, collect owned resources
  resolve mandatory capabilities
  validate definitions/schemas/conflicts
  create supervisor/inbox/session binding
publish AgentHandle only if all succeeded
on any error: close -> cancel -> await -> dispose handles in reverse order
```

必须测试每一个 setup fault point，证明没有半注册 tool、子进程或后台 task。

### 3.3 当前 ABA bug 的首个红灯测试

```rust
let a = registry.register_owned("same", handler_a);
let b = registry.register_override("same", handler_b, expected = a.id());
drop(a);
assert_eq!(registry.resolve("same").registration_id, b.id());
```

修复 PR 只改 identity/disposal，不同时实现 scope/pipeline。然后再单独增加 overlay。

## 4. 单一工具解析路径

同一个 `ActiveToolSet` 必须同时供三个 consumer 使用：

1. context/prompt：选择哪些 schema 发给模型；
2. executor：模型按 name 调用时找到相同 entry；
3. UI/doctor：展示来源、权限、冲突和不可用原因。

```text
registry snapshot
  -> policy filter
  -> context selector/tool budget
  -> ActiveToolSet { ordered definitions + exact handlers }
       |                             |
       +----> ModelRequest.tools     +----> execute by name
```

禁止 prompt 重新从 global registry 列表，而 executor 使用另一 map。

## 5. 显式 pipeline

### 5.1 阶段

```text
0 Resolve
1 Parse JSON / size limit
2 Validate schema and typed builtin args
3 Authorize policy / monotonic guards
4 Analyze effect and access footprint
5 Plan recovery / acquire scheduler permits
6 Persist ToolRequested/Started as required
7 Execute with cancellation and bounded progress
8 Normalize/validate canonical output
9 Apply declared control directives/effect reports
10 Persist ToolCompleted and sync required barrier
11 Publish terminal RuntimeEvent/View projection
12 Release permits/cleanup backup/temp
```

阶段 6、7、10 的顺序是 crash contract，不允许 generic middleware 重新排序。post-process 即使失败，也必须为工具调用形成一个 terminal outcome；不能让模型协议悬空。

### 5.2 ExecutionPolicy

```rust
enum ConcurrencyClass {
    Parallel,
    ExclusiveWorkspace,
    ExclusiveSession,
    Custom(ResourceSet),
}

enum EffectClass {
    Pure,
    ReadOnly,
    ReversibleWorkspaceWrite,
    IrreversibleExternal,
    ProcessLifecycle,
    Control,
    UnknownExternal,
}
```

这是 policy 输入，不是对安全的自我声明。外部工具来源声明的 effect class 要经过 trust policy 降权：未知 MCP tool 不能自行声称 Pure 后绕过批准。

### 5.3 Access footprint

使用 canonical paths/resource keys 做冲突判断：

- read/read 可并行；
- write 与相交 read/write 冲突；
- shell unknown workspace effect 默认 exclusive 或 policy 指定；
- process status/read tail 可并行，start/cancel 对同一 process key 串行；
- remote/local path canonicalization 必须由 workspace adapter 完成。

不要一开始追求完美静态分析。工具在 validate/plan 阶段返回保守 footprint；未知即串行。以后用真实 profile 证明过度串行再优化。

## 6. Canonical input/output

### 6.1 输入

`CanonicalToolInput` 至少含：validated JSON value、原始有界 JSON、tool/call ID、schema version。builtin 可 deserialize 为具体 Args；外部边界用编译过的 JSON Schema validator。

### 6.2 输出

```rust
struct CanonicalToolOutput {
    status: ToolTerminalStatus,
    content: Vec<ToolContentBlock>,
    value: Option<serde_json::Value>,
    diagnostics: Vec<ToolDiagnostic>,
    artifacts: Vec<ArtifactRef>,
    effect_report: EffectReport,
    directives: Vec<ControlDirective>,
    presentation: Option<ToolPresentation>,
    metrics: ToolMetrics,
}
```

- `content/value` 是模型/其他程序可消费的业务结果；
- `diagnostics` 是有界错误/警告，不拼进成功正文假装结果；
- `artifacts` 指向 durable/bounded storage，不内嵌巨大文本；
- `effect_report` 告诉 runtime 实际读写/进程变化，供 progress/recovery/observability；
- `directives` 只能来自获准的 control capability；
- `presentation` 是 hint，不包含 Ratatui style/line；UI 可忽略；
- `metrics` 包含 duration、bytes、truncation、exit code 等。

模型 payload 是这个 canonical output 的投影，不再是权威存储。

### 6.3 删除工具名协议

当前特殊行为迁移表：

| 当前 name 判断 | 目标 owner |
|---|---|
| `update_plan` | Plan capability consumer 解析 `ControlDirective::ReplacePlan`，session projector 写 `PlanReplaced` |
| `request_input` | Interaction capability 返回 `ControlDirective::SuspendForInput`; Agent state machine 执行 |
| `edit/write` bump epoch | `EffectReport::WorkspaceChanged { revisions }`；progress tracker 消费 |
| `web_fetch` summarize | Web capability 自己的 result finalizer，或 context projection 的 content policy；不在 executor 匹配名称 |

每迁移一个特殊名：先加等价 typed output 特征测试；短期双读只允许在一个 adapter 内，并设删除阶段。

## 7. Write-ahead 和恢复

### 7.1 不可退化的不变量

对可恢复 workspace write：

1. validate/canonicalize target；
2. 计算 expected/candidate revision；
3. 在同一目标 workspace/目录准备 temp/backup；
4. durable append recovery plan 并按 barrier sync；
5. 执行 commit；
6. 验证 target/backup/temp 状态；
7. append terminal outcome 并 sync；
8. 最后清理 backup/temp。

registry/pipeline 重构必须复用现有 `recovery_matrix`，不得让 external 工具自动获得“可恢复”标签。只有实现 `RecoverableEffectProvider` 并通过 crash suite 的 handler 才可使用该 class。

### 7.2 Irreversible external effect

网络写、发送消息、付款等通常不可回滚。策略必须在执行前记录 intent/idempotency key（若协议支持），完成后记录 receipt。重启时不能盲目重试 `Unknown` 结果；展示“可能已发生，需要核验”。

## 8. 权限与安全策略

### 8.1 Policy input

```text
actor: user/parent-agent/child-agent/external-agent
scope: workspace/session/agent
tool origin + signed/trusted status
effect class + resource footprint
resolved target/network host/process command
interaction mode: interactive/headless
current trust profile and prior grant
```

### 8.2 Policy decision

```rust
enum PolicyDecision {
    Allow { constraints: RuntimeConstraints },
    Deny { code: PolicyCode, reason: String },
    RequireApproval { request: ApprovalRequest },
}
```

approval 是明确 state/lifecycle，不得 blocking read stdin。headless 中 `RequireApproval` 默认 fail closed，除非调用方提供 approval port。

### 8.3 最低安全控制

- workspace 外路径必须在 canonicalization 后检查，处理 symlink/reparse point/UNC；
- web fetch 每次 redirect 都重新 SSRF 检查；DNS rebinding 策略明确；
- subprocess environment 采用 allowlist/overlay，secret 不进入 session/trace；
- output/arguments/schema/message 都有 byte/depth/count 上限；
- MCP server tool name/description/schema 是不可信输入；
- child agent 的 tool set 是 parent policy 的子集，不能自行升级；
- 外部 agent full-access 时 UI 明确标记“TPI 无法强制其内部工具权限”；
- Wasm 只 preopen 获准目录，默认无 network，无 host process spawn；
- 插件安装/配置和运行时调用分开授权。

## 9. MCP 生命周期

```text
Configured -> Starting -> Negotiating -> Active -> Stopping -> Stopped
                       \-> Failed       \-> Failed/Restarting
```

每个 MCP connection owner 持有：child process/transport、reader/writer tasks、server capability snapshot、tool registrations、cancel token、restart state。

重启顺序：

1. 新连接在 private setup scope 完成 handshake/list/validate；
2. 原 Active snapshot 仍可服务已开始的 Step；
3. 原子发布新 generation；
4. 旧 generation 停止接收新调用；
5. 允许进行中调用在预算内完成或 cancel；
6. drop 精确旧 registrations；
7. 等待旧 tasks/process 退出。

若新 setup 失败，旧 generation 不受影响。禁止先清空旧工具再尝试启动新 server。

## 10. Tool conformance suite

每种来源至少运行：

- definition name/schema/size validation；
- invalid JSON、schema mismatch、unknown fields；
- cancel before start、during wait、during IO、after effect；
- timeout 与 user cancel 分类；
- progress flood 背压；
- success/failure/rejected/cancelled 恰好一个 terminal；
- output size/depth/Unicode/truncation；
- parallel completion 乱序但 result source-order；
- policy deny 无 handler side effect；
- handler panic/transport close 被隔离；
- registry reload 时当前 snapshot 稳定；
- disposer idempotent/ABA safe；
- crash recovery（仅声明 recoverable 的来源）；
- model/session/UI 三个 projection 一致。

Builtin、MCP fake server 和至少一个 fake third-party adapter 必须用同一 suite；来源专有测试另加。

## 11. 分步实现顺序

1. 给当前 registry 加 `RegistrationId`，红灯复现 ABA，最小修复。
2. 移除 `global_registry()` 的新调用；composition root 显式注入现有 registry。
3. 建 immutable `ActiveToolSet`，prompt/lookup 共用。
4. 把 definition 与 handler 分开，但保持旧 `BuiltinToolAdapter` facade。
5. 引入 input/output canonical types；从一个无副作用工具开始。
6. 建显式 pipeline skeleton，旧 executor 作为 handler adapter。
7. 一次迁移一个特殊名协议：plan、input、workspace changed、web finalizer。
8. 把 builtin/external 分支压到 adapter 内，scheduler 只看 definition metadata。
9. 引入 scope overlay 和 setup transaction。
10. MCP generation reload；最后删除旧 facade/global registry。

每一步独立 build/test/benchmark；不能在一个 PR 同时做 1–10。
