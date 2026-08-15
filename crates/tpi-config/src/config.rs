//! 用户与 workspace 配置的加载和合并。
//!
//! 优先级（§18.1）：CLI 参数 → workspace `.tpi/config.toml` → `~/.tpi/config.toml` → 内建默认值。
//! 不允许"看不见的默认模型"：模型配置缺失时明确报错。

use camino::Utf8PathBuf;
use serde::Deserialize;

const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_INSTRUCTION_BYTES: usize = 1024 * 1024;

/// 持久化 `[ui] theme` 到用户配置（~/.tpi/config.toml；不存在则创建），
/// 保留其它配置项。返回写入路径。
///
/// 注意：主题是用户级偏好，总是写 home 配置。若 workspace `.tpi/config.toml`
/// 也配置了 `[ui] theme`（优先级更高），下次启动仍以 workspace 为准——
/// 菜单内已提示此限制。
pub fn set_ui_theme(theme: &str) -> Result<std::path::PathBuf, String> {
    set_ui_theme_at(&tpi_home(), theme)
}

/// 以指定配置根目录写入（测试隔离用；公开入口 [`set_ui_theme`]）。
pub(crate) fn set_ui_theme_at(
    home: &std::path::Path,
    theme: &str,
) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(home).map_err(|e| format!("创建配置目录失败: {e}"))?;
    let path = home.join("config.toml");
    let mut value: toml::Value = match std::fs::read_to_string(&path) {
        Ok(raw) => {
            toml::from_str(&raw).map_err(|e| format!("解析配置失败（{}）: {e}", path.display()))?
        }
        Err(_) => toml::Value::Table(toml::Table::new()),
    };
    let table = value
        .as_table_mut()
        .ok_or_else(|| "配置根必须是 table".to_string())?;
    let ui = table
        .entry("ui".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let ui = ui
        .as_table_mut()
        .ok_or_else(|| "配置 [ui] 必须是 table".to_string())?;
    ui.insert("theme".to_string(), toml::Value::String(theme.to_string()));
    let out = toml::to_string(&value).map_err(|e| format!("序列化配置失败: {e}"))?;
    std::fs::write(&path, out).map_err(|e| format!("写入配置失败（{}）: {e}", path.display()))?;
    Ok(path)
}

