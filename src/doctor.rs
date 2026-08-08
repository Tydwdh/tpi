//! 环境诊断（P2：`tpi doctor` / `/doctor`）。
//!
//! 检查项集中在单个函数，CLI 与 TUI 共用同一份报告，
//! 避免两处维护不同的检查逻辑。

use camino::Utf8PathBuf;

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
    let mut checks = Vec::new();

    // 1. 配置与模型。
    let home = crate::config::tpi_home();
    let config_path = home.join("config.toml");
    let config_exists = config_path.exists();
    let model_configured = crate::config::load(workspace_root, None)
        .map(|config| {
            let has_primary = !config.model.name.is_empty();
            (true, has_primary)
        })
        .unwrap_or((false, false));
    checks.push(DoctorCheck {
        name: "config",
        ok: config_exists,
        detail: if config_exists {
            format!("{} 存在", config_path.display())
        } else {
            format!("{} 不存在（运行 `tpi init` 生成）", config_path.display())
        },
    });
    checks.push(DoctorCheck {
        name: "model",
        ok: model_configured.1,
        detail: if model_configured.0 {
            "模型配置可用".into()
        } else {
            "未配置 [model.primary]（provider/name/base_url 必填）".into()
        },
    });

    // 2. API key（环境变量或 keyring）。
    let api_key_ok = crate::config::load(workspace_root, None)
        .and_then(|config| crate::config::read_api_key(&config))
        .is_ok();
    checks.push(DoctorCheck {
        name: "api_key",
        ok: api_key_ok,
        detail: if api_key_ok {
            "API key 可读取（环境变量或凭据管理器）".into()
        } else {
            "未找到 API key：设置 TPI_API_KEY 或用 `tpi auth set <provider>`".into()
        },
    });

    // 3. Git Bash（bash 是唯一命令执行通道）。
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
        shell_path: None,
        snapshot_store: std::sync::Arc::new(std::sync::Mutex::new(
            crate::tool::edit::SnapshotStore::new(64, 8),
        )),
        current_plan: std::sync::Arc::new(std::sync::Mutex::new(None)),
        interactive: false,
    };
    let bash = crate::tool::command::locate_git_bash(&ctx);
    checks.push(DoctorCheck {
        name: "git_bash",
        ok: bash.is_some(),
        detail: match bash {
            Some(path) => path.to_string(),
            None => "未找到 Git Bash（运行 scripts/install-bash.ps1 或配置 shell.path）".into(),
        },
    });

    // 4. 目录。
    for (name, dir) in [
        ("sessions", home.join("sessions")),
        ("artifacts", home.join("artifacts")),
        ("logs", home.join("logs")),
    ] {
        let ok = dir.exists() || std::fs::create_dir_all(&dir).is_ok();
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

    // 5. workspace 可写。
    let ws_writable = std::fs::File::create(workspace_root.join(".doctor-probe"))
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"x"))
        .map(|_| std::fs::remove_file(workspace_root.join(".doctor-probe")).is_ok())
        .unwrap_or(false);
    checks.push(DoctorCheck {
        name: "workspace",
        ok: ws_writable,
        detail: if ws_writable {
            "workspace 可写".into()
        } else {
            "workspace 不可写（检查权限）".into()
        },
    });

    checks
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
