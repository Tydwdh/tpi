# TPI

面向 Windows 的个人终端 Coding Agent（Rust / ratatui）。

## 功能

- 交互式 TUI 会话与 `-p` 非交互模式（stdout 只输出最终答案）
- Durable session：JSONL 事件流是唯一事实源，支持 `--continue` / `--resume` 恢复
- 内置工具：read / edit / write / bash / request_input 等（内容/文件名检索用 bash + rg）
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
tpi server                   # 多端 Server：HTTP + WebSocket（默认 127.0.0.1:8765）
```

### 多端（Web / Desktop）

```text
tpi server --web-dist apps/web/dist
# 然后浏览器打开 http://127.0.0.1:8765
# （未指定 --token 时本地随机生成并打印一次；连接经 ?token= 注入）
```

开发模式（Vite HMR + 代理）：

```text
cd apps/web && npm install && npm run dev
# 另一个终端：tpi server（Vite 代理 /api 与 /ws 到 127.0.0.1:8765）
```

Desktop（Tauri 复用同一前端 + embedded server）：

```text
cd desktop/src-tauri && cargo run
```

协议与多端架构见 `web_desktop.md` 与 `docs/architecture.md` §11。

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

OpenAI Responses API 模型也可以直接配置；`base_url` 要填写完整的 `/responses` 端点，
例如 OpenCode Zen 的 Muse Spark 1.2：

```toml
[[model.profiles]]
provider = "opencode-go"
name = "muse-spark-1.2"
base_url = "https://opencode.ai/zen/go/v1/responses"
api_key_env = "OPENCODE_API_KEY"
```

保存配置后，在 TUI 输入 `/model`，选择 `muse-spark-1.2`；或启动时使用
`tpi --model muse-spark-1.2`。Responses API 与 Chat Completions 的端点和请求格式不同，
不要把 `/responses` 改成仅有 `/v1` 的地址。

> 注意：`api_key` 是明文存储，请勿把配置文件提交到版本库
> （Windows 下建议限制文件权限；或改用 `tpi auth set` 存凭据管理器）。

## 文档

- `docs/architecture.md`：核心架构与设计原则（Session is truth / Context is projection / ToolExecutor 深模块等）
- `AGENTS.md`：本仓库的代码审查与修复准则（供 Coding Agent 工作）

## 目录

```text
crates/tpi-protocol/ 多端协议 DTO（Command/Event/View/Error/Envelope）
crates/tpi-runtime/  唯一业务入口（RuntimeHandle + actor 风格 ApplicationService）
crates/tpi-server/   Network Adapter（Axum HTTP + WebSocket + auth + embedded）
apps/web/            React + Vite Web UI（复用 packages/tpi-client）
packages/tpi-client/ TypeScript 协议客户端 SDK
crates/tpi-core/     纯数据与工具层
crates/tpi-session/  durable 事件存储（source of truth）
crates/tpi-agent/    AgentLoop 与工具执行编排
crates/tpi-tui/      ratatui 界面
crates/tpi-capabilities/  MCP/Skills/process/remote/shell/tool/workspace
src/                 CLI 入口、app 层、eval、web（旧版局域网接口）
tests/               契约 / 属性（proptest）/ 集成测试
```
