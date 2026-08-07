# 从 Pi 迁移到 TPI

本文说明如何把当前 Pi 个人配置迁移到 TPI（§21 M6：当前个人 SYSTEM.md、主题
和 provider 配置迁移说明）。TPI 的实现契约见 `TPI_DESIGN.md`。

## 1. 配置位置

| 内容 | Pi（旧） | TPI（新） |
| --- | --- | --- |
| 全局设置 | `~/.pi/config.json` | `~/.tpi/config.toml` |
| 个人指令 | `~/.pi/agent/APPEND_SYSTEM.md` | `~/.tpi/SYSTEM.md` |
| Session | `~/.pi/sessions/...` | `~/.tpi/sessions/<workspace-id>/...` |
| 日志 | 终端内 | `~/.tpi/logs/tpi.log`（tracing，§19.2） |
| 凭据 | 环境变量/明文 | Windows Credential Manager（§18.4） |

旧 Pi sessions 默认不做格式兼容；TPI 与 Pi 并存，必要时以后写一次性离线导入器
（§2.4 迁移矩阵）。

## 2. 个人指令迁移（SYSTEM.md）

`~/.pi/agent/APPEND_SYSTEM.md` 中的内容：

- **保留**：软件设计偏好、Git 行为、表达习惯等稳定个人规则 → 写入 `~/.tpi/SYSTEM.md`。
- **删除**：与 TPI 工具协议重复的临时规则——TPI 已用代码保证 revision 回传、
  bash 真实退出码、stderr 不误判、stdout 单 renderer 等（§2.4：能由工具协议
  消除的问题应由代码消除，而不是继续增加提示词）。
- 内建 system prompt 跟随版本测试，不需要手工维护（`src/agent/system_prompt.md`）。

## 3. 主题迁移

Pi 的 `omp-inspired` 配色偏好 → TPI 的 OMP 语义主题（`src/tui/theme.rs`）。
主题按语义角色（user/assistant/reasoning/tool/system）着色；如需自定义，
修改 `Theme::omp()` 的语义颜色即可，组件不直接依赖具体颜色（§16.3）。

## 4. Provider 配置迁移

TPI 只实现 OpenAI-compatible Chat Completions adapter（§7.1）。在
`~/.tpi/config.toml` 配置：

```toml
[model.primary]
provider = "opencode-go"
name = "deepseek-v4-flash"
base_url = "https://<你的端点>/v1"   # 从当前 Pi 配置迁入，不硬编码
reasoning = "max"
max_output_tokens = 16384
context_window = 1000000
# 注意：supports_tools / reasoning_field 尚未实现（P1-11：配置示例与实现对齐）；
# 工具支持与 reasoning 字段名当前按 openai-compat 固定约定（§18.1）。
# api_key_env = "TPI_API_KEY"   # 环境变量显式覆盖（§18.4）

[shell]
kind = "git-bash"
# path = "C:\\Program Files\\Git\\bin\\bash.exe"

[agent.limits]
max_model_turns = 80
max_tool_calls = 160
max_wall_time_minutes = 45
max_parallel_tools = 4
max_identical_no_progress = 2

[web]
search_backend = "brave"        # v1 固定 Brave（§17）
auto_open_browser = false       # 绝不自动打开浏览器
summary_model = "none"          # web_summary 默认关闭
# brave_api_key_env = "BRAVE_API_KEY"
```

凭据不写进 TOML：

```powershell
$env:TPI_API_KEY = "..."        # 或
tpi auth set opencode-go        # 写入 Windows Credential Manager（§18.4）
```

## 5. 行为差异速查

- `bash` 工具固定使用 Bash 语法（Git Bash），是唯一命令执行工具；
  需要 PowerShell 时在 bash 命令里调用 `pwsh.exe`。
- 普通程序、构建、测试、Git 都用 `bash` 执行。
- `edit` 只替换明确给出的 `old_text`（revision-bound exact edit）；stale 必须
  重新 `read`（§10.3）。
- `write` 只创建新文件；已有文件整体重写也必须通过 `edit`（§10.6）。
- 输出有界：完整输出在 artifact，模型用 `read(@artifact/...)` 读取（§8.4）。