/// 配置根目录（~/.tpi，§14.1）。
/// P7-02 拆 crate：实现下沉 tpi-core（tpi_core::util::tpi_home）；此处 re-export
/// 保持 `crate::config::tpi_home` 路径兼容。
pub use tpi_core::util::tpi_home;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct UiFile {
    /// 主题名：omp / dark / light（未知值回退 omp）。
    pub theme: Option<String>,
    /// 视口模式：fullscreen（默认）/ inline（兼容模式；§1.2）。
    pub mode: Option<String>,
    /// 键位覆盖表：`{ action = "ctrl+enter" }` 或 `{ action = ["k", "ctrl+p"] }`。
    /// 未配置的动作保持内建默认；workspace 的 keymap 逐 key 覆盖 home。
    pub keymap: Option<toml::Table>,
    /// 卡片折叠时显示的正文行数（§用户诉求：thinking/工具卡片统一）；
    /// 默认 0 = 折叠态只显示主行摘要，不显示正文。配置如 `collapsed_lines = 10`。
    pub collapsed_lines: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct ContextFile {
    /// §15.4 safety reserve（默认 8192；compaction 触发阈值用）。
    pub safety_reserve_tokens: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct ShellFile {
    /// 预留的 shell 类型（如 `git-bash`）。**尚未接线**：运行时只消费 `path`
    /// （未配置时自动查找 Git Bash），设置 `kind` 不会改变行为（ISSUE-020）。
    pub kind: Option<String>,
    /// 显式 Git Bash 路径（§11.2 解析顺序第 1 位）。
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct ModelFile {
    /// 默认模型（`tpi --model` 未指定时使用；向后兼容）。
    pub primary: Option<PrimaryModelFile>,
    /// 多模型列表（P8：`tpi --model <name>` 从 primary + profiles 中选择）。
    /// workspace 的 profiles 整表覆盖 home 的 profiles（不逐项合并）。
    #[serde(default)]
    pub profiles: Vec<PrimaryModelFile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimaryModelFile {
    pub provider: String,
    pub name: String,
    pub base_url: String,
    pub reasoning: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub context_window: Option<u64>,
    /// 从环境变量读取 API key 的变量名（默认 TPI_API_KEY）。
    pub api_key_env: Option<String>,
    /// 直接在配置文件保存的 API key（§用户需求：不用系统变量；
    /// 优先级：环境变量 > 配置文件 api_key > 凭据管理器）。
    /// 注意：明文存 key，配置文件请勿提交到版本库（建议 chmod 600）。
    pub api_key: Option<String>,
    /// 输入/输出单价（每百万 token，美元；§16.2 花费展示，可选）。
    #[serde(default)]
    pub price_input: Option<f64>,
    #[serde(default)]
    pub price_output: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct AgentFile {
    pub limits: Option<AgentLimitsFile>,
    /// 是否允许文件工具访问 workspace 外的绝对路径（§9.1 自由模式；
    /// 默认 true——个人工具以 AI 自由优先；false 恢复严格 workspace 沙箱）。
    pub allow_outside_workspace: Option<bool>,
}

/// [agent.limits]：运行护栏（§用户诉求：默认全部 0 = 不限制，按需配置）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentLimitsFile {
    /// 模型回合数上限；0 = 不限制（默认）。
    pub max_model_turns: Option<u32>,
    /// 工具调用总数上限；0 = 不限制（默认）。
    pub max_tool_calls: Option<u32>,
    /// 墙钟时间上限（分钟）；0 = 不限制（默认，不启动 watchdog）。
    pub max_wall_time_minutes: Option<u64>,
    /// 同 wave 并行工具数；必须 > 0（默认 4）。
    pub max_parallel_tools: Option<u32>,
    /// 无进展重复检测阈值；0 = 关闭检测（默认）。
    pub max_identical_no_progress: Option<u32>,
}

/// 生效配置（含来源标注，§18.1：`/settings` 可查看来源）。
#[derive(Debug, Clone)]
pub struct Config {
    pub model: ModelConfig,
    /// 全部可用模型（primary 第一 + profiles；`tpi --model <name>` 从中选择）。
    pub models: Vec<ModelConfig>,
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
    /// §16.3 [ui] theme：omp / dark / light / opencode / onedarkpro；默认
    /// **onedarkpro**（无 `[ui]` 配置时的实际值；未知主题回退 omp）。
    pub ui_theme: String,
    /// §1 [ui] mode：fullscreen（默认）/ inline（兼容模式）。
    pub ui_mode: tpi_ui_types::ViewMode,
    /// §成熟化 [ui] keymap：动作 → 按键覆盖（未配置动作保持内建默认）。
    pub ui_keymap: tpi_ui_types::Keymap,
    /// §用户诉求 [ui] collapsed_lines：卡片折叠时显示的正文行数（thinking/工具
    /// 卡片统一）；默认 0 = 折叠态只显示主行摘要。配置如 `collapsed_lines = 10`。
    pub ui_collapsed_lines: usize,
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
    /// 配置文件直存的 API key（None = 走环境变量/凭据管理器）。
    pub api_key: Option<String>,
    /// 输入/输出单价（每百万 token，美元；None = 不显示花费）。
    pub price_input: Option<f64>,
    pub price_output: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
pub struct LimitsConfig {
    /// 模型回合数上限；0 = 不限制（§用户诉求：默认不限制，仅护栏用）。
    pub max_model_turns: u32,
    /// 工具调用总数上限；0 = 不限制。
    pub max_tool_calls: u32,
    /// 墙钟时间上限（分钟）；0 = 不限制（不启动 watchdog）。
    pub max_wall_time_minutes: u64,
    /// 默认 4（M4 scheduler 使用；并行度是性能参数不是护栏，保留默认）。
    pub max_parallel_tools: u32,
    /// 无进展重复检测阈值；0 = 关闭检测（§用户诉求：默认不限制）。
    pub max_identical_no_progress: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            // §用户诉求：默认不限制——护栏交给用户按需配置（对齐 OpenCode/
            // Claude Code：交互模式默认无硬上限，限制是防失控逃生舱）。
            max_model_turns: 0,
            max_tool_calls: 0,
            max_wall_time_minutes: 0,
            max_parallel_tools: 4,
            max_identical_no_progress: 0,
        }
    }
}

// ---- P1-05：resolved config 的窄视图（每个 owner 只读自己的设置）----
//
// `Config` 保留为 composition resolver 的总输出（字段级 merge / unknown
// rejection / default snapshot 不变）；以下窄视图是从 `Config` 投影的
// domain-specific resolved views，供各 owner 接收，避免“单个大 config 在
// 各层透传、每个组件读到不属于自己的设置”（audit Medium-5）。
// 新代码优先接收窄视图；`Config` 只应在 composition root（main/app 组装）出现。

/// Agent 运行所需配置（model + 护栏 + context 预算 + 指令）。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: ModelConfig,
    pub limits: LimitsConfig,
    pub safety_reserve_tokens: u64,
    pub system_prompt_extra: Option<String>,
    pub workspace_root: Utf8PathBuf,
}

/// 工具执行策略（权限/路径/网络/web 摘要）。
#[derive(Debug, Clone)]
pub struct ToolPolicy {
    pub allow_outside_workspace: bool,
    pub shell_path: Option<Utf8PathBuf>,
    pub artifacts_root: std::path::PathBuf,
    pub sessions_root: std::path::PathBuf,
    pub auto_open_browser: bool,
    pub web_summary_model: String,
}

/// TUI 展示配置（theme/mode/keymap/折叠行数）。
#[derive(Debug, Clone)]
pub struct UiConfig {
    pub theme: String,
    pub mode: tpi_ui_types::ViewMode,
    pub keymap: tpi_ui_types::Keymap,
    pub collapsed_lines: usize,
}

/// 存储路径（workspace/session/artifact）。
#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub workspace_root: Utf8PathBuf,
    pub sessions_root: std::path::PathBuf,
    pub artifacts_root: std::path::PathBuf,
}

impl Config {
    /// Agent 视图：agent/context 层只读这一份（不触 UI/tool 策略）。
    pub fn agent_config(&self) -> AgentConfig {
        AgentConfig {
            model: self.model.clone(),
            limits: self.limits,
            safety_reserve_tokens: self.safety_reserve_tokens,
            system_prompt_extra: self.system_prompt_extra.clone(),
            workspace_root: self.workspace_root.clone(),
        }
    }

    /// 工具策略视图：ToolContext/工具执行只读这一份。
    pub fn tool_policy(&self) -> ToolPolicy {
        ToolPolicy {
            allow_outside_workspace: self.allow_outside_workspace,
            shell_path: self.shell_path.clone(),
            artifacts_root: self.artifacts_root.clone(),
            sessions_root: self.sessions_root.clone(),
            auto_open_browser: self.auto_open_browser,
            web_summary_model: self.web_summary_model.clone(),
        }
    }

    /// TUI 视图：渲染/键位/主题只读这一份。
    pub fn ui_config(&self) -> UiConfig {
        UiConfig {
            theme: self.ui_theme.clone(),
            mode: self.ui_mode,
            keymap: self.ui_keymap.clone(),
            collapsed_lines: self.ui_collapsed_lines,
        }
    }

    /// 存储视图：session/artifact 路径。
    pub fn storage_config(&self) -> StorageConfig {
        StorageConfig {
            workspace_root: self.workspace_root.clone(),
            sessions_root: self.sessions_root.clone(),
            artifacts_root: self.artifacts_root.clone(),
        }
    }
}

/// 测试辅助：最小可用 Config（不读真实配置；agent 测试构造 fake config 用）。
/// P7-02 拆 crate：主 crate 的 agent 测试也需要它，故 pub + 无 cfg(test)。
pub fn test_config(workspace_root: &Utf8PathBuf) -> Config {
    Config {
        model: ModelConfig {
            provider: "test".into(),
            name: "fake-model".into(),
            base_url: "https://example.invalid/v1".into(),
            reasoning: None,
            max_output_tokens: None,
            context_window: None,
            api_key_env: "TPI_TEST_API_KEY".into(),
            api_key: None,
            price_input: None,
            price_output: None,
        },
        models: Vec::new(),
        limits: LimitsConfig::default(),
        workspace_root: workspace_root.clone(),
        sessions_root: std::path::PathBuf::from(".tpi-test-sessions"),
        artifacts_root: std::path::PathBuf::from(".tpi-test-artifacts"),
        shell_path: None,
        safety_reserve_tokens: 8192,
        ui_mode: tpi_ui_types::ViewMode::default(),
        ui_keymap: tpi_ui_types::Keymap::builtin(),
        ui_collapsed_lines: 10,
        auto_open_browser: false,
        web_summary_model: "none".into(),
        system_prompt_extra: None,
        source: "test".into(),
        ui_theme: "omp".into(),
        allow_outside_workspace: true,
    }
}

/// 加载配置：合并 ~/.tpi/config.toml 与 workspace .tpi/config.toml。
///
/// 模型配置缺失时返回明确错误（§18.1：不允许看不见的默认模型）。
pub fn load(workspace_root: &Utf8PathBuf, cli_model: Option<&str>) -> Result<Config, String> {
    let home = tpi_home();
    load_from_home(workspace_root, cli_model, &home)
}

/// P7-02 拆 crate：doctor（主 crate）调用，改 pub。
pub fn load_from_home(
    workspace_root: &Utf8PathBuf,
    cli_model: Option<&str>,
    home: &std::path::Path,
) -> Result<Config, String> {
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

    // P8 多模型：primary + profiles 构成可用列表（按 name 去重，primary 优先）。
    let mut models: Vec<PrimaryModelFile> = Vec::new();
    {
        let mut seen = std::collections::BTreeSet::new();
        for m in std::iter::once(primary.clone()).chain(merged.model.profiles.clone()) {
            if seen.insert(m.name.clone()) {
                models.push(m);
            }
        }
    }
    // `--model <name>` 从列表选择；未指定用 primary。
    let selected = match cli_model {
        Some(cli) => models
            .iter()
            .find(|m| m.name == cli)
            .cloned()
            .ok_or_else(|| {
                let available = models
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("--model {cli} 未找到；可用模型：{available}")
            })?,
        None => primary,
    };

    let name = selected.name.clone();
    validate_model(&selected, &name)?;
    let source = if cli_model.is_some() {
        "cli --model".to_string()
    } else if workspace_has_model {
        format!("workspace {workspace_file}")
    } else if home_has_model {
        format!("user {}", home_file.display())
    } else {
        "builtin defaults".to_string()
    };

    let system_prompt_extra = read_system_md(&home.join("SYSTEM.md"))?;
    // P1-12：workspace 项目规则（AGENTS.md）——叠加在个人全局规则之后，
    // 注入时标明来源（§18.2：项目级约束进入 system prompt）。
    let system_prompt_extra = match read_system_md(workspace_root.join("AGENTS.md").as_std_path())?
    {
        Some(project_rules) => Some(match system_prompt_extra {
            Some(global) => format!("{global}\n\n[project rule: AGENTS.md]\n{project_rules}"),
            None => format!("[project rule: AGENTS.md]\n{project_rules}"),
        }),
        None => system_prompt_extra,
    };

    let limits = LimitsConfig {
        // §用户诉求：默认不限制（0）——护栏按需配置；与 LimitsConfig::default()
        // 一致，避免两处默认值漂移。
        max_model_turns: merged
            .agent
            .limits
            .as_ref()
            .and_then(|l| l.max_model_turns)
            .unwrap_or(0),
        max_tool_calls: merged
            .agent
            .limits
            .as_ref()
            .and_then(|l| l.max_tool_calls)
            .unwrap_or(0),
        max_wall_time_minutes: merged
            .agent
            .limits
            .as_ref()
            .and_then(|l| l.max_wall_time_minutes)
            .unwrap_or(0),
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
            .unwrap_or(0),
    };
    validate_limits(&limits)?;
    let shell_path = merged.shell.path.as_deref().map(Utf8PathBuf::from);
    if shell_path
        .as_ref()
        .is_some_and(|path| path.as_str().trim().is_empty())
    {
        return Err("shell.path 不能为空".into());
    }

    let safety_reserve_tokens = merged.context.safety_reserve_tokens.unwrap_or(8192);
    if let Some(context_window) = selected.context_window
        && tpi_core::revision::usable_input(
            context_window,
            u64::from(selected.max_output_tokens.unwrap_or(0)),
            safety_reserve_tokens,
        ) == 0
    {
        return Err(
            "context_window 必须大于 max_output_tokens 与 safety_reserve_tokens 之和".into(),
        );
    }

    // 全部模型转 ModelConfig（含 api_key；供 /settings 与选择展示）。
    let models: Vec<ModelConfig> = models
        .into_iter()
        .map(|m| ModelConfig {
            provider: m.provider,
            name: m.name,
            base_url: m.base_url,
            reasoning: m.reasoning,
            max_output_tokens: m.max_output_tokens,
            context_window: m.context_window,
            api_key_env: m.api_key_env.unwrap_or_else(|| "TPI_API_KEY".into()),
            api_key: m.api_key,
            price_input: m.price_input,
            price_output: m.price_output,
        })
        .collect();

    Ok(Config {
        model: ModelConfig {
            provider: selected.provider,
            name,
            base_url: selected.base_url,
            reasoning: selected.reasoning,
            max_output_tokens: selected.max_output_tokens,
            context_window: selected.context_window,
            api_key_env: selected.api_key_env.unwrap_or_else(|| "TPI_API_KEY".into()),
            api_key: selected.api_key,
            price_input: selected.price_input,
            price_output: selected.price_output,
        },
        models,
        limits,
        workspace_root: workspace_root.clone(),
        sessions_root: home.join("sessions"),
        artifacts_root: home.join("artifacts"),
        shell_path,
        safety_reserve_tokens,
        auto_open_browser: false,
        web_summary_model: "none".into(),
        system_prompt_extra,
        source,
        ui_theme: merged
            .ui
            .theme
            .clone()
            .unwrap_or_else(|| "onedarkpro".to_string()),
        ui_collapsed_lines: merged.ui.collapsed_lines.unwrap_or(0),
        ui_mode: merged
            .ui
            .mode
            .as_deref()
            .map(tpi_ui_types::ViewMode::parse)
            .unwrap_or_default(),
        ui_keymap: tpi_ui_types::Keymap::from_config(&merged.ui.keymap.unwrap_or_default()),
        allow_outside_workspace: merged.agent.allow_outside_workspace.unwrap_or(true),
    })
}

fn read_config(path: &std::path::Path) -> Result<ConfigFile, String> {
    if !path.exists() {
        return Ok(ConfigFile::default());
    }
    let text = tpi_core::util::read_utf8_file_bounded(path, MAX_CONFIG_BYTES)
        .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("解析 {} 失败: {e}", path.display()))
}

