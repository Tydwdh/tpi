# ADR-001：极小内核与依赖 DAG

- 状态：**Approved（P0-08，2026-08-14）**
- 关联：`docs/refactor/02-target-architecture.md` §1、`docs/refactor/08-migration-roadmap.md` P0-08

## Context

TPI 是单 crate（约 48k 行 src）。`src/app.rs`（2,910 行）同时是 composition root、
terminal input adapter、use case 与 presentation controller；`agent` 直接引用 `tui`
类型（ViewMode/Keymap），`tui::reducer` 反向引用 `crate::app::preview_lines_to_body`
（审计 Medium-1/High-1）。没有依赖方向约束时，任何跨层改动都可能破坏无关时序，
且未来物理拆 crate 必然因循环依赖失败。

目标不是"文件小"，而是**每个模块只依赖它实际需要的稳定小核心**：AgentLoop / Session /
Context / ToolExecutor / ToolRegistry 是核心；MCP / Skills / LSP / Web / 子代理是扩展。

## Decision

采用**模块化单体 + 极小微内核 + ports/adapters + 显式 composition root**，分两步：

1. **逻辑边界**（P1–P6，单 crate 内）：建立并强制以下依赖方向，禁止反向引用。
2. **物理边界**（P7，仅当连续两个阶段无反向 import 回流）：拆 Cargo workspace。

允许的依赖方向（`A -> B` 表示 A 依赖 B）：

```text
core（IDs/消息/状态/命令/live event/错误分类）——不依赖 session IO、capability 实现、
    provider、TUI、platform
session -> core
capabilities（tool/provider/workspace/subagent contract、registry、policy）-> core
agent -> core + session port + capability ports（不依赖 Ratatui/Reqwest/Russh/MCP SDK）
adapters -> core + session + capabilities（允许外部 IO 库）
tui -> core 的 command/event DTO + 自己的 view model（不依赖 agent 实现/session file）
cli/app（composition root）-> 全部
```

不变量：**core 不得 import 任何 IO/UI/provider 类型**；`tui` 不引用 `app`；
`agent` 不引用 `tui`；注册表不是进程级 service locator（P4 起 composition root 注入）。

## Alternatives

- **一切皆动态插件（Everything-is-Plugin）**：拒绝。AgentLoop/Session/Context 是
  稳定核心，不是插件容器；TPI 用 trait/Arc/registry/RAII/显式生命周期满足需求
  （`docs/architecture.md` §不吸收）。
- **维持现状（无规则单 crate）**：拒绝。已实测存在 `tui→app`、`agent→tui` 反向引用
  （P0-07 登记 16 处），拆 crate 或加新能力时必然再次跨越错误边界。
- **直接拆七个 crate**：拒绝（文档 §4.2）。直接拆产生大量 pub/feature/循环依赖噪音；
  先在单 crate 内用模块边界 + 静态 gate 证明依赖方向。

## Consequences

正面：
- 每次修改的影响面受依赖方向约束；低阶模型只需建立局部上下文。
- P7 拆 crate 是机械移动而非架构赌博。
- TUI/headless/测试可共享同一 semantic event，不再被迫消费 TUI 表示。

负面/成本：
- 前期需要容忍"逻辑边界在单 crate 内、物理边界未生效"的过渡期。
- 边界移动涉及 re-export/兼容层，需逐个删除（文档 §4.3：兼容层必须有删除期限）。

## Migration

1. P0-07 已建立 `scripts/arch_gate.sh`（R1 `tui→app`、R2 `agent→tui`、R3 新增
   `global_registry()` 均拒绝，精确 allowlist 只减不增，已接入 CI）。
2. P1：拆 live runtime 与 view event；domain message 与 provider ChatMessage 并存
   （双写 adapter）；消除 `tui→app`（P1-04）。
3. P1 Exit gate：`agent→tui` 引用清零、`tui→app` 清零。
4. P4：composition root 注入 registry，删除 `global_registry()`。
5. P7：`cargo metadata` 脚本 dry-run 模拟 crate edges，无反向 import 才物理拆。

## Rollback

- 单阶段回滚：revert 该阶段 PR（模块边界移动可逆，不做数据迁移）。
- 架构 gate 回滚：删除 `arch_gate.sh` 步骤即可（但会失去防护，不推荐）。
- 物理拆 crate 前（P7）全部回滚点均为 revert commit；拆 crate 后回滚 = 恢复
  `workspace` 配置与 re-export，仍有删除期限。

## Evidence

- `scripts/arch_gate.sh` + CI 步骤（P0-07 已合并，`a40fcf5`）。
- 审计基线：`docs/refactor/00-current-state-audit.md` §3 依赖压力表
  （`app→tui` 62 refs、`tui→tool` 59、`agent→tool` 47）。
- 违规登记：R1 1 处（`src/tui/reducer.rs:44`）、R2 6 处、R3 9 处（P0-07 时点）。

## 非目标

- 不引入运行时依赖注入框架 / 全局 service locator。
- 不为"文件大小"拆分高内聚模块（`edit.rs` 2,584 行保持单文件直到有实际问题）。
- 不要求默认配置启用全部扩展能力（P4.6 默认克制）。
- 本 ADR 不决策工具 pipeline 内部阶段（见 ADR-005）与 TUI 组件契约（见 ADR-008）。
