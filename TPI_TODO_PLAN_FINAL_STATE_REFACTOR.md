# TPI Todo / Plan Final-State Refactor Specification

> Target repository: `Tydwdh/tpi`
>
> Scope: Todo / Plan subsystem only.
>
> This is a **final-state execution specification** for a coding agent.
>
> Do not implement a compatibility layer or transitional design.
> Do not preserve the current "inject current plan into every model request as a system message" mechanism.
> The final implementation must converge directly to the architecture defined below.

---

# 1. Objective

Refactor TPI's Todo / Plan subsystem so that:

- Todo/Plan is independent structured session state;
- the model updates it through `update_plan`;
- the UI reads it from session/runtime projection;
- the model does **not** receive the current plan as a synthetic system message on every inference;
- model working memory comes from its own `update_plan` tool-call history;
- compaction preserves necessary unfinished-plan state when old tool calls are compressed away;
- a new user turn starts a new execution-plan lifecycle;
- old plan state cannot override or bias a new user instruction;
- no duplicate authoritative copies of the plan exist.

Core invariant:

```text
Todo/Plan is session state, not a synthetic conversation message.
```

---

# 2. Final architecture

Target data flow:

```text
                           Agent
                             |
                             | update_plan(full snapshot)
                             v
                      +--------------+
                      | ToolExecutor |
                      +------+-------+
                             |
                 +-----------+-----------+
                 |                       |
                 v                       v
         tool call / result        Session Event
           model history          PlanUpdated(...)
                 |                       |
                 v                       v
              Model                Session Projection
                                         |
                              +----------+----------+
                              |                     |
                              v                     v
                             UI                  Resume
```

There must be **no** path:

```text
PlanUpdated
    |
    v
build_context()
    |
    v
System("CURRENT PLAN ...")
```

Delete that path completely.

---

# 3. Source of truth

Todo/Plan authoritative state must live in the session/event model.

Use one durable event such as:

```rust
SessionEvent::PlanUpdated {
    items: Vec<PlanItem>,
}
```

or the repository's equivalent final event type.

The event contains the complete current plan snapshot.

Plan updates use **full-list replacement**, not incremental mutation commands.

Conceptually:

```rust
struct PlanItem {
    content: String,
    status: PlanStatus,
}

enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}
```

If TPI currently supports additional meaningful plan metadata, preserve it only if it has real product value.

Do not preserve fields merely because they exist today.

---

# 4. `update_plan` final contract

`update_plan` remains the model-facing mechanism for changing plan state.

Final conceptual schema:

```json
{
  "plan": [
    {
      "step": "Inspect current edit implementation",
      "status": "completed"
    },
    {
      "step": "Remove model-visible revisions",
      "status": "in_progress"
    },
    {
      "step": "Run contract tests",
      "status": "pending"
    }
  ]
}
```

Optional explanation may remain only if it is currently useful to the UI/model interaction.

The important semantics are:

```text
update_plan(new_plan)
    ->
replace current active plan snapshot
```

Do not support:

```text
append one item
patch item by index
implicit merge
legacy todo format
multiple equivalent schemas
```

unless a strong product reason exists.

One model-visible operation should produce one canonical complete plan state.

---

# 5. Model message surface

## 5.1 Remove current-plan system injection

Delete all logic that converts the current plan projection into a model-visible `System` message during ordinary context construction.

The final request must not contain synthetic messages such as:

```text
system:
[当前计划·唯一权威·完整快照·以此为准]
Phase 1 ...
Phase 2 ...
Phase 4 in_progress
```

Do not move this block to an earlier system message.

Do not convert it to a developer message.

Do not convert it to a user message.

Do not rename it to:

```text
Harness State
Runtime State
Current Todo
Current Plan
Execution State
```

and continue injecting it every turn.

The mechanism itself must be removed.

## 5.2 Model working memory

Within the same turn, the model already sees its own tool call:

```text
assistant:
    update_plan({...full plan...})

tool:
    Plan updated
```

That tool-call history is the model's normal working-memory representation.

Do not duplicate the same information in another message.

## 5.3 Tool result

The `update_plan` tool result should be short and stable.

Preferred:

```text
Plan updated.
```

or a compact summary:

```text
Plan updated: 2 pending, 1 in progress, 3 completed.
```

Do not echo the full plan in the tool result if the full plan is already present in the tool-call arguments.

