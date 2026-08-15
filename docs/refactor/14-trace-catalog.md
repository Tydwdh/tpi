# 14. Trace Catalog：span/event/sensitivity/completeness 目录（O1 / P1-07）

> 状态：O1 catalog 基线（2026-08-14）。本文是**唯一**的 trace name 注册表——
> 新增 span/event 必须先在此登记（`src/trace.rs::CATALOG` + `trace_catalog_is_complete`
> 测试强制）；禁止孤儿 name。
>
> 原则（`12-debug-tracing-and-replay.md` §3.1）：Trace 写失败不能改变业务结果；
> Trace record 不是 SessionEvent；不把 trace sink 行为再次写入同一 trace。

## 1. 身份模型（真实边界注入）

| ID | 类型 | 注入边界 |
|---|---|---|
| `TraceId` | `src/ids.rs::TraceId`（UUIDv7） | 一次 public Agent Run（`agent::run`）——每次 run 新 TraceId |
| `SpanId` | `src/ids.rs::SpanId`（UUIDv7） | 每个 span（当前：`agent.run` span 创建时） |
| `SessionId` | 已有 | session 创建时（durable envelope） |
| `RunId` | 已有 | `session.begin_run()` |
| `RequestId` | 已有 | 每个 model request（= Step 身份，P1-01） |
| `ToolCallId` | 已有 | 每次工具调用 |
| `TurnId/AttemptId/ChildAgentId/RegistrationId` | **未引入** | 无真实混用（P1-01）；TurnId 待 P8 inbox，AttemptId 用 `(RequestId,u32)`，RegistrationId 待 P4-01 |

trace 边界：**一次 Run = 一个 Trace**；follow-up 新 Run = 新 Trace（session ID 关联）；
子代理默认新 Trace + 显式 link（O8）。树形 parent span 只表达嵌套执行；非树关系
（inbox→turn、tool call→invocation、retry→previous、runtime terminal→committed
event）用显式 link（`caused_by_event_id` 只引用 durable event）。

## 2. Span 目录

| name | owner | sensitivity | 字段 | 说明 |
|---|---|---|---|---|
| `agent.run` | `agent::run` | Internal | run_id, trace_id, span_id | 整个 run 的根 span；`Future::instrument`（P0-09 修复跨 await） |

## 3. Event 目录（tracing 宏调用点）

| name（宏第一参数） | owner | sensitivity | 关键字段 |
|---|---|---|---|
| `tpi starting` | `main` | Internal | workspace, model |
| `agent run completed` | `agent::run_inner` | Internal | run_id, reason, turns, tool_calls, tokens, elapsed_ms |
| `run approaching wall-time budget` | `agent::limits` | Internal | — |
| `tool completed` | `agent::tool_runtime` | Internal | call_id, tool, status, duration_ms, exit_code, artifact |
| `compaction failed; not retrying in this band` | `agent::run_inner` | Internal | error |
| `manual compaction: summary invalid` | `agent::run_inner` | Internal | — |
| `manual compaction: not significant` | `agent::run_inner` | Internal | — |
| `provider trace 无法打开日志文件；trace 已禁用` | `provider::trace` | Internal | error, path |
| `provider trace 写入失败` | `provider::trace` | Internal | error |
| `session 第 {} 行损坏: {}` | `session` | Internal | line, error |
| `decode_html_entities: 字节索引越界（内部不变量破坏）` | `tool::web` | Internal | — |

其余 `tracing::warn!/error!` 调用点（约 30 处，含 remote/MCP/process 的 error 转发、
`warn!(server=..)`、`warn!(seq=..)` 等）为 O1 盘点时**未逐条注册**的运维日志：
字段均为 Internal/error 类，无 Secret；O2 落地时随 sink 逐条补登（catalog 测试
届时改为强制全量）。**当前只强制已注册 name 无孤儿**（`is_registered` 正查）。

## 4. Sensitivity 规则

- `Authorization` / API key / credential value / 完整 environment：**永远不能 Plain**（当前代码不记录，见 P0-09 secret canary）。
- Workspace 文件/用户文本：`WorkspaceContent`——O2 前不进 trace（session 是事实源，trace 只记引用/摘要）。
- 新 trace value 构造必须经 `TraceValue`（Plain/Hashed/Redacted），禁止裸 Debug string。

## 5. Completeness

- 正常：`Complete`；sink 降级/溢出：`Lossy` + `Gap` record；crash 缺 close：诊断有效（`Crashed`）。
- 每份 manifest 报告：dropped_records_by_kind、first/last record_seq、missing_payloads（O2 落地）。

## 6. 类型位置

- typed IDs：`src/ids.rs`（`TraceId`/`SpanId`）
- TraceRecord/Sensitivity/TraceValue/Completeness/Catalog：`src/trace.rs`
- 业务模块**不直接依赖 exporter DTO**；tracing 宏照用，字段经本 catalog 登记。
