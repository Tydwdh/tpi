# ADR-003：JSONL 继续作为 Session 事实源

- 状态：**Approved（P0-08，2026-08-14）**
- 关联：`docs/refactor/05-session-context-storage.md` §1、`docs/refactor/08-migration-roadmap.md` P0-08

## Context

Session 是 TPI 最重的资产：append-only JSONL 事件流承载恢复、审计、模型可见性与
用户历史。现状（审计 §2.2）已具备 typed `SessionEvent`、envelope（schema/seq/
event_id/timestamp）、单写者、尾部修复（Truncate/AppendNewline）、protocol
validation、compaction 与 repair。任何"换存储"都会同时触碰 wire 格式、崩溃恢复、
repair、迁移与降级，风险极高；而重构期间真正的问题是**职责分散**（session/mod.rs
1,488 行同时承担 protocol/codec/store/projection），不是存储介质。

## Decision

**重构期间不迁移主存储：JSONL append-only event log 继续是唯一事实源**。
派生视图（conversation、transcript、plan、metrics/eval）全部是 projection，
从 JSONL 重建；不把 UI transcript 或 provider request 变成事实源。

```text
Session JSONL (canonical durable facts)
    +--> Conversation projection --> context policies --> provider messages
    +--> Transcript projection ----> UI view model
    +--> Plan/process projection ---> runtime restore
    +--> Catalog/search projection -> optional SQLite（P9-02 证据门控，当前关闭）
    +--> Metrics/eval projection
```

P2 只做**物理拆文件**（protocol/codec/store/projection 各归其位）与 `SessionStore`
port + JSONL adapter，wire 格式零变化（golden hash 证明 encode 不变）。

## Alternatives

- **SQLite / redb 主存储**：延后（P9-02）。只有真实全文检索需求 + JSONL 扫描性能
  数据证明必要才评估（用户决策 2026-08-14：当前不需要，不引入 SQLite）。
- **UI transcript 作事实源**：拒绝。TUI 可丢弃、可重排、含展示字段，不能承载恢复。
- **provider request 作事实源**：拒绝。请求是 projection 的输入，不是事实本身
  （模型可见 ⇔ 可重建，见 12 号文档 O3，但那是诊断层不是存储层）。

## Consequences

正面：
- 重构期间 session 兼容窗口 = 现有 reader 行为，P0-03 golden corpus 直接成为回归网。
- 崩溃恢复/repair 语义不动；P10 迁移 rehearsal 只针对"读旧→写新"的投影。
- 不引入新依赖（SQLite 等）与迁移/锁/损坏面。

负面/成本：
- JSONL 全量扫描在超大 session 上可能成为性能上限——P0-05 基线已建立
  （10k 消息 replay 读 17ms / 4.5MB），P9-02 的证据门用此数据判断是否需要 SQLite。
- 全文检索功能缺位（用户已确认当前不需要）。

## Migration

1. P0-03 已建立 session golden corpus（`tests/fixtures/session_corpus/`，
   6 个 fixture：真实 replay/中断/挂起/compaction/corrupt tail/corrupt middle）
   + `tests/session_golden.rs`（10 断言）。
2. P2-01 机械拆 protocol/codec/store：仅移动代码与 visibility，golden hash 证明
   encode 无变化；old corpus 全过。
3. P2-02 `SessionStore` port + JSONL adapter：当前 `SessionLog` 实现 port；
   in-memory fake 跑 agent_flow。
4. P2-03 conversation/transcript/plan 纯 projector；属性测试 incremental == rebuild。
5. P10-02 migration rehearsal：用户副本 corpus dry run / migrate / rollback /
   old binary read 行为对照。

## Rollback

- P2 拆文件：revert PR（wire 零变化，行为可逆）。
- 若未来真换存储（P9-02）：必须双写/双读一周期 → 新存储为权威 → 保留一个 release
  的旧 reader fallback → 删除。任何 migration failure 停止该 session，不写半迁移文件
  （文档 §14.3）。

## Evidence

- `docs/refactor/baseline-2202887.md` §3：39 个真实 session、0 损坏、最大 5.3MB。
- `tests/session_golden.rs` 10 断言全绿（真实 replay + seq 单调 + lifecycle 覆盖 +
  corrupt tail 截断 + corrupt middle 报错 + blake3 校验）。
- P0-05：10k 消息 replay 17ms / context estimate 0.77ms（合成基线）。

## 非目标

- 不引入 SQLite/redb 派生索引（除非 P9-02 证据门通过）。
- 不重写 JSONL 格式/schema 版本（保持 schema=1 兼容窗口）。
- 不把 provider trace / trace sink 变成 session 事实源（trace 是诊断平面，见 12 号文档）。
- 不决策 conversation 投影的压缩策略细节（见 `05-session-context-storage.md` §3）。