This avoids token duplication.

---

# 6. Session projection and UI

Todo/Plan must remain available to the product UI independently of model messages.

Session replay should derive:

```text
CurrentPlanProjection
```

from the latest active `PlanUpdated` event in the relevant lifecycle.

UI should consume this projection.

The model message derivation path must ignore PlanUpdated state events.

Conceptually:

```text
derive_session_projection(events)
    includes PlanUpdated

derive_model_messages(events)
    excludes PlanUpdated
```

This separation is mandatory.

Plan state is durable application state.

It is not itself a conversation message.

---

# 7. Turn lifecycle

Treat Todo/Plan as the execution plan for the current user turn.

Final lifecycle:

```text
User Turn A starts
    |
    v
active plan = none
    |
    v
model may call update_plan(...)
    |
    v
plan visible in UI
    |
    v
model works
    |
    v
turn finishes
    |
    v
completed/latest plan may remain visible to user

User Turn B starts
    |
    v
old active plan is cleared / archived from active projection
    |
    v
new turn begins with no active plan
```

A previous turn's unfinished plan must not be automatically injected or treated as authoritative execution instructions for a new user message.

This prevents stale-plan takeover such as:

```text
previous plan:
Phase 4 in_progress

new user:
"Stop that. Check another bug."

model:
"I should continue Phase 4."
```

That behavior must become structurally impossible from plan injection.

---

# 8. Turn-boundary event semantics

At new user turn start, explicitly clear the **active plan projection**.

Preferred event model:

```rust
SessionEvent::TurnStarted { ... }
```

Projection rule:

```text
TurnStarted
    ->
active_plan = None
```

Alternative:

```rust
SessionEvent::PlanCleared
```

may be used only if it produces a cleaner event model.

Prefer lifecycle-derived clearing over redundant events if `TurnStarted` already exists and is authoritative.

Do not delete old PlanUpdated events from history.

They remain useful for:

- replay;
- debugging;
- UI history;
- evaluation;
- analytics.

Only the active projection resets.

---

# 9. Compaction

Removing system reinjection introduces one legitimate concern:

```text
old update_plan tool call
    |
    v
large context
    |
    v
compaction
    |
    v
tool call may disappear from active model context
```

Solve this only at compaction boundaries.

Do not solve it by injecting the plan on every request.

## 9.1 Final compaction rule

When generating a compaction summary:

- inspect current active plan projection;
- if no active plan exists, do nothing;
- if an active plan exists and contains unfinished items, preserve only the minimum execution state needed to continue;
- completed-only plans do not need to be carried forward unless context requires them.

Example compacted state:

```text
Execution state:
- Completed: inspect edit implementation
- In progress: remove model-visible revisions
- Pending: run contract tests
```

This state becomes part of the **compaction summary** because compaction is already replacing old conversation context.

It must not become a new recurring system-state injection mechanism.

## 9.2 After compaction

The summary acts as the bridge until the model next updates the plan.

Once the model calls `update_plan` again, normal tool-call history becomes current working state.

---

# 10. Resume behavior

On session resume:

- reconstruct session state from events;
- reconstruct active plan projection according to turn lifecycle;
- restore UI projection;
- do not synthesize a current-plan system message.

If the resumed model context still contains the relevant `update_plan` tool history or compacted summary, nothing else is needed.

If the session resumes at a point where active execution context cannot be reconstructed safely, prefer:

```text
no active model plan
```

over silently injecting stale authoritative state.

Session/UI history may still show the historical plan.

---

# 11. Branch behavior

If TPI supports branching/forking conversation state, plan projection must follow branch history.

The branch's current plan is derived from events visible on that branch.

Do not use global mutable plan state detached from branch/session event history.

Required invariant:

```text
branch A plan != branch B plan
unless both histories actually contain the same latest plan event
```

Branch rewind must automatically rewind plan projection.

---

# 12. Tool-call/message derivation

Audit the complete event-to-message path.

Final rule:

```text
Conversation events:
    user message
    assistant message
    tool call
    tool result
    compaction summary
        -> model surface

Application state events:
    PlanUpdated
    runtime projections
    UI-only status
        -> NOT model surface
```

Do not identify model-surface events merely by "everything in the session event log".

Create an explicit allowlist / typed distinction.

For example:

```rust
enum EventVisibility {
    ModelSurface,
    ApplicationState,
}
```

