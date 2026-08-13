//! 环境诊断（P2：`tpi doctor` / `/doctor`）。
//!
//! 检查项集中在单个函数，CLI 与 TUI 共用同一份报告，
//! 避免两处维护不同的检查逻辑。

use camino::Utf8PathBuf;
use std::io::Write;

/// 单项检查结果。
#[derive(Debug, Clone)]
pub struct DoctorCheck {
    /// 检查项名称。
    pub name: &'static str,
    /// 是否通过。
    pub ok: bool,
    /// 说明（通过时描述，失败时给下一步动作）。
    pub detail: String,
}

/// 运行全部环境检查。
pub fn doctor_report(workspace_root: &Utf8PathBuf) -> Vec<DoctorCheck> {
    doctor_report_with_home(workspace_root, &crate::config::tpi_home())
}

fn doctor_report_with_home(
    workspace_root: &Utf8PathBuf,
    home: &std::path::Path,
) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();

    // 1. 配置与模型。
    let config_path = home.join("config.toml");
    let workspace_config_path = workspace_root.join(".tpi").join("config.toml");
    let config_exists = config_path.exists() || workspace_config_path.exists();
    let loaded_config = crate::config::load_from_home(workspace_root, None, home);
    let model_configured = loaded_config
        .as_ref()
        .map(|config| !config.model.name.is_empty())
        .unwrap_or(false);
    checks.push(DoctorCheck {
        name: "config",
        ok: config_exists,
        detail: if config_exists {
            let active_path = if workspace_config_path.exists() {
                workspace_config_path.as_std_path()
            } else {
                &config_path
            };
            format!("{} 存在", active_path.display())
        } else {
            format!(
                "{} 与 {} 均不存在（运行 `tpi init` 生成用户配置）",
                config_path.display(),
                workspace_config_path
            )
        },
    });
    checks.push(DoctorCheck {
        name: "model",
        ok: model_configured,
        detail: if model_configured {
            "模型配置可用".into()
        } else {
            loaded_config
                .as_ref()
                .err()
                .cloned()
                .unwrap_or_else(|| "未配置 [model.primary]（provider/name/base_url 必填）".into())
        },
    });

    // 2. API key（环境变量或 keyring）。
    let api_key_env = loaded_config
        .as_ref()
        .ok()
        .map(|config| config.model.api_key_env.as_str())
        .unwrap_or("TPI_API_KEY");
    let api_key_ok = loaded_config
        .as_ref()
        .ok()
        .and_then(|config| crate::config::read_api_key(config).ok())
        .is_some();
    checks.push(DoctorCheck {
        name: "api_key",
        ok: api_key_ok,
        detail: if api_key_ok {
            "API key 可读取（环境变量或凭据管理器）".into()
        } else {
            format!("未找到 API key：设置 {api_key_env} 或用 `tpi auth set <provider>`")
        },
    });

    // 3. Git Bash（bash 是唯一命令执行通道）。
    // §PointerHit 10：优先用配置的 shell.path（locate_git_bash 第一优先级）。
    let configured_shell = loaded_config
        .as_ref()
        .ok()
        .and_then(|config| config.shell_path.clone());
    let local = crate::workspace::LocalWorkspace::new(workspace_root.clone(), true);
    let ctx = crate::tool::ToolContext {
        workspace_root: workspace_root.clone(),
        allow_outside_workspace: true,
        cancel: tokio_util::sync::CancellationToken::new(),
        artifacts_root: home.join("artifacts"),
        session_id: "doctor".into(),
        call_id: crate::ids::ToolCallId::new_v7(),
        output_tx: None,
        scan_snapshots: std::sync::Arc::new(
            std::sync::Mutex::new(std::collections::HashMap::new()),
        ),
        shell_path: configured_shell.clone(),
        snapshot_store: std::sync::Arc::new(std::sync::Mutex::new(
            crate::tool::edit::SnapshotStore::new(64, 8),
        )),
        current_plan: std::sync::Arc::new(std::sync::Mutex::new(None)),
        shell: local.shell.clone(),
        workspace: std::sync::Arc::new(std::sync::Mutex::new(
            crate::workspace::ActiveWorkspace::local(local),
        )),
        interactive: false,
    };
    let bash = crate::tool::command::locate_git_bash(&ctx);
    checks.push(DoctorCheck {
        name: "git_bash",
        ok: bash.is_some(),
        detail: match bash {
            Some(path) => {
                let source = if configured_shell.is_some() {
                    "（配置的 shell.path）"
                } else {
                    "（自动探测）"
                };
                format!("{path} {source}")
            }
            None => "未找到 Git Bash（运行 scripts/install-bash.ps1 或配置 shell.path）".into(),
        },
    });

    // 4. 目录。
    for (name, dir) in [
        ("sessions", home.join("sessions")),
        ("artifacts", home.join("artifacts")),
        ("logs", home.join("logs")),
    ] {
        let ok = probe_directory_writable(&dir).is_ok();
        checks.push(DoctorCheck {
            name,
            ok,
            detail: if ok {
                format!("{} 可写", dir.display())
            } else {
                format!("{} 不可用", dir.display())
            },
        });
    }

    // 4.5 控制台 UTF-8（Windows 中文系统 GBK 代码页会显示乱码）。
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::GetConsoleOutputCP;
        // SAFETY: GetConsoleOutputCP has no pointer arguments or caller-side preconditions.
        let cp = unsafe { GetConsoleOutputCP() };
        checks.push(DoctorCheck {
            name: "console",
            ok: cp == 65001,
            detail: if cp == 65001 {
                "控制台输出代码页 = UTF-8 (65001)".into()
            } else {
                format!("控制台输出代码页 = {cp}（中文系统可能显示乱码，重启后 TPI 会自动切换）")
            },
        });
    }

    // 5. workspace 可写。
    let ws_writable = probe_directory_writable(workspace_root.as_std_path()).is_ok();
    checks.push(DoctorCheck {
        name: "workspace",
        ok: ws_writable,
        detail: if ws_writable {
            "workspace 可写".into()
        } else {
            "workspace 不可写（检查权限）".into()
        },
    });

    // 6. 关键键位（P0-5）：submit/insert_newline/escape 必须各有至少一个绑定。
    // from_config 已做兜底（缺失回退默认键），这里展示当前生效绑定供排查。
    let keymap = loaded_config
        .as_ref()
        .ok()
        .map(|config| config.ui_keymap.clone())
        .unwrap_or_else(crate::tui::keymap::Keymap::builtin);
    use crate::tui::keymap::KeyAction;
    let critical = [
        ("submit", KeyAction::Submit),
        ("insert_newline", KeyAction::InsertNewline),
        ("escape", KeyAction::Escape),
    ];
    let all_present = critical
        .iter()
        .all(|(_, action)| keymap.has_action(*action));
    let detail = critical
        .iter()
        .map(|(name, action)| format!("{name}: {}", keymap.keys_for(*action)))
        .collect::<Vec<_>>()
        .join(" · ");
    checks.push(DoctorCheck {
        name: "keymap",
        ok: all_present,
        detail: if all_present {
            detail
        } else {
            format!("{detail}（关键动作缺失，已回退默认键）")
        },
    });

    // 7. session 完整性（P0-2）：中间坏行会导致 resume 失败；报告坏行位置。
    let workspace_id = crate::session::workspace_id_for(workspace_root.as_std_path());
    let session_dir = home.join("sessions").join(&workspace_id);
    let mut corrupted: Vec<(String, usize, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&session_dir) {
        let mut files: Vec<std::path::PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "jsonl").unwrap_or(false))
            .collect();
        files.sort();
        for path in files {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if uuid::Uuid::parse_str(stem).is_err() {
                continue; // 附属文件（.bak-* / .quarantine）非 session 本体。
            }
            if let Ok(bad) = crate::session::repair::diagnose(&path) {
                for line in bad {
                    corrupted.push((stem.to_string(), line.line, line.reason));
                }
            }
        }
    }
    checks.push(DoctorCheck {
        name: "session_integrity",
        ok: corrupted.is_empty(),
        detail: if corrupted.is_empty() {
            "当前 workspace 的 session 文件全部健康".into()
        } else {
            let mut detail = format!("{} 个坏行（`tpi sessions repair` 修复）:", corrupted.len());
            for (id, line, reason) in corrupted.iter().take(5) {
                detail.push_str(&format!("\n  {id} L{line}: {reason}"));
            }
            if corrupted.len() > 5 {
                detail.push_str(&format!("\n  …等 {} 条", corrupted.len()));
            }
            detail
        },
    });

    checks
}

