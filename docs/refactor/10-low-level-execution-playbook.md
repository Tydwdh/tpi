# 10. 低阶模型逐任务执行手册

## 1. 使用方式

本手册用于把 [08-migration-roadmap.md](08-migration-roadmap.md) 中的任务交给上下文和推理能力较弱的模型。它不能替代任务卡，也不能授权模型自行扩大范围。

一次只交一个任务。推荐把以下内容一起提供：

1. 根 `AGENTS.md` 全文；
2. 本手册；
3. 当前任务对应的专题文档；
4. 任务卡列出的具体文件和 tests；
5. 当前 `git status --short`；
6. 上一阶段 evidence/ADR；
7. 明确禁止触碰的用户修改文件。

## 2. 标准任务卡模板

复制并填满，不能留“自行判断”给低阶模型：

```markdown
# Task P?-??: <单一结果>

## 背景
当前表现：
触发序列：
正确行为：
根因证据（文件/符号/测试）：

## 本任务只做
- ...

## 本任务不做
- ...

## 允许修改
- path/file.rs: symbols...
- tests/foo.rs

## 禁止修改
- session wire format / default config / keymap / ...
- 用户已有修改：...

## 前置阅读（必须读完）
- AGENTS.md
- docs/refactor/...
- source file/symbol
- relevant tests

## 先写的失败测试
测试名：
输入/状态序列：
期望：
旧代码为什么失败：

## 实现约束
- owner:
- invariant:
- error/cancel semantics:
- trace span/event + parent/link:
- sensitivity/completeness:
- compatibility facade:
- facade 删除阶段:

## 验证命令
- cargo fmt --all -- --check
- cargo test <exact filter/target>
- cargo check --all-targets --all-features
- cargo clippy ...
- other...

## 完成条件
- ...

## 回滚
- revert this commit; no data migration / restore backup...
```

任务卡没有“触发序列、正确行为、根因”时，不允许开始修改。

## 3. 每个任务的固定流程

### Step 0：保护现场

运行：

```powershell
git status --short
git diff --stat
git diff -- <允许修改的文件>
```

把任何已有修改视为用户资产。若允许文件已有修改：

- 阅读 diff，确认任务修改能否避开相同行；
- 能避开就只 patch 指定区域；
- 会覆盖/语义冲突就停止并报告，不使用 checkout/reset；
- 不格式化整个无关文件。

记录 `git rev-parse HEAD`。不要假定文档中的 2026-08-14 commit 仍是当前基线。

### Step 1：读完整上下文

按任务卡读：入口、定义、调用者、状态 owner、tests。使用 `rg` 搜索符号所有引用：

```powershell
rg -n "SymbolName|related_event|config_field" src tests
```

不要根据文件名猜。写一个不超过 15 行的本地工作笔记：

```text
input -> owner -> state change -> side effect -> durable fact -> runtime/UI feedback
locks/await:
cancel path:
error path:
cleanup owner:
invariants:
```

若不能回答 owner/cancel/error/cleanup，继续读，不改代码。

### Step 2：复现并建立红灯

优先新增最小 test。要求：

- test 从 public/owner boundary 构造可发生状态；
- test 先在旧实现失败，失败原因正是目标 bug；
- 不用 `sleep` 等竞态，使用 barrier/fake clock/channel/explicit fault；
- 不只断言“返回 error”，还断言状态/副作用/session/UI；
- 测试名描述序列与正确结果。

运行最小命令并保存失败摘要。例如：

```powershell
cargo test tool::registry::tests::old_registration_does_not_remove_replacement -- --exact
```

如果旧代码意外通过，说明测试没有暴露问题或假设错了。不要为了让它红而改无关断言；重新定位。

### Step 3：设计最小修复

在修改前逐项回答：

1. 哪个 owner 应维护该 invariant？
2. 能否只改一个数据结构/转换点？
3. normal/error/cancel/shutdown 四条路径如何变化？
4. 是否改变 durable wire、public API、default、keymap、权限？
5. 是否需要兼容 adapter？删除条件是什么？
6. 有没有更小的修复能同样通过 test？

禁止用 catch、refresh、clone、bool、timeout、sleep 掩盖。

### Step 4：用 `apply_patch` 小步修改

每个 patch 一个概念。建议节奏：

```text
type/state change -> compile
owner logic -> targeted test
caller migration -> targeted suite
old path removal -> search + suite
docs/ADR -> full gate
```

不得使用脚本批量重写大文件，除非任务是已审查的机械 rename/move，并且先 dry run。新增 dependency 单独 patch Cargo manifest，再 review lockfile。

