---
name: rust-review
description: Rust 代码审查：关注抽象边界、错误处理与可维护性
---

# Rust Review

对 Rust 代码做结构化审查时，按以下顺序：

1. **读取目标代码**：`read` / `list` 定位相关模块。
2. **错误处理**：检查是否使用 `thiserror` / `anyhow` 分层、是否避免 `unwrap`/`panic`。
   详见 `references/error-handling.md`。
3. **抽象边界**：模块是否职责单一、pub 面是否最小。
4. **可维护性**：命名、文档、测试覆盖。
5. **输出**：按「问题 → 证据 → 建议」格式汇报，优先最小修改。

> references/ 下的文件按需读取（progressive disclosure）。
