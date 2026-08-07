# TPI：个人终端 Coding Agent

面向 Windows、以 Bash 为 Shell 方言、由 Rust 实现的个人终端 Coding Agent。
实现契约与设计决策见 [TPI_DESIGN.md](TPI_DESIGN.md)（绝对标准）；从 Pi 迁移
个人配置见 [MIGRATION.md](MIGRATION.md)。

## 快速开始

```powershell
# 1. 一键安装：编译 tpi + 下载随包 Git Bash（bash 工具免装 Git）
#    等价于 cargo install --path . --locked + scripts\install-bash.ps1
.\scripts\install.ps1

# 2. 配置模型（§18.1：不允许"看不见的默认模型"）
#    在 ~/.tpi/config.toml 写入 [model.primary]（provider/name/base_url），
#    示例见 MIGRATION.md。

# 3. 凭据（§18.4：写入 Windows Credential Manager，不写进 TOML）
tpi auth set opencode-go

# 4. 运行
tpi                      # 交互会话（Ratatui inline TUI）
tpi "修复这个测试"       # 进入交互并提交首条消息
tpi -p "解释失败原因"    # 非交互，stdout 只输出最终答案
tpi --continue           # 继续当前 workspace 最近 session
tpi --resume <session-id>
tpi --model <name>
```

`scripts\install.ps1 -SkipBash` 跳过 Git Bash 下载（系统已装 Git 时可选）；
纯编译安装 `cargo install --path . --locked`（release profile 已配置）。

## 已实现能力（按里程碑）

- **M0**：核心类型、fake provider、4 个驱动失败测试、CI。
- **M1**：CLI/配置、OpenAI-compatible SSE、read/edit/write、串行
  tool-call loop、append-only session（JSONL）、write-ahead 与恢复、Ctrl-C 取消。
- **M2**：list/search（ignore/预算/cursor）、bash（Git Bash + pipefail，唯一执行通道）、
  artifact store（`@artifact/...` 有界读取）、process-host + Job Object
  进程树取消。
- **M3**：SnapshotStore、ReplaceFileW + backup 恢复协议、CRLF/BOM/mixed
  行尾 property tests、unified diff、竞态检测（backup digest）。
- **M4**：资源感知 scheduler（waves 并行 read/串行 write）、确定性
  no-progress 检测、watchdog 预算、原子短计划 `update_plan`、context
  pruning + compaction（§15.4 完整语义）。
- **M5**：Ratatui inline renderer（transcript/editor/footer）、OMP 语义主题、
  16 ms 帧合并、synchronized update、中文 slash commands、TestBackend
  稳定性指标（§20.3）。
- **M6**：web_search（Brave，不调用 LLM endpoint）/web_fetch（有界 + html2text）、
  keyring 凭据、迁移说明、release profile。

## 边界

未实现 Skills/MCP/插件兼容、多 Agent 工作流、跨会话记忆、client/server、
隐藏审阅模型；核心代码没有为它们预留空框架（§25）。

## 测试

`cargo test --all-targets`（60+ 测试；真实 provider smoke 需
`TPI_RUN_LIVE_TESTS=1` + 凭据，§20.1）。
