# TPI

面向 Windows 的个人终端 Coding Agent（Rust / ratatui）。

## 功能

- 交互式 TUI 会话与 `-p` 非交互模式（stdout 只输出最终答案）
- Durable session：JSONL 事件流是唯一事实源，支持 `--continue` / `--resume` 恢复
- 内置工具：read / list / search / glob / edit / write / bash / request_input 等
- MCP 与 Skills 扩展（工具注册表 + SKILL.md 渐进式披露）
- 托管后台进程（`bash background=true` + process 工具）
- Remote workspace：经 SSH 操作远端工程（russh）
- Windows 集成：Git Bash 执行链、Windows Credential Manager 凭据存储、UTF-8 控制台

## 构建与验证

需要 Rust 1.97+（`rust-toolchain.toml` pin 1.97.1，与 CI 一致）。

```bash
cargo build
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings -D clippy::undocumented_unsafe_blocks
cargo fmt --all -- --check
```

## 使用

```text
tpi                          # 当前目录进入交互会话
tpi "修复这个测试"            # 进入交互并提交首条消息
tpi -p "解释失败原因"         # 非交互，stdout 只输出最终答案
tpi --continue               # 继续当前 workspace 最近 session
tpi --resume <session-id>    # 恢复指定 session（完整 id 或唯一前缀）
tpi --model <name>           # 从配置的多个模型中选择（primary + profiles）
tpi auth set <provider>      # 把 token 写入 Windows Credential Manager
tpi init                     # 交互式生成配置
tpi doctor                   # 环境检查（config/模型/API key/Git Bash/目录）
```

## 模型配置（~/.tpi/config.toml 或 <workspace>/.tpi/config.toml）

`[model.primary]` 是默认模型；`[[model.profiles]]` 可配置多个备选模型，
`tpi --model <name>` 选择（按 `name` 匹配，未指定时用 primary）。
API key 可直接写在配置文件的 `api_key` 字段（无需系统变量）；读取优先级：
环境变量（`api_key_env`，显式覆盖）> 配置文件 `api_key` > Windows 凭据管理器
（`tpi auth set`）。

```toml
[model.primary]
provider = "openai"
name = "gpt-4o"
base_url = "https://api.openai.com/v1"
api_key = "sk-..."          # 可省略；省略时走环境变量/凭据管理器

[[model.profiles]]           # 备选模型（可选）
provider = "anthropic"
name = "claude-sonnet"
base_url = "https://api.anthropic.com/v1"
api_key = "sk-ant-..."

[[model.profiles]]
provider = "openai"
name = "gpt-4o-mini"
base_url = "https://api.openai.com/v1"
```

> 注意：`api_key` 是明文存储，请勿把配置文件提交到版本库
> （Windows 下建议限制文件权限；或改用 `tpi auth set` 存凭据管理器）。

## 文档

- `docs/architecture.md`：核心架构与设计原则（Session is truth / Context is projection / ToolExecutor 深模块等）
- `AGENTS.md`：本仓库的代码审查与修复准则（供 Coding Agent 工作）

## 目录

```text
src/agent/      AgentLoop 与工具执行编排
src/session/    durable 事件存储（source of truth）
src/context/    model context 投影
src/tool/       工具抽象、注册表、调度与内置工具
src/tui/        ratatui 界面（纯 reducer + effect）
src/provider/   模型适配（openai-compat 等）
src/mcp/        MCP server 生命周期与工具适配
src/skills/     SKILL.md 工作流
src/process/    托管后台进程（process-host 单二进制模式）
src/remote/     SSH remote workspace
src/eval/       自动评测 harness（真实 provider，显式运行才产生费用）
tests/          契约 / 属性（proptest）/ 集成测试
```
