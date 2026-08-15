# P6-06/07 spike 决策记录（2026-08-14）

## P6-06 Crossterm EventStream spike

**结论：保留 owned blocking thread（键盘线程，P3-06 已加 start/shutdown/join 契约）。**

验证点：
- Windows IME / bracketed paste / repeat / exit parity：owned thread 已实现
  bracketed paste 探测（`src/app/mod.rs` 键盘线程 + `tui/paste.rs`），
  crossterm Windows 后端不解析 `[200~` 序列 → 需要自实现；
- EventStream 迁移收益不明确：现有线程阻塞 `event::read()` 零延迟，迁移到
  async EventStream 需处理 `KeyEventKind` 过滤、paste 序列重实现；
- 风险：同时改 app 与 paste（roadmap 明确"延后到 Phase 6，避免同时改 app 和 paste"）。

**决策：不迁移。** owned thread 满足全部验收（P3-06 join 契约；terminal error 明确
反馈；ctrl-c 由 Ctrl-C handler 处理）。EventStream 相关改进（若 IME 场景真实出现）
记入 backlog，需真实用户场景才做。

## P6-07 renderer strategy spike

**结论：保持单 renderer（当前 main/alt 行为），不引入双 renderer。**

验证点：
- 当前行为：Ratatui inline renderer（M5）+ FRAME_INTERVAL 帧合并 + synchronized
  update；已有 `renderer.restore()`（交互退出恢复终端）；
- 双 renderer（main/alt screen 切换）需要用户明确需求（如"错误后保留输出"）；
- 无真实 consumer 时不加复杂度（AGENTS.md：不要为未来假设增加抽象）。

**决策：不引入。** 记录为"延后"ADR（roadmap P7-06 同机制）。若用户明确需要
alt-screen 全屏模式，再评估。

## 结论对 roadmap 的影响

- P6-06/P6-07 判定为"spike 决策：保留现状"，不改变行为；
- 相应测试（键盘线程 join 契约 P3-06、renderer restore 既有）保持绿色。