or encode the separation in event types/modules.

The exact mechanism is flexible.

The semantic separation is mandatory.

---

# 13. Remove plan injection from `build_context`

Audit the current context builder.

Delete code equivalent to:

```rust
if let Some(plan) = current_plan {
    out.push(ChatMessage::System(format!(
        "[当前计划·唯一权威·完整快照·以此为准]\n{plan}"
    )));
}
```

Also delete:

- formatting helpers only used by that injection;
- tests asserting tail system plan injection;
- comments defending tail injection for prefix caching;
- provider-specific workarounds for mid/tail system messages;
- plan-message deduplication logic;
- "current plan is authoritative" prompt text that exists solely because of injection.

Do not replace tail injection with front injection.

---

# 14. Prefix-cache behavior

The final design should naturally improve prefix stability.

Normal operation becomes append-only:

```text
stable system prompt
conversation history
assistant update_plan call
tool result
new tool calls/messages...
```

A plan status transition creates a new tool call near the tail rather than mutating an early system prefix.

This is desirable.

Do not introduce mutable current-plan text near the beginning of the request.

Do not optimize cache behavior using duplicate plan messages.

---

# 15. System prompt changes

The system prompt should explain how to use `update_plan`, but must not embed current plan state.

Keep guidance such as:

```text
Use update_plan for multi-step work when a plan materially helps execution.
Keep exactly one step in progress when work is actively underway.
Update the plan when progress changes.
Do not use a plan for trivial one-step tasks.
```

Do not include:

```text
The current plan is authoritative.
Always continue the injected current plan.
Read the current plan system block before acting.
```

The latest user instruction always remains authoritative over an older execution plan.

The plan is a working aid, not a higher-priority instruction source.

---

# 16. Plan semantics

Recommended plan invariants:

- plan may be absent;
- plan is optional for simple tasks;
- full update replaces previous active plan;
- at most one item is `in_progress`;
- completed items do not return to `pending` unless the model explicitly rewrites the plan for a valid reason;
- plan item text should describe observable work, not private reasoning;
- plan must not store chain-of-thought;
- UI state must be deterministic from durable session events.

Validate structural invariants in the tool implementation where practical.

Do not rely only on prompt guidance.

---

# 17. Do not turn Todo into a project manager

Keep `update_plan` lightweight.

Do not add unless already justified by real product needs:

```text
dependencies
owners
deadlines
labels
priority matrices
nested subtasks
progress percentages
timestamps exposed to model
task IDs exposed to model
arbitrary metadata
```

The purpose is coding-agent execution tracking.

Minimal useful model:

```text
content
status
```

Optional display metadata may stay outside the model protocol if the UI needs it.

---

# 18. Required deletions

The final branch should delete all obsolete Plan/Todo context-injection machinery.

Search for and remove concepts such as:

```text
current plan system message
current plan authoritative snapshot
plan system injection
todo system injection
tail system plan
append plan system
build_context plan push
plan_as_system
inject_plan
current_plan_message
CURRENT PLAN
当前计划
唯一权威
完整快照
以此为准
```

Also remove obsolete tests/docs/comments.

Do not leave disabled or dead compatibility code.

---

# 19. Required keep list

Do not delete:

```text
update_plan tool
PlanItem structured state
PlanUpdated durable event
session projection
UI plan display
session replay
branch-aware projection
compaction
resume
tool-call history
```

The goal is to remove duplicated **model injection**, not remove plan functionality.

---

# 20. Required tests

Implement or update at least the following contract tests.

## 20.1 `plan_event_not_in_model_surface`

Given:

```text
UserMessage
PlanUpdated
AssistantMessage
```

derived model messages must contain:

```text
UserMessage
AssistantMessage
```

and must not synthesize a plan System/User/Developer message.

## 20.2 `update_plan_tool_call_remains_in_history`

After model calls:

```text
update_plan(...)
```

the normal assistant tool-call and tool-result history remains available to subsequent model requests.

Do not remove the actual tool history while removing the state event.

## 20.3 `new_turn_clears_active_plan`

Scenario:

```text
Turn A:
    update_plan(A in_progress)
    turn finishes

Turn B starts
```

Expected:

```text
active_plan == None
```

Historical event remains stored.

## 20.4 `new_user_instruction_not_overridden_by_old_plan`

Scenario:

