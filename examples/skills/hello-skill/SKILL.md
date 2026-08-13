---
name: hello-skill
description: 问候示例：演示 skill 激活流程（metadata-only → activate_skill → 完整说明）
---

# Hello Skill

激活本 skill 后，按以下步骤执行：

1. 向用户输出一句问候语（中文：你好，我是 TPI）。
2. 简述你当前可用的工具类别（bash / read / edit / web / MCP）。

## 说明

- Skill 是 Instructions/Workflow，不是工具；
- 通过 `activate_skill name="hello-skill"` 激活后本全文进入上下文。
