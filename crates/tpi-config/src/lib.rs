//! TPI 配置层（P7-02 拆 crate：Config/ModelConfig/AgentConfig + 凭据 auth）。

pub mod auth;
pub mod config;

pub use auth::{auth_clear, auth_get, auth_set};
pub use config::{
    AgentConfig, Config, LimitsConfig, ModelConfig, StorageConfig, ToolPolicy, UiConfig, load,
    load_from_home, read_api_key, read_api_key_for, set_ui_theme, test_config, tpi_home,
};