fn probe_directory_writable(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for _ in 0..4 {
        let probe = dir.join(format!(".tpi-write-probe-{}", uuid::Uuid::now_v7()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(mut file) => {
                let write_result = file.write_all(b"probe");
                drop(file);
                let cleanup_result = std::fs::remove_file(&probe);
                write_result?;
                cleanup_result?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique write probe",
    ))
}

/// 报告渲染（CLI 与 /doctor 共用）。
pub fn render_report(workspace_root: &Utf8PathBuf) -> String {
    let mut out = String::from("TPI 环境检查\n");
    for check in doctor_report(workspace_root) {
        let mark = if check.ok { "✓" } else { "✗" };
        out.push_str(&format!("{mark} {}：{}\n", check.name, check.detail));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_probe_never_clobbers_legacy_probe_name() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join(".doctor-probe");
        std::fs::write(&sentinel, "user data").unwrap();

        probe_directory_writable(dir.path()).unwrap();

        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "user data");
    }

    #[test]
    fn workspace_only_config_is_reported_as_present() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = Utf8PathBuf::from_path_buf(root.path().join("workspace")).unwrap();
        std::fs::create_dir_all(workspace.join(".tpi")).unwrap();
        std::fs::write(
            workspace.join(".tpi/config.toml"),
            "[model.primary]\nprovider = \"test\"\nname = \"m\"\nbase_url = \"https://example.invalid/v1\"\n",
        )
        .unwrap();

        let report = doctor_report_with_home(&workspace, &home);
        let config = report.iter().find(|check| check.name == "config").unwrap();
        let model = report.iter().find(|check| check.name == "model").unwrap();
        assert!(config.ok, "{config:?}");
        assert!(model.ok, "{model:?}");
        assert!(config.detail.contains("workspace"), "{}", config.detail);
    }

    /// P0-2：doctor 报告损坏 session 的坏行位置（不含附属文件）。
    #[test]
    fn doctor_reports_corrupted_session_lines() {
        use crate::ids::RunId;
        use crate::session::{SessionEvent, SessionLog};
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = Utf8PathBuf::from_path_buf(root.path().join("workspace")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let sessions_root = home.join("sessions");
        std::fs::create_dir_all(&sessions_root).unwrap();
        let workspace_id = crate::session::workspace_id_for(workspace.as_std_path());
        let session_id = crate::ids::SessionId::new_v7();
        let path = sessions_root
            .join(&workspace_id)
            .join(format!("{session_id}.jsonl"));
        let mut log = SessionLog::create_with_id(
            &sessions_root,
            workspace.as_std_path(),
            RunId::new_v7(),
            session_id,
        )
        .unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "a".into(),
        })
        .unwrap();
        drop(log);
        let mut raw = std::fs::read(&path).unwrap();
        raw.extend_from_slice(b"broken-line\n");
        std::fs::write(&path, raw).unwrap();

        let report = doctor_report_with_home(&workspace, &home);
        let sessions = report
            .iter()
            .find(|check| check.name == "session_integrity")
            .unwrap();
        assert!(!sessions.ok, "损坏 session 必须报错: {sessions:?}");
        assert!(
            sessions.detail.contains("tpi sessions repair"),
            "{}",
            sessions.detail
        );
        assert!(
            sessions.detail.contains("L2"),
            "应报告坏行行号: {}",
            sessions.detail
        );
    }
}
