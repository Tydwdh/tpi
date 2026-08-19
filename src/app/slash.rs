//! P3-03：Slash command registry——typed command definitions（单一来源）。
//!
//! - help / completion / dispatch 都来自同一 [`SLASH_COMMANDS`] snapshot；
//! - [`SlashCommandSpec`] 含 name/desc/dangerous（危险命令确认 policy 由
//!   dispatcher 保留；本阶段 registration 静态，不开放第三方）；
//! - dispatch（`command_from_slash`）从 P3-01 移入，与 registry 同源；
//! - golden：registry name 集合与 `handle_slash_command` 分支一致（测试强制）。

use crate::app::intent::AppCommand;

/// 一条 slash 命令定义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommandSpec {
    pub name: &'static str,
    pub desc: &'static str,
    /// 危险命令（quit/new/compact/retry/cancel 等）需确认 policy。
    pub dangerous: bool,
}

#[must_use]
pub const fn spec(name: &'static str, desc: &'static str, dangerous: bool) -> SlashCommandSpec {
    SlashCommandSpec {
        name,
        desc,
        dangerous,
    }
}

/// 单一来源 registry（顺序即帮助/补全顺序）。
/// “/ + 回车”默认选中第一项：首项必须是安全命令（help）。
pub const SLASH_COMMANDS: &[SlashCommandSpec] = &[
    spec("help", "显示帮助与快捷键", false),
    spec("settings", "查看生效配置及来源", false),
    spec("model", "查看/切换模型（primary + profiles）", false),
    spec("session", "查看会话与成本", false),
    spec("sessions", "浏览并恢复历史会话", false),
    spec("theme", "切换主题（UI + 代码高亮）", false),
    spec("new", "开始新会话", true),
    spec("cancel", "取消当前 run", false),
    spec("thinking", "查看推理设置", false),
    spec("diff", "查看本轮全部文件 diff", false),
    spec("doctor", "环境检查（config/模型/API key/Git Bash）", false),
    spec("compact", "手动压缩上下文", true),
    spec("retry", "重试上一次失败/中断的 turn", true),
    spec("quit", "退出 TPI", true),
];

/// 命令名 → 描述（help 渲染与补全共用）。
#[must_use]
pub fn help_lines() -> Vec<(&'static str, &'static str)> {
    SLASH_COMMANDS.iter().map(|s| (s.name, s.desc)).collect()
}

/// 是否登记的命令（补全过滤用）。
#[must_use]
pub fn is_registered(name: &str) -> bool {
    SLASH_COMMANDS.iter().any(|s| s.name == name)
}

/// P3-01 adapter（移入 registry 同源）：slash 命令文本 → 语义 `AppCommand`。
#[must_use]
pub fn command_from_slash(message: &str) -> Option<AppCommand> {
    let msg = message.trim();
    match msg {
        "/quit" | "/exit" => Some(AppCommand::Quit),
        "/cancel" => Some(AppCommand::CancelRun),
        "/new" => Some(AppCommand::StartNewSession),
        "/compact" => Some(AppCommand::CompactNow),
        "/retry" => Some(AppCommand::RetryLast),
        "/mcp" if msg.starts_with("/mcp") => Some(AppCommand::OpenModal { name: "mcp".into() }),
        "/settings" => Some(AppCommand::OpenModal {
            name: "settings".into(),
        }),
        "/model" => Some(AppCommand::OpenModal {
            name: "model".into(),
        }),
        "/help" => Some(AppCommand::OpenModal {
            name: "help".into(),
        }),
        "/session" => Some(AppCommand::OpenModal {
            name: "session".into(),
        }),
        "/sessions" => Some(AppCommand::OpenModal {
            name: "sessions".into(),
        }),
        "/theme" => Some(AppCommand::OpenModal {
            name: "theme".into(),
        }),
        "/diff" => Some(AppCommand::OpenModal {
            name: "diff".into(),
        }),
        "/doctor" => Some(AppCommand::OpenModal {
            name: "doctor".into(),
        }),
        "/thinking" => Some(AppCommand::OpenModal {
            name: "thinking".into(),
        }),
        _ => None, // 非 slash 命令（普通消息）
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dispatch 覆盖 = registry 覆盖（golden：无孤儿命令，无未登记分支）。
    #[test]
    fn dispatch_covers_registry() {
        for spec in SLASH_COMMANDS {
            let mapped = command_from_slash(&format!("/{}", spec.name));
            assert!(
                mapped.is_some(),
                "registry 命令 /{} 必须可 dispatch",
                spec.name
            );
        }
        // 未登记命令不 dispatch（普通消息）。
        assert_eq!(command_from_slash("/definitely-not-registered"), None);
    }

    /// registry 无重复名。
    #[test]
    fn registry_names_are_unique() {
        let mut names: Vec<&str> = SLASH_COMMANDS.iter().map(|s| s.name).collect();
        let n = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), n, "registry 不允许重复 name");
    }

    /// 首项必须安全（help），避免“/ + 回车”误退出。
    #[test]
    fn first_command_is_safe() {
        assert_eq!(SLASH_COMMANDS[0].name, "help");
        assert!(!SLASH_COMMANDS[0].dangerous);
    }
}