fn merge(home: ConfigFile, workspace: ConfigFile) -> ConfigFile {
    ConfigFile {
        model: ModelFile {
            primary: workspace.model.primary.or(home.model.primary),
            // profiles 整表覆盖（workspace 优先；不逐项合并——模型列表是
            // 声明式的，混搭两层会产生歧义）。
            profiles: if workspace.model.profiles.is_empty() {
                home.model.profiles
            } else {
                workspace.model.profiles
            },
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
            keymap: merge_keymap(home.ui.keymap, workspace.ui.keymap),
            collapsed_lines: workspace.ui.collapsed_lines.or(home.ui.collapsed_lines),
        },
    }
}

/// keymap 逐 key 合并（workspace 覆盖 home；字段级合并哲学 §18.1）。
fn merge_keymap(home: Option<toml::Table>, workspace: Option<toml::Table>) -> Option<toml::Table> {
    let mut out = home.unwrap_or_default();
    if let Some(ws) = workspace {
        for (key, value) in ws {
            out.insert(key, value);
        }
    }
    (!out.is_empty()).then_some(out)
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

fn validate_model(primary: &PrimaryModelFile, effective_name: &str) -> Result<(), String> {
    if primary.provider.trim().is_empty() {
        return Err("model.primary.provider 不能为空".into());
    }
    if effective_name.trim().is_empty() {
        return Err("model.primary.name 不能为空".into());
    }
    let url = reqwest::Url::parse(&primary.base_url)
        .map_err(|error| format!("model.primary.base_url 无效: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("model.primary.base_url 只支持 http 或 https".into());
    }
    if primary
        .api_key_env
        .as_deref()
        .is_some_and(|name| name.trim().is_empty() || name.contains('='))
    {
        return Err("model.primary.api_key_env 不是有效的环境变量名".into());
    }
    // api_key 直存：只做基本健全性（有值且无换行/空白包裹）。
    if primary.api_key.as_deref().is_some_and(|key| {
        let trimmed = key.trim();
        trimmed.is_empty() || trimmed != key || key.contains('\n') || key.contains('\r')
    }) {
        return Err("model.api_key 不能为空或含换行（请去掉首尾空白）".into());
    }
    if primary.max_output_tokens == Some(0) {
        return Err("model.primary.max_output_tokens 必须大于 0".into());
    }
    if primary.context_window == Some(0) {
        return Err("model.primary.context_window 必须大于 0".into());
    }
    if let (Some(output), Some(context)) = (primary.max_output_tokens, primary.context_window)
        && u64::from(output) > context
    {
        return Err("model.primary.max_output_tokens 不能大于 context_window".into());
    }
    for (name, price) in [
        ("price_input", primary.price_input),
        ("price_output", primary.price_output),
    ] {
        if price.is_some_and(|value| !value.is_finite() || value < 0.0) {
            return Err(format!("model.primary.{name} 必须是非负有限数"));
        }
    }
    Ok(())
}

fn validate_limits(limits: &LimitsConfig) -> Result<(), String> {
    // §用户诉求：0 = 不限制（默认）——max_model_turns/max_wall_time_minutes/
    // max_identical_no_progress/max_tool_calls 都为 0 合法；仅并行度必须 > 0
    // （性能参数，0 会导致 wave 构建异常）。
    if limits.max_wall_time_minutes > u64::MAX / 60 {
        return Err("agent.limits.max_wall_time_minutes 过大".into());
    }
    if limits.max_parallel_tools == 0 {
        return Err("agent.limits.max_parallel_tools 必须大于 0".into());
    }
    Ok(())
}

fn read_system_md(path: &std::path::Path) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content = tpi_core::util::read_utf8_file_bounded(path, MAX_INSTRUCTION_BYTES)
        .map_err(|error| format!("读取指令文件 {} 失败: {error}", path.display()))?;
    Ok((!content.trim().is_empty()).then_some(content))
}

/// 读取 API key（§18.4：环境变量显式覆盖；keyring 属 M6）。
pub fn read_api_key(config: &Config) -> Result<String, String> {
    read_api_key_for(&config.model)
}

/// 对指定模型配置读 API key（P8：TUI /model 切换时对目标模型读取）。
/// 优先级——环境变量（显式覆盖）> 配置文件 api_key > Windows Credential Manager。
pub fn read_api_key_for(model: &ModelConfig) -> Result<String, String> {
    if let Ok(key) = std::env::var(&model.api_key_env)
        && !key.is_empty()
    {
        return Ok(key);
    }
    if let Some(key) = &model.api_key
        && !key.trim().is_empty()
    {
        return Ok(key.clone());
    }
    if let Some(key) = crate::auth_get(&model.provider)? {
        return Ok(key);
    }
    Err(format!(
        "未找到 API key：请在配置文件的 model.api_key、环境变量 {} 中设置，或运行 `tpi auth set {}` 写入凭据（§18.4）",
        model.api_key_env, model.provider
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    /// P8：多模型——profiles 列表构建；`--model <name>` 从列表选择。
    #[test]
    fn multi_model_profiles_and_cli_selection() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            r#"
[model.primary]
provider = "openai"
name = "gpt-4o"
base_url = "https://api.openai.com/v1"

[[model.profiles]]
provider = "openai"
name = "gpt-4o-mini"
base_url = "https://api.openai.com/v1"

[[model.profiles]]
provider = "anthropic"
name = "claude-sonnet"
base_url = "https://api.anthropic.com/v1"
"#,
        )
        .unwrap();
        let ws = Utf8PathBuf::from_path_buf(dir.path().join("ws")).unwrap();
        std::fs::create_dir_all(ws.join(".tpi")).unwrap();
        // 默认：primary。
        let cfg = load_from_home(&ws, None, &home).unwrap();
        assert_eq!(cfg.model.name, "gpt-4o", "默认选中 primary");
        assert_eq!(cfg.models.len(), 3, "primary + 2 profiles");
        // --model 选 profile。
        let cfg = load_from_home(&ws, Some("claude-sonnet"), &home).unwrap();
        assert_eq!(cfg.model.name, "claude-sonnet", "--model 选择 profile");
        assert_eq!(cfg.model.provider, "anthropic");
        // --model 未找到：报错列出可用模型。
        let err = load_from_home(&ws, Some("nope"), &home).unwrap_err();
        assert!(err.contains("nope 未找到"), "{err}");
        assert!(err.contains("gpt-4o"), "错误信息列出可用模型: {err}");
    }

    /// P8：API key 直存配置文件——读取优先级 env > 配置 api_key > 凭据。
    #[test]
    fn api_key_from_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            r#"
[model.primary]
provider = "openai"
name = "gpt-4o"
base_url = "https://api.openai.com/v1"
api_key = "sk-config-file-key"
"#,
        )
        .unwrap();
        let ws = Utf8PathBuf::from_path_buf(dir.path().join("ws")).unwrap();
        std::fs::create_dir_all(ws.join(".tpi")).unwrap();
        let cfg = load_from_home(&ws, None, &home).unwrap();
        assert_eq!(cfg.model.api_key.as_deref(), Some("sk-config-file-key"));
        // read_api_key 从配置读取。
        assert_eq!(read_api_key(&cfg).unwrap(), "sk-config-file-key");
    }

    /// P8：api_key 校验——空/带换行/首尾空白拒绝。
    #[test]
    fn api_key_validation() {
        let base = PrimaryModelFile {
            provider: "openai".into(),
            name: "gpt-4o".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: None,
            max_output_tokens: None,
            context_window: None,
            api_key_env: None,
            api_key: Some(" sk-with-space".into()),
            price_input: None,
            price_output: None,
        };
        assert!(validate_model(&base, "gpt-4o").is_err(), "首尾空白拒绝");
        let mut ok = base;
        ok.api_key = Some("sk-valid".into());
        assert!(validate_model(&ok, "gpt-4o").is_ok());
    }

    /// §用户诉求：默认不限制——护栏字段默认全 0，且 validate_limits 接受
    /// （仅 max_parallel_tools 必须 > 0）。
    #[test]
    fn limits_default_to_unlimited_and_validate_accepts_zero() {
        let limits = LimitsConfig::default();
        assert_eq!(limits.max_model_turns, 0, "回合数默认不限制");
        assert_eq!(limits.max_tool_calls, 0, "工具数默认不限制");
        assert_eq!(limits.max_wall_time_minutes, 0, "墙钟默认不限制");
        assert_eq!(limits.max_identical_no_progress, 0, "无进展检测默认关闭");
        assert_eq!(limits.max_parallel_tools, 4, "并行度保留默认（性能参数）");
        assert!(validate_limits(&limits).is_ok(), "全 0 配置必须合法");
        // 并行度 0 仍非法（性能参数不能为 0）。
        let mut bad = limits;
        bad.max_parallel_tools = 0;
        assert!(validate_limits(&bad).is_err());
    }

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
        let config = load_from_home(&workspace, None, &home).expect("load");
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

    #[test]
    fn load_rejects_unknown_fields_and_invalid_runtime_values() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().join("workspace")).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let write_config = |text: &str| std::fs::write(home.join("config.toml"), text).unwrap();
        write_config(
            "[model.primary]\nprovider = \"p\"\nname = \"m\"\nbase_url = \"https://example.invalid\"\ntypo_field = true\n",
        );
        assert!(
            load_from_home(&workspace, None, &home)
                .unwrap_err()
                .contains("unknown field")
        );

        write_config(
            "[model.primary]\nprovider = \"p\"\nname = \"m\"\nbase_url = \"file:///tmp/model\"\n",
        );
        assert!(
            load_from_home(&workspace, None, &home)
                .unwrap_err()
                .contains("http")
        );

        write_config(
            "[model.primary]\nprovider = \"p\"\nname = \"m\"\nbase_url = \"https://example.invalid\"\n\n[agent.limits]\nmax_parallel_tools = 0\n",
        );
        assert!(
            load_from_home(&workspace, None, &home)
                .unwrap_err()
                .contains("max_parallel_tools")
        );

        write_config(
            "[model.primary]\nprovider = \"p\"\nname = \"m\"\nbase_url = \"https://example.invalid\"\nmax_output_tokens = 500\ncontext_window = 1000\n\n[context]\nsafety_reserve_tokens = 500\n",
        );
        assert!(
            load_from_home(&workspace, None, &home)
                .unwrap_err()
                .contains("safety_reserve_tokens")
        );
    }

    #[test]
    fn config_and_instruction_files_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let oversized_config = dir.path().join("config.toml");
        std::fs::File::create(&oversized_config)
            .unwrap()
            .set_len((MAX_CONFIG_BYTES + 1) as u64)
            .unwrap();
        assert!(read_config(&oversized_config).unwrap_err().contains("上限"));

        let oversized_rules = dir.path().join("SYSTEM.md");
        std::fs::File::create(&oversized_rules)
            .unwrap()
            .set_len((MAX_INSTRUCTION_BYTES + 1) as u64)
            .unwrap();
        assert!(
            read_system_md(&oversized_rules)
                .unwrap_err()
                .contains("上限")
        );
    }

    #[test]
    fn cli_model_override_cannot_be_empty() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().join("workspace")).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "[model.primary]\nprovider = \"p\"\nname = \"m\"\nbase_url = \"https://example.invalid\"\n",
        )
        .unwrap();

        assert!(load_from_home(&workspace, Some(""), &home).is_err());
    }

    /// /theme 菜单持久化：写 [ui] theme 到 home 配置，保留其它字段。
    #[test]
    fn set_ui_theme_creates_and_updates_config() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        // 1. 配置不存在：创建并写入 [ui] theme。
        let path = set_ui_theme_at(&home, "onedarkpro").unwrap();
        assert_eq!(path, home.join("config.toml"));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("theme = \"onedarkpro\""), "raw: {raw}");

        // 2. 已存在其它配置：更新 theme，保留 model 等其它字段。
        std::fs::write(
            &path,
            "[model.primary]\nprovider = \"p\"\nname = \"m\"\nbase_url = \"https://x\"\n[ui]\ncollapsed_lines = 5\n",
        )
        .unwrap();
        set_ui_theme_at(&home, "light").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("theme = \"light\""), "raw: {raw}");
        assert!(raw.contains("provider = \"p\""), "其它字段必须保留: {raw}");
        assert!(
            raw.contains("collapsed_lines = 5"),
            "ui 其它字段必须保留: {raw}"
        );

        // 3. 写入结果可被 load 解析。
        let workspace = Utf8PathBuf::from_path_buf(dir.path().join("workspace")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let cfg = load_from_home(&workspace, None, &home).unwrap();
        assert_eq!(cfg.ui_theme, "light");
    }

    /// P1-05：resolved 窄视图是 Config 的投影——default snapshot 不变
    /// （各视图字段与 Config 字段逐一一致），字段级 merge / unknown rejection
    /// 由既有测试（load_rejects_unknown_fields 等）保持。
    #[test]
    fn resolved_views_match_config_fields() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().join("workspace")).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "[model.primary]\nprovider = \"p\"\nname = \"m\"\nbase_url = \"https://example.invalid\"\nprice_input = 0.5\n\n[agent.limits]\nmax_model_turns = 10\nmax_parallel_tools = 2\n\n[ui]\ntheme = \"dark\"\ncollapsed_lines = 5\n",
        )
        .unwrap();
        let cfg = load_from_home(&workspace, None, &home).unwrap();

        // Agent 视图：model/limits/context 预算/指令/workspace。
        let agent = cfg.agent_config();
        assert_eq!(agent.model.name, cfg.model.name);
        assert_eq!(agent.limits.max_model_turns, cfg.limits.max_model_turns);
        assert_eq!(
            agent.limits.max_parallel_tools,
            cfg.limits.max_parallel_tools
        );
        assert_eq!(agent.safety_reserve_tokens, cfg.safety_reserve_tokens);
        assert_eq!(agent.system_prompt_extra, cfg.system_prompt_extra);
        assert_eq!(agent.workspace_root, cfg.workspace_root);

        // 工具策略视图：权限/路径/网络。
        let policy = cfg.tool_policy();
        assert_eq!(policy.allow_outside_workspace, cfg.allow_outside_workspace);
        assert_eq!(policy.shell_path, cfg.shell_path);
        assert_eq!(policy.artifacts_root, cfg.artifacts_root);
        assert_eq!(policy.sessions_root, cfg.sessions_root);
        assert_eq!(policy.auto_open_browser, cfg.auto_open_browser);
        assert_eq!(policy.web_summary_model, cfg.web_summary_model);

        // TUI 视图。
        let ui = cfg.ui_config();
        assert_eq!(ui.theme, cfg.ui_theme);
        assert_eq!(ui.mode, cfg.ui_mode);
        assert_eq!(ui.keymap, cfg.ui_keymap);
        assert_eq!(ui.collapsed_lines, cfg.ui_collapsed_lines);

        // 存储视图。
        let storage = cfg.storage_config();
        assert_eq!(storage.workspace_root, cfg.workspace_root);
        assert_eq!(storage.sessions_root, cfg.sessions_root);
        assert_eq!(storage.artifacts_root, cfg.artifacts_root);
    }
}
