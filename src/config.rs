//! 配置（文档 §18）。
//!
//! 优先级（§18.1）：CLI 参数 → workspace `.tpi/config.toml` → `~/.tpi/config.toml` → 内建默认值。
//! 不允许"看不见的默认模型"：模型配置缺失时明确报错。

use camino::Utf8PathBuf;
use serde::Deserialize;

/// 配置根目录（~/.tpi，§14.1）。
pub fn tpi_home() -> std::path::PathBuf {
    std::env::var_os("TPI_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|home| std::path::PathBuf::from(home).join(".tpi"))
                .unwrap_or_else(|| std::path::PathBuf::from(".tpi"))
        })
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ConfigFile {
    pub model: ModelFile,
    pub agent: AgentFile,
    pub shell: ShellFile,
    pub context: ContextFile,
    pub ui: UiFile,
}

/// §16.3 [ui] 配置（P2：主题可选，默认 omp；TUI v2：模式可选，默认 fullscreen）。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct UiFile {
    /// 主题名：omp / dark / light（未知值回退 omp）。
    pub theme: Option<String>,
    /// 视口模式：fullscreen（默认）/ inline（兼容模式；§1.2）。
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ContextFile {
    /// §15.4 safety reserve（默认 8192；compaction 触发阈值用）。
    pub safety_reserve_tokens: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ShellFile {
    pub kind: Option<String>,
    /// 显式 Git Bash 路径（§11.2 解析顺序第 1 位）。
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ModelFile {
    pub primary: Option<PrimaryModelFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrimaryModelFile {
    pub provider: String,
    pub name: String,
    pub base_url: String,
    pub reasoning: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub context_window: Option<u64>,
    /// 从环境变量读取 API key 的变量名（默认 TPI_API_KEY）。
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AgentFile {
    pub limits: Option<AgentLimitsFile>,
    /// 是否允许文件工具访问 workspace 外的绝对路径（§9.1 自由模式；
    /// 默认 true——个人工具以 AI 自由优先；false 恢复严格 workspace 沙箱）。
    pub allow_outside_workspace: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentLimitsFile {
    pub max_model_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_wall_time_minutes: Option<u64>,
    pub max_parallel_tools: Option<u32>,
    pub max_identical_no_progress: Option<u32>,
}

/// 生效配置（含来源标注，§18.1：`/settings` 可查看来源）。
#[derive(Debug, Clone)]
pub struct Config {
    pub model: ModelConfig,
    pub limits: LimitsConfig,
    pub workspace_root: Utf8PathBuf,
    pub sessions_root: std::path::PathBuf,
    /// artifact 根目录（§14.1：`~/.tpi/artifacts`）。
    pub artifacts_root: std::path::PathBuf,
    /// 显式 Git Bash 路径（§11.2）。
    pub shell_path: Option<Utf8PathBuf>,
    /// §15.4：compaction 触发阈值的 safety reserve。
    pub safety_reserve_tokens: u64,
    /// §17：web_search 使用免费 DuckDuckGo 端点（无需 API key）。
    /// §17：绝不自动打开浏览器（v1 固定 false）。
    pub auto_open_browser: bool,
    /// §17：web_summary 默认关闭。
    pub web_summary_model: String,
    /// 个人全局指令（~/.tpi/SYSTEM.md，§18.2）。
    pub system_prompt_extra: Option<String>,
    /// 配置来源（`/settings` 展示用）。
    pub source: String,
    /// §16.3 [ui] theme：omp / dark / light（P2：主题可选，默认 omp）。
    pub ui_theme: String,
    /// §1 [ui] mode：fullscreen（默认）/ inline（兼容模式）。
    pub ui_mode: crate::tui::terminal::ViewMode,
    /// §9.1：文件工具（read/edit/write/list/search）是否允许访问 workspace 外路径。
    /// 默认 true（bash 本来就能自由访问，保持一致）；false 恢复严格沙箱。
    pub allow_outside_workspace: bool,
}

#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub provider: String,
    pub name: String,
    pub base_url: String,
    pub reasoning: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub context_window: Option<u64>,
    pub api_key_env: String,
}

#[derive(Debug, Clone, Copy)]
pub struct LimitsConfig {
    /// 默认 80（§18.1 示例）。
    pub max_model_turns: u32,
    /// 默认 160。
    pub max_tool_calls: u32,
    /// 默认 45 分钟。
    pub max_wall_time_minutes: u64,
    /// 默认 4（M4 scheduler 使用）。
    pub max_parallel_tools: u32,
    /// 默认 2（M4 no-progress 检测使用）。
    pub max_identical_no_progress: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_model_turns: 80,
            max_tool_calls: 160,
            max_wall_time_minutes: 45,
            max_parallel_tools: 4,
            max_identical_no_progress: 2,
        }
    }
}

/// 加载配置：合并 ~/.tpi/config.toml 与 workspace .tpi/config.toml。
///
/// 模型配置缺失时返回明确错误（§18.1：不允许看不见的默认模型）。
pub fn load(workspace_root: &Utf8PathBuf, cli_model: Option<&str>) -> Result<Config, String> {
    let home = tpi_home();
    let home_file = home.join("config.toml");
    let workspace_file = workspace_root.join(".tpi").join("config.toml");

    let home_config: ConfigFile = read_config(&home_file)?;
    let workspace_config: ConfigFile = read_config(workspace_file.as_std_path())?;

    let home_has_model = home_config.model.primary.is_some();
    let workspace_has_model = workspace_config.model.primary.is_some();

    // 优先级：workspace 覆盖 home（§18.1）。
    let merged = merge(home_config, workspace_config);

    let primary = merged
        .model
        .primary
        .ok_or_else(|| {
            format!(
                "未配置模型：请在 {} 或 {} 中设置 [model.primary]\n（provider/name/base_url 必填；不允许看不见的默认模型，§18.1）",
                home_file.display(),
                workspace_file
            )
        })?;

    let name = cli_model.unwrap_or(&primary.name).to_string();
    let source = if cli_model.is_some() {
        "cli --model".to_string()
    } else if workspace_has_model {
        format!("workspace {workspace_file}")
    } else if home_has_model {
        format!("user {}", home_file.display())
    } else {
        "builtin defaults".to_string()
    };

    let system_prompt_extra = read_system_md(&home.join("SYSTEM.md"));
    // P1-12：workspace 项目规则（AGENTS.md）——叠加在个人全局规则之后，
    // 注入时标明来源（§18.2：项目级约束进入 system prompt）。
    let system_prompt_extra = match read_system_md(workspace_root.join("AGENTS.md").as_std_path()) {
        Some(project_rules) => Some(match system_prompt_extra {
            Some(global) => format!("{global}\n\n[project rule: AGENTS.md]\n{project_rules}"),
            None => format!("[project rule: AGENTS.md]\n{project_rules}"),
        }),
        None => system_prompt_extra,
    };

    Ok(Config {
        model: ModelConfig {
            provider: primary.provider,
            name,
            base_url: primary.base_url,
            reasoning: primary.reasoning,
            max_output_tokens: primary.max_output_tokens,
            context_window: primary.context_window,
            api_key_env: primary.api_key_env.unwrap_or_else(|| "TPI_API_KEY".into()),
        },
        limits: LimitsConfig {
            max_model_turns: merged
                .agent
                .limits
                .as_ref()
                .and_then(|l| l.max_model_turns)
                .unwrap_or(80),
            max_tool_calls: merged
                .agent
                .limits
                .as_ref()
                .and_then(|l| l.max_tool_calls)
                .unwrap_or(160),
            max_wall_time_minutes: merged
                .agent
                .limits
                .as_ref()
                .and_then(|l| l.max_wall_time_minutes)
                .unwrap_or(45),
            max_parallel_tools: merged
                .agent
                .limits
                .as_ref()
                .and_then(|l| l.max_parallel_tools)
                .unwrap_or(4),
            max_identical_no_progress: merged
                .agent
                .limits
                .as_ref()
                .and_then(|l| l.max_identical_no_progress)
                .unwrap_or(2),
        },
        workspace_root: workspace_root.clone(),
        sessions_root: home.join("sessions"),
        artifacts_root: home.join("artifacts"),
        shell_path: merged.shell.path.as_deref().map(Utf8PathBuf::from),
        safety_reserve_tokens: merged.context.safety_reserve_tokens.unwrap_or(8192),
        auto_open_browser: false,
        web_summary_model: "none".into(),
        system_prompt_extra,
        source,
        ui_theme: merged.ui.theme.clone().unwrap_or_else(|| "omp".to_string()),
        ui_mode: merged
            .ui
            .mode
            .as_deref()
            .map(crate::tui::terminal::ViewMode::parse)
            .unwrap_or_default(),
        allow_outside_workspace: merged.agent.allow_outside_workspace.unwrap_or(true),
    })
}

fn read_config(path: &std::path::Path) -> Result<ConfigFile, String> {
    if !path.exists() {
        return Ok(ConfigFile::default());
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("解析 {} 失败: {e}", path.display()))
}

fn merge(home: ConfigFile, workspace: ConfigFile) -> ConfigFile {
    ConfigFile {
        model: ModelFile {
            primary: workspace.model.primary.or(home.model.primary),
        },
        agent: AgentFile {
            limits: merge_limits(home.agent.limits, workspace.agent.limits),
            allow_outside_workspace: workspace
                .agent
                .allow_outside_workspace
                .or(home.agent.allow_outside_workspace),
        },
        shell: ShellFile {
            path: workspace.shell.path.or(home.shell.path),
            kind: workspace.shell.kind.or(home.shell.kind),
        },
        context: ContextFile {
            safety_reserve_tokens: workspace
                .context
                .safety_reserve_tokens
                .or(home.context.safety_reserve_tokens),
        },
        ui: UiFile {
            theme: workspace.ui.theme.or(home.ui.theme),
            mode: workspace.ui.mode.or(home.ui.mode),
        },
    }
}

/// §18.1：配置合并是字段级，不是块级——workspace 只定义部分字段时，
/// home 的同块其余字段必须保留（否则 workspace 定义一项会静默丢弃 home 全部设置）。
fn merge_limits(
    home: Option<AgentLimitsFile>,
    workspace: Option<AgentLimitsFile>,
) -> Option<AgentLimitsFile> {
    match (home, workspace) {
        (Some(home), Some(workspace)) => Some(AgentLimitsFile {
            max_model_turns: workspace.max_model_turns.or(home.max_model_turns),
            max_tool_calls: workspace.max_tool_calls.or(home.max_tool_calls),
            max_wall_time_minutes: workspace
                .max_wall_time_minutes
                .or(home.max_wall_time_minutes),
            max_parallel_tools: workspace.max_parallel_tools.or(home.max_parallel_tools),
            max_identical_no_progress: workspace
                .max_identical_no_progress
                .or(home.max_identical_no_progress),
        }),
        (home, workspace) => home.or(workspace),
    }
}

fn read_system_md(path: &std::path::Path) -> Option<String> {
    if path.exists() {
        std::fs::read_to_string(path)
            .ok()
            .filter(|s| !s.trim().is_empty())
    } else {
        None
    }
}

/// 读取 API key（§18.4：环境变量显式覆盖；keyring 属 M6）。
pub fn read_api_key(config: &Config) -> Result<String, String> {
    // §18.4：环境变量是显式覆盖；否则从 Windows Credential Manager 读取
    //（`tpi auth set <provider>` 写入，配置只保存 credential label）。
    if let Ok(key) = std::env::var(&config.model.api_key_env)
        && !key.is_empty()
    {
        return Ok(key);
    }
    if let Some(key) = crate::auth::auth_get(&config.model.provider) {
        return Ok(key);
    }
    Err(format!(
        "未找到 API key：请设置环境变量 {} 或运行 `tpi auth set {}` 写入凭据（§18.4）",
        config.model.api_key_env, config.model.provider
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    /// P1-12：workspace 的 AGENTS.md 项目规则必须注入 system_prompt_extra
    ///（此前只读取 ~/.tpi/SYSTEM.md，项目级约束进不了 system prompt）。
    #[test]
    fn workspace_agents_md_is_injected() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        // 最小模型配置（load 拒绝无模型）。
        std::fs::write(
            home.join("config.toml"),
            "[model.primary]\nprovider = \"test\"\nname = \"m\"\nbase_url = \"https://example.invalid/v1\"\n",
        )
        .unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "永远使用 LF 换行\n").unwrap();
        unsafe {
            std::env::set_var("TPI_HOME", &home);
        }
        let config = load(&workspace, None).expect("load");
        let extra = config.system_prompt_extra.expect("AGENTS.md 必须注入");
        assert!(extra.contains("[project rule: AGENTS.md]"), "{extra}");
        assert!(extra.contains("LF 换行"), "{extra}");
    }

    #[test]
    fn merge_is_field_level_not_block_level() {
        let home = ConfigFile {
            agent: AgentFile {
                allow_outside_workspace: None,
                limits: Some(AgentLimitsFile {
                    max_model_turns: Some(100),
                    max_tool_calls: Some(200),
                    max_wall_time_minutes: None,
                    max_parallel_tools: None,
                    max_identical_no_progress: None,
                }),
            },
            ..ConfigFile::default()
        };
        // workspace 只定义 max_parallel_tools：home 的其他 limits 字段必须保留。
        let workspace = ConfigFile {
            agent: AgentFile {
                allow_outside_workspace: None,
                limits: Some(AgentLimitsFile {
                    max_model_turns: None,
                    max_tool_calls: None,
                    max_wall_time_minutes: None,
                    max_parallel_tools: Some(2),
                    max_identical_no_progress: None,
                }),
            },
            ..ConfigFile::default()
        };
        let merged = merge(home, workspace);
        let limits = merged.agent.limits.expect("limits present");
        assert_eq!(
            limits.max_model_turns,
            Some(100),
            "home 的 max_model_turns 不得被 workspace 的整块覆盖丢弃"
        );
        assert_eq!(limits.max_tool_calls, Some(200));
        assert_eq!(limits.max_parallel_tools, Some(2));
    }

    #[test]
    fn shell_and_context_merge_field_level() {
        let home = ConfigFile {
            shell: ShellFile {
                kind: Some("git-bash".into()),
                path: Some(r"C:\git\bash.exe".into()),
            },
            context: ContextFile {
                safety_reserve_tokens: Some(4096),
            },
            ..ConfigFile::default()
        };
        let workspace = ConfigFile {
            shell: ShellFile {
                kind: None,
                path: Some(r"D:\bash.exe".into()),
            },
            ..ConfigFile::default()
        };
        let merged = merge(home, workspace);
        assert_eq!(merged.shell.kind.as_deref(), Some("git-bash"));
        assert_eq!(merged.shell.path.as_deref(), Some(r"D:\bash.exe"));
        assert_eq!(
            merged.context.safety_reserve_tokens,
            Some(4096),
            "workspace 未设置 context 时 home 的值必须保留"
        );
    }
}
