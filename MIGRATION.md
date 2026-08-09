# TPI 迁移与配置

从其他个人 Agent（或从零）迁移到 TPI 的配置说明。TPI 不提供"看不见的
默认模型"（§18.1）：模型必须显式配置。

## config.toml 完整示例

配置文件位于 `~/.tpi/config.toml`（`tpi init` 生成骨架）。所有字段均可选。

```toml
[model.primary]
provider = "opencode-go"        # provider 名（写凭据/文档用）
name = "deepseek-v4-flash"      # 模型名（传给 API 的 model 字段）
base_url = "https://api.example.com/v1"
reasoning = "high"              # 可选：传给 API 的 reasoning 字段
max_output_tokens = 8192        # 可选：单次输出上限
context_window = 131072         # 可选：上下文窗口（compaction 阈值按它算）
api_key_env = "TPI_API_KEY"     # 可选：API key 环境变量名（默认 TPI_API_KEY）
# price_input = 0.0             # 可选：每百万输入 token 美元（§16.2 花费展示）
# price_output = 0.0            # 可选：每百万输出 token 美元

[shell]
# kind = "git-bash"             # 可选：shell 种类（当前仅 git-bash）
# path = "C:\\tools\\tpi\\git\\bin\\bash.exe"   # 显式 Git Bash 路径（§11.2 解析顺序第 1 位）

[agent]
allow_outside_workspace = true  # 可选：允许文件工具访问 workspace 外（默认 true）

[agent.limits]
# max_model_turns = 80          # 默认 80
# max_tool_calls = 160          # 默认 160
# max_wall_time_minutes = 45    # 默认 45
# max_parallel_tools = 5        # 并行工具数
# max_identical_no_progress = 3 # 相同无进展动作上限（确定性 no-progress 检测）

[context]
# safety_reserve_tokens = 8192  # compaction 触发阈值的 safety reserve（默认 8192）

[ui]
# theme = "omp"                 # omp / dark / light（未知值回退 omp）
# mode = "fullscreen"           # fullscreen（默认）/ inline（兼容模式，§1.2）
```

## 从其他 Agent 迁移

### 模型配置

把原配置里的 provider/base_url/model 翻译为 `[model.primary]` 四字段即可。
TPI 用 OpenAI-compatible chat completions SSE；`reasoning` 字段原样传给 API，
不需要时省略。

### 凭据

- 环境变量：设 `TPI_API_KEY`（或配置里的 `api_key_env` 名字）即可，显式覆盖
  keyring（§18.4）。
- Windows Credential Manager：`tpi auth set <provider>` 把 token 写入 keyring，
  配置里不落盘明文。读取优先级：环境变量 > keyring。

### Bash

bash 是唯一命令执行通道（§11），需要 Git Bash。`scripts\install.ps1` 会下载
随包 Git Bash（免装系统 Git）；系统已装 Git 时可 `-SkipBash`，或用
`[shell].path` 显式指定 Git Bash 的 bash.exe 路径。

## 常见问题

- `tpi doctor` 可检查环境：config、API key、Git Bash、目录可写。
- 运行期间复制、主题、资源限制等均可在不重启会话的情况下通过配置调整
  （TUI 每次启动重新读配置）。
