# TPI

TPI 是一个面向 Windows、使用 Rust 实现的个人终端 coding agent。命令执行统一使用
Git Bash；模型接入采用 OpenAI-compatible Chat Completions 流式接口。

## 使用

```powershell
# 编译并安装 TPI，同时准备随包 Git Bash
.\scripts\install.ps1

# 系统已安装 Git Bash 时
.\scripts\install.ps1 -SkipBash

# 生成用户配置并检查运行环境
tpi init
tpi doctor

# 凭据写入 Windows Credential Manager，不写入配置文件
tpi auth set <provider>

# 交互、一次性提示和恢复会话
tpi
tpi "修复当前失败的测试"
tpi -p "解释这个仓库的入口"
tpi --continue
tpi --resume <session-id>
```

用户配置位于 `~/.tpi/config.toml`，项目配置位于
`<workspace>/.tpi/config.toml`。项目配置覆盖用户配置；模型配置缺失时 TPI 会明确报错，
不会选择隐藏的默认模型。`AGENTS.md` 与 `~/.tpi/SYSTEM.md` 会作为项目和用户规则载入。

## 架构边界

| 模块 | 隐藏的设计知识 |
| --- | --- |
| `app` | CLI/交互生命周期、会话选择和取消信号 |
| `agent` | 模型—工具循环、预算、调度和工具运行时状态 |
| `provider` | OpenAI-compatible 请求、SSE 解析、重试和本地 trace |
| `session` | append-only 事件日志、恢复、投影和 artifact 生命周期 |
| `tool` | 静态工具集合、参数校验、路径边界和统一结果协议 |
| `process` | Git Bash 子进程、Windows Job Object 和进程树取消 |
| `tui` | 终端所有权、事件 reducer、视图状态和渲染 |

关键不变量：session 事件日志是持久事实源；`Conversation` 同时拥有日志和可 replay
的模型历史；工具 schema、参数解析、访问声明和执行语义集中在工具层；只有 TUI
renderer 可以写终端；生产 `web_fetch` 始终拒绝 loopback、私网和链路本地目标。

## 开发与验证

工具链固定为 Rust 1.97。提交前运行：

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings -D clippy::undocumented_unsafe_blocks
cargo test --all-targets --all-features
```

CI 在 Windows 执行相同门禁，并定期通过 RustSec 审计 `Cargo.lock`。三个真实 provider
smoke test 默认忽略；它们需要显式提供真实凭据和网络环境。评测夹具可用
`python scripts/gen_evals.py` 重新生成，再通过 `tpi eval --list` 检查。

