//! MCP Server 配置（README2 §8：不写死源码，从 `~/.tpi/config.toml` 读）。
//!
//! ```toml
//! [mcp.servers.bevy]
//! command = "bevy_brp_mcp"
//! args = []
//! enabled = true
//! timeout_ms = 30000
//!
//! [mcp.servers.example.env]
//! FOO = "bar"
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

/// 单个 MCP Server 配置（README2 §8）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub enabled: bool,
    pub timeout: Duration,
}

impl McpServerConfig {
    /// 默认调用超时（README2 §7：timeout 是必须项）。
    pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
}

/// TOML 反序列化中间层（`[mcp.servers.<name>]`）。
#[derive(Debug, Deserialize)]
#[serde(default)]
struct TomlFile {
    mcp: TomlMcp,
}

#[derive(Debug, Default, Deserialize)]
struct TomlMcp {
    servers: HashMap<String, TomlServer>,
}

#[derive(Debug, Default, Deserialize)]
struct TomlServer {
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
    enabled: Option<bool>,
    timeout_ms: Option<u64>,
}

impl Default for TomlFile {
    fn default() -> Self {
        Self {
            mcp: TomlMcp::default(),
        }
    }
}

/// 从 `~/.tpi/config.toml` 读取 MCP servers（无配置 → 空）。
pub fn load(tpi_home: &Path) -> Vec<McpServerConfig> {
    load_from_path(&tpi_home.join("config.toml"))
}

/// 从指定 TOML 文件读取（测试用）。
pub fn load_from_path(path: &Path) -> Vec<McpServerConfig> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let file: TomlFile = match toml::from_str(&content) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "MCP 配置解析失败");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for (name, server) in file.mcp.servers {
        let Some(command) = server.command.filter(|c| !c.is_empty()) else {
            tracing::warn!(server = %name, "MCP server 缺 command，跳过");
            continue;
        };
        out.push(McpServerConfig {
            name,
            command,
            args: server.args.unwrap_or_default(),
            env: server.env.unwrap_or_default(),
            enabled: server.enabled.unwrap_or(true),
            timeout: Duration::from_millis(
                server.timeout_ms.unwrap_or(McpServerConfig::DEFAULT_TIMEOUT_MS).max(1),
            ),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 只返回 enabled 的 server。
pub fn load_enabled(tpi_home: &Path) -> Vec<McpServerConfig> {
    load(tpi_home)
        .into_iter()
        .filter(|server| server.enabled)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_servers_with_env_and_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[mcp.servers.bevy]
command = "bevy_brp_mcp"
args = ["--port", "1234"]
enabled = true
timeout_ms = 5000

[mcp.servers.example.env]
FOO = "bar"
"#,
        )
        .unwrap();
        let servers = load_from_path(&path);
        assert_eq!(servers.len(), 1, "example 缺 command 应跳过");
        let bevy = &servers[0];
        assert_eq!(bevy.name, "bevy");
        assert_eq!(bevy.command, "bevy_brp_mcp");
        assert_eq!(bevy.args, vec!["--port", "1234"]);
        assert!(bevy.enabled);
        assert_eq!(bevy.timeout, Duration::from_millis(5000));
    }

    #[test]
    fn disabled_server_filtered_by_load_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[mcp.servers.a]
command = "cmd-a"

[mcp.servers.b]
command = "cmd-b"
enabled = false
"#,
        )
        .unwrap();
        let all = load_from_path(&path);
        assert_eq!(all.len(), 2);
        let enabled = all.iter().filter(|s| s.enabled).count();
        assert_eq!(enabled, 1);
    }

    #[test]
    fn missing_file_returns_empty() {
        assert!(load_from_path(Path::new("/nonexistent/config.toml")).is_empty());
    }
}