### Step 5：逐级验证

先快后慢：

```powershell
cargo fmt --all -- --check
cargo test <module or exact integration target>
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings -D clippy::undocumented_unsafe_blocks
cargo test --all-targets --all-features
cargo test --doc
git diff --check
```

若 workspace 已拆，改为 `--workspace`。若全量 test 因已知环境失败：

- 验证它是否与当前任务无关；
- 报告真实 target/case/error；
- 不逐例 skip 伪造 PASS；
- P0 后 `cygpath` fixture 不应再作为允许失败。

### Step 6：审查 diff

```powershell
git diff --stat
git diff -- <modified files>
rg -n "TODO|FIXME|unwrap\(|expect\(|let _ =|tokio::spawn|std::thread::spawn" <modified files>
```

检查：

- 是否有无关格式化/rename；
- 是否复制了第二份状态/决策；
- 是否在锁内 await；
- 是否丢 Result；
- 是否新增 detached task；
- 是否输出 secret/full prompt/env；
- async span 是否用 instrument 而非把 enter guard 跨 await；
- 新 trace 是否有 typed IDs、sensitivity、completeness 和 queue policy；
- 是否改变默认行为；
- compat 是否标删除阶段；
- test 是否能在修复回退后失败。

### Step 7：交付报告

使用：

```text
Outcome:
Root cause:
Files changed:
Behavior preserved/changed:
Tests added:
Validation:
Compatibility/rollback:
Remaining risk:
User changes preserved:
```

不能把“发现”写成“修复”，也不能只说“所有测试通过”而不列命令。

## 4. 重构专用规则

### 4.1 Mechanical move 与 semantic change 分开

例：拆 `session/mod.rs`：

- PR A：move types/functions，必要 `pub(crate)`，wire/hash/测试零变化；
- PR B：抽 port/adapter，旧 `SessionLog` facade；
- PR C：调用者迁移；
- PR D：删除 facade。

禁止 A 中顺便 rename event、改变 serde tag、优化 IO。

### 4.2 只有一个 writer/side-effect path

shadow migration 可以双读/双投影，不能双执行工具、双写 session、双启动 child。旧路径做决定时，新路径只比较纯输出。

### 4.3 新 abstraction 必须有两个真实角色

建立 trait 前列出当前 providers/consumers。测试 fake 不单独算真实 consumer。如果只有一个内部 caller，优先普通 struct/private closure。

### 4.4 不建立 pass-through 层

如果 A 的每个方法只是同名调用 B 且不拥有 invariant/translation/lifecycle，删除 A 或说明 boundary 价值。不要为了目录图增加 `Manager/Service/Facade`。

### 4.5 错误上下文只加一次

底层返回 typed cause；owner 加 action/resource context；presentation 渲染 help。每层都 `map_err(|e| format!(...))` 会丢类别和重复日志。

## 5. 异步与并发检查清单

修改任何 async code 时逐项填：

```text
Task owner:
Join handle stored in:
Cancellation parent/child:
Channel capacity:
Queue-full behavior:
Sender close point:
Receiver exit condition:
Locks acquired:
Any await while locked:
Error destination:
Panic behavior:
Shutdown ordering:
Leak test:
```

### 5.1 Channel

- terminal/control event 不可 silent drop；
- 高频 delta 可 coalesce/drop，但带 dropped count；
- bounded capacity 是 policy，有 count + bytes；
- channel closed 是 lifecycle signal，不一概记 fatal；
- 不为避免 deadlock 启动永久 drain task，先修 producer/consumer contract。

### 5.2 Lock

- lock scope只包内存 mutation/snapshot；
- clone needed fields 后释放，再 await/IO；
- 明确 lock ordering；
- poison 是 programmer/panic evidence，不随便 unwrap in public path；
- registry snapshot 后 handler execute 不持锁。

### 5.3 Cancellation

- cancel token 每层派生，不能新建孤立 root；
- select 中 cancel branch 清理资源；
- blocking/OS process 要有专门中断机制；
- user cancel、wall budget、shutdown、provider failure 分类不同；
- terminal outcome exactly once。

## 6. Session 修改检查清单

```text
Does wire change? yes/no
Oldest fixture read:
Unknown required event behavior:
Append sequence/lock:
Durability barrier:
Pre-effect ordering:
Incomplete tail:
Middle corruption:
Projection incremental == rebuild:
Migration backup/rollback:
Secret/size limits:
```

任何 serde field/tag/default 改动先更新 compatibility table 和 golden test。禁止运行 formatter/脚本直接重写 golden user session。