```text
old plan:
    Phase 4 in_progress

new user message:
    stop this and inspect another issue
```

Model request must not include a synthetic system message telling the model to continue Phase 4.

## 20.5 `plan_projection_replays_from_events`

Replay the same session events twice.

Expected:

```text
identical active/historical plan projection
```

No hidden mutable singleton state.

## 20.6 `plan_projection_is_branch_local`

Create two branches after a shared history.

Update plan differently on each branch.

Expected:

```text
branch A projection == A
branch B projection == B
```

## 20.7 `compaction_preserves_unfinished_plan`

Before compaction:

```text
A completed
B in_progress
C pending
```

After compaction removes old tool-call detail:

```text
summary contains sufficient execution state for B/C
```

Do not create an independent recurring plan system injection.

## 20.8 `completed_plan_not_needlessly_compacted`

If all items are completed at compaction boundary and no continuation depends on them:

```text
do not add redundant active execution plan text
```

Historical summary may mention completed work naturally if relevant.

## 20.9 `resume_does_not_inject_plan_message`

Resume a session with historical PlanUpdated events.

Expected:

```text
UI projection restored
model context contains no synthesized current-plan system message
```

## 20.10 `single_in_progress_validation`

If final product adopts the invariant:

```text
max one in_progress
```

invalid plan updates must be rejected or normalized by one clearly defined rule.

Prefer rejection with a concise diagnostic.

---

# 21. Definition of Done

The Todo/Plan refactor is complete only when all conditions are true.

- [ ] `update_plan` remains functional.
- [ ] Plan state is stored durably in session/event state.
- [ ] Plan UI still works.
- [ ] Plan replay still works.
- [ ] Plan state is branch-aware.
- [ ] Plan state is not converted into a recurring System message.
- [ ] Plan state is not converted into a recurring Developer message.
- [ ] Plan state is not converted into a recurring User message.
- [ ] `build_context()` does not append/prepend current-plan snapshots.
- [ ] The model sees its own `update_plan` tool-call history normally.
- [ ] `update_plan` tool result is concise and does not duplicate the full plan unnecessarily.
- [ ] A new user turn clears the old active plan projection.
- [ ] Historical plans remain available for replay/UI/debugging.
- [ ] Compaction carries unfinished execution state only when required.
- [ ] Resume restores UI/session plan state without synthetic plan injection.
- [ ] Old plan cannot become higher-priority than a new user instruction.
- [ ] Prefix cache is not invalidated by mutable plan text near the system-prefix region.
- [ ] No legacy compatibility injection path remains.
- [ ] No tests exist solely to preserve old plan-system-message behavior.
- [ ] System prompt describes `update_plan` usage but contains no current plan state.
- [ ] Architecture docs describe Todo as application/session state, not model-message state.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings -D clippy::undocumented_unsafe_blocks` passes.
- [ ] `cargo test --all-targets --all-features` passes.

If current plan state is still injected into every model request in any role, this refactor is not complete.

---

# 22. Execution instructions for the coding agent

1. Locate every Plan/Todo type, event, projection, tool, context-builder hook, UI projection, compaction hook and resume path.
2. Draw the current call graph before editing.
3. Identify the exact point where current plan state becomes `ChatMessage::System`.
4. Remove that conversion entirely.
5. Preserve the normal `update_plan` assistant tool-call and tool-result messages.
6. Make PlanUpdated an application/session-state event excluded from model-surface derivation.
7. Implement turn-start clearing of active plan projection.
8. Keep historical plan events durable.
9. Integrate unfinished active plan into compaction summaries only at compaction time.
10. Verify resume and branch behavior.
11. Remove old injection helpers, compatibility code, tests and documentation.
12. Update the system prompt to describe usage semantics only.
13. Run focused Plan/Todo tests.
14. Run full formatting, lint and test suite.
15. Search again for old injection markers and delete remaining dead references.

Do not introduce a replacement "Harness State" message containing the current plan.

Do not preserve the old mechanism under another role or name.

---

# 23. Final invariant

The completed design must satisfy:

```text
Plan is written by the model through update_plan.

Plan is stored by the session as structured state.

Plan is shown by the UI from projection.

Plan is remembered by the model through normal tool-call history.

Plan survives compaction through the compaction summary when necessary.

Plan does not become a synthetic message on every request.

A new user turn starts with no inherited active execution plan.
```

This is the final target architecture.
