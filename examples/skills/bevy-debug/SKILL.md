---
name: bevy-debug
description: Bevy 游戏调试：查询实体状态、模拟输入、诊断角色移动问题
---

# Bevy Debug

诊断 Bevy 角色移动问题时的工作流（组合 bash/read/MCP 工具）：

1. 构建游戏：`bash cargo build`
2. 启动游戏：`bash cargo run`（或测试模式）
3. 通过 Bevy MCP 查询 Player Entity 初始 Transform
4. 模拟 W 输入（forward）
5. 等待一个 tick
6. 再查询 Transform / Velocity
7. 比较前后状态，判断输入是否生效
8. 若未生效：阅读输入系统与移动系统代码（`search` + `read`）
9. 修改代码 → `cargo check` → 重启 → 再次运行时验证

> Skill = workflow；MCP = runtime capability（README2 §22 典型组合）。