## 7. Tool 修改检查清单

```text
Definition and handler source:
Active snapshot identity:
Input size/schema/typed validation:
Policy decision before effect:
Effect class/access footprint:
Scheduler ordering:
Write-ahead requirement:
Cancel before/during/after effect:
Canonical output validation:
Exactly one terminal:
Model/session/UI projection:
Registration disposer identity:
```

新增工具不能在 Agent runtime 加 `if tool.name() == ...`。需要控制流时新增 typed directive 和 owner consumer。

## 8. TUI 修改检查清单

```text
Focus before/after:
Esc/Enter/Tab behavior:
Mouse/keyboard parity:
Follow vs anchored scroll:
Append while history:
Resize:
0/1/narrow/wide layout:
ASCII/CJK/emoji/combining:
Byte/grapheme/cell conversions:
IME hardware cursor:
Paste transaction/undo:
Render cache invalidation:
Manual terminals tested:
```

UI bug不能通过每帧全量 relayout、强制 scroll bottom、强制 refresh 修。

## 9. 追踪与诊断修改检查清单

任何新增 async、IO、工具、provider、session、capability、process 或 child 边界都填写：

```text
Semantic operation name:
Span owner and terminal outcome:
Trace/span parent or link:
Session/run/turn/step/attempt/request/tool/child IDs:
Start/terminal cardinality:
Attributes and cardinality:
Payload sensitivity class:
Standard/Verbose/Forensic behavior:
Redaction/truncation rules:
Completeness/gap behavior:
Queue capacity and overflow:
Sink failure behavior:
Shutdown/flush owner:
Replay/no-effect behavior:
Invariant companion:
Ancestry/cardinality/redaction tests:
```

执行规则：

- 打一个表示“语义操作”的 span，不为每行函数、每个 token 建 span；高频 chunk 用可 coalesce event/计数；
- future 用 `.instrument(span)`/`#[instrument]`，禁止 `let _guard = span.enter()` 活过 `.await`；
- 并发 child/remote work 使用显式 parent 或 link，不能依赖当前线程的 ambient span；
- Standard 只记录 ID、类别、尺寸、digest、duration、outcome；正文进独立 payload，必须 opt-in；
- 不对含 secret 的结构使用 `?value`/`Debug` 整体展开；先转 typed safe fields；
- sink 必须 bounded；drop/coalesce/truncate/redact 都成为可查询 completeness/gap；
- trace 写失败不改变 session seq、tool effect、model request 或 terminal outcome；
- replay/inspector 只读，使用 no-effect adapters；不能为了“复现”再次执行工具；
- lifecycle owner 持有 writer guard/flush deadline；禁止 `Box::leak` 规避所有权；
- 测试 decode record 并断言因果/敏感级别，不能只搜日志字符串。

如果任务无法说清“这个记录属于 Ledger、Trace、Payload、Metrics 哪一层”，先停下重做设计。完整规范见 [12-debug-tracing-and-replay.md](12-debug-tracing-and-replay.md)。

## 10. 新依赖检查清单

```text
Problem evidence:
Candidate official docs:
Version/MSRV/license:
Maintainer/release/security status:
Default features:
Windows/Linux support:
Direct/transitive diff:
Duplicate versions:
Unsafe/FFI/system dependency:
Binary/compile size:
Spike branch and benchmark:
Fallback/removal:
ADR:
```

运行：

```powershell
cargo tree -p <crate>
cargo tree -d
cargo deny check
cargo check --all-targets --all-features
```

若只是试验，结束后要么正式采用，要么从 `Cargo.toml`/lockfile/源码完全删除。

## 11. 首批任务的逐步示例

### 11.1 P0-02：修 `cygpath` fixture

1. `git status`，确认 `tests/fixtures/remote_server.rs` 是否干净。
2. 读该文件上下至少 100 行及所有 helper 调用。
3. `rg -n "cygpath|remote_server" tests` 找全部消费者。
4. 单独运行 `cargo test --test remote_bash`，保存 error。
5. 写一个针对 path conversion/helper setup 的 Windows test，无 `cygpath` 环境也可运行。
6. 确定 remote fake server 需要的是 MSYS path、SFTP path 还是 display path；不能仅替换反斜杠。
7. 用 typed Windows path conversion/fixture-owned temp root，或显式 locate required shell；选能表达真实协议者。
8. 只 patch fixture/helper。
9. 运行 `remote_bash`、`agent_remote`、所有 `remote_*` targets，再 full test。
10. 报告有/无 Git Bash 行为；不要把 skip 当 PASS。

### 11.2 P4-01：RegistrationId/ABA

1. 读 `ToolRegistry`、`ToolRegistration`、MCP manager registrations 和 tests。
2. 新增 exact ABA test：A register、B explicit replace、drop A、B remains。
3. 运行 test 见红。
4. 新建 opaque `RegistrationId`，registry entry 存 `{id, tool}`。
5. `ToolRegistration` 存 name+id；unregister_if_matches。
6. 保持 `get/list/descriptors` 对外结果不变。
7. 明确 duplicate/replace API；若本任务只修 ABA，不顺便实现 scope。
8. 测 double unregister、drop after registry replacement、不同 name。
9. 跑 MCP/tool/agent tests；用 `rg global_registry` 确认未增加调用。
10. diff review：没有在 Drop panic；poison 策略按 owner处理。

### 11.3 P1-04：消除 TUI -> app

1. 读 `preview_lines_to_body` 定义、全部调用和 tests。
2. 确认它是纯 presentation projection，无 file/session IO。
3. 给当前输入输出加/确认 characterization tests。
4. 选择 owner：若只供 session preview view，移动到 `tui::projection/session_preview.rs`；若 app/headless 共用，放 app-view/projection 的低层模块但 TUI 只依赖该 port/type。
5. 机械移动函数和 tests；不改文案/截断算法。
6. `rg "crate::app" src/tui` 必须为空。
7. 更新 dependency gate，删除 exception。

### 11.4 P2-05：Supervisor walking skeleton

1. 不先迁全部 `tokio::spawn`。
2. 定义 owner 内部 `TaskTracker + CancellationToken + accepting bool/state`。
3. 建一个测试 task 等 token cancel 后设置 terminal flag。
4. `shutdown`: stop accepting -> cancel -> tracker.close -> wait。
5. 测 shutdown 两次、shutdown 与 spawn race、task panic/error。
6. 确认 no lock across wait。
7. 通过后只迁一个现有 background task。

### 11.5 P6-03：Semantic scroll anchor

1. 读取所有 scroll model/reducer/render/property tests。
2. 记录 old offset state 对 fixtures 的屏幕顶行/底行。
3. 新 `ScrollAnchor` 在旁路计算，不立即替换渲染决定。
4. 对 append/resize/fold/stream 比较 old/new；先解释分歧。
5. 写用户语义期望：history anchor 保持，bottom follow。
6. 一个 action 一个 reducer transition。
7. 切新模型，保留临时 adapter；全 scroll/interaction snapshots。
8. 人工 wheel/PageUp/End/stream/resize；删除 adapter 的阶段明确。

### 11.6 P8-04：只读 child

1. 前置 gate：Supervisor、scoped capability、independent session、inbox、structured report 均已完成；缺一不得开始。
2. fake provider 先实现 conformance；不接真实模型。
3. `ChildSpec` 固定 depth=1、concurrency=1、SharedReadOnly。
4. child tool snapshot只含 read-only allowlist，runtime policy 再校验 effect。
5. child 使用独立 session ID/Artifact namespace。
6. parent cancel token 派生；start fault逐点 rollback。
7. terminal report 有 byte/schema cap；parent context只收 summary/ref。
8. 100 次 start/cancel leak test。
9. 再接一个真实 model adapter；默认 config off。
10. 最后做 UI summary card，不能把 UI 作为 runtime 正确性条件。

### 11.7 P0-09：修正 async trace ancestry

1. 读 `src/main.rs::init_logging`、`src/agent/mod.rs` 中 `agent.run` span、全部 `.enter()` 和 provider trace。
2. 不先改 writer。写 capture subscriber test：run span 内创建一个 await 前 event、await 后 child span/event；旧代码的实际 parent chain 应暴露错误。
3. 明确 `agent.run` 的 owner/terminal outcome；为 future 使用 `.instrument(run_span)`，同步小段才使用 `in_scope`。
4. 搜索仓库所有 `entered()/enter()`，逐个判断 guard 是否可能跨 await；本任务只修确认有问题的位置。
5. 对并发 tool/provider task 显式 clone/传递 parent span 或 typed context；不要假设 spawn 自动继承正确 parent。
6. 测 success/error/cancel 三个 terminal，断言 run/step/request/tool IDs 和 parent/link，不断言格式化字符串。
7. 运行 agent/provider focused suites 和 clippy；比较关闭/开启 subscriber 的 session/output parity。
8. 记录 provider 同步 flush 为后续 P2-08 风险，不在本 PR 同时重写存储和 trace schema。

### 11.8 P2-08：bounded local trace sink

1. 前置：O1 `TraceRecord`/catalog 已批准；没有 schema 时不从 writer 开始。
2. 用 fake slow writer、short write、disk full、panic-free close fixtures 建红灯。
3. application owner 持有 queue sender、worker handle/guard、cancel token 和 flush deadline。
4. 分别配置 record count/byte 上限；定义 Standard 的 drop/coalesce/terminal reserve 行为。
5. queue overflow 累计 dropped count，并在恢复写入时产生 `TraceGap`；不能在满队列上递归记录 queue-full log。
6. rotation 后 segment header 保留 schema/process/start seq；截断尾部 scanner 标 incomplete。
7. sink error 进入 health snapshot/最后可用 stderr，但不得让 agent/session/tool 失败或死锁。
8. 100 次 start/shutdown、10 分钟 burst、disabled parity、secret canary；给出 p95/p99 开销。

## 12. 失败处理决策树

```text
Test fails
  |
  +-- new focused test fails as expected? -> implement minimal fix
  |
  +-- unrelated existing test fails?
  |      +-- reproduce on baseline -> record pre-existing, do not hide
  |      \-- only head fails -> locate interaction, task not done
  |
  +-- flaky/timing?
  |      +-- replace time with barrier/fake clock
  |      \-- external prereq -> explicit capability/setup
  |
  +-- architecture requires broader change?
         +-- can add narrow adapter within task -> do + deletion condition
         \-- materially changes scope -> stop, split/re-plan
```

编译错误不是授权大范围改 public。先看是否错误边界/visibility 选错。

## 13. Code review 问题清单

Reviewer 不问“看起来更优雅吗”，而问：

1. 这个 diff 消除了哪个可复现 bug/复杂度？
2. invariant 的唯一 owner 现在是谁？
3. 有没有新增非法状态组合？
4. error/cancel/shutdown 也到 terminal 吗？
5. effect 前后的 durable 顺序正确吗？
6. 新接口有当前 provider/consumer 吗？
7. prompt-visible、executable、durable 的 tool/message 是否同源？
8. Unicode/path/Windows/remote 边界如何？
9. 用户焦点/滚动/反馈如何？
10. 性能/内存是否有数据？
11. compatibility 何时删除？
12. revert 后测试是否会红？

## 14. 模型必须停止并请求人类决策的情况

- 要改变 session wire、旧文件兼容窗口或自动迁移；
- 要改变 `allow_outside_workspace`、权限默认、删除确认；
- 要选择 ACP client vs server、SQLite vs no index、Wasm public ABI；
- 要覆盖当前用户修改或删除不理解的数据；
- 测试需要真实 credential/外部付费调用；
- 子代理要写共享 workspace 或自动合并；
- 新依赖有不兼容许可证/MSRV/系统库；
- 发现当前任务根因与计划不符且需要跨三个以上 owner；
- 无法用合理 fixture 定义正确用户行为。

停止时报告：已知事实、已尝试、两个以上方案、各自风险和需要的具体选择。不要只说“需要更多信息”。

## 15. 完成阶段而非单任务的检查表

```text
[ ] phase entry conditions were met
[ ] all task cards have evidence links
[ ] full Windows/Linux gates pass
[ ] session golden/crash tests pass
[ ] architecture exceptions reduced, not increased
[ ] no user dirty changes overwritten
[ ] no unowned spawn/process/registration
[ ] dependency/license/MSRV review complete
[ ] performance/resource comparison complete
[ ] manual UX matrix complete if applicable
[ ] compatibility inventory updated
[ ] rollback rehearsed
[ ] ADRs reflect actual implementation
[ ] docs/examples/help match behavior
[ ] phase evidence index published
```

## 16. 最终交付报告模板

```markdown
## 1. 已发现问题

### High / Medium / Low / Ergonomics
位置：
问题：
触发条件：
根因：
影响：
修复：
验证：

## 2. 已修改内容
- 仅列真正落地的代码、测试、文档、依赖。

## 3. 验证结果
Build: PASS/FAIL（命令）
Tests: PASS/FAIL（数量、失败 target/cause）
Lint: PASS/FAIL
Architecture gate: PASS/FAIL
Coverage/Mutation/Benchmark: result or not run with reason
Manual verification: PASS/未执行（矩阵）

## 4. 兼容与回滚
session/config/API/default changes：
compat layer/delete phase：
rollback steps：

## 5. 剩余风险
- 未验证、设计债务、人工项、故意未改项。
```

低阶模型只有在所有完成条件有证据时才能写“完成”；接近 token/time 限额不是完成理由。
