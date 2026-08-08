//! 安全边界契约：workspace 路径沙箱、artifact 引用与 web SSRF。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::test_tool_context;
use tpi::tool::files::{ReadArgs, WriteArgs, read, write};
use tpi::tool::outcome::ToolStatus;
use tpi::tool::web::{WebFetchArgs, validate_fetch_url, web_fetch};
use tpi::tool::{resolve_workspace_path, validate_artifact_component};

/// 串行化依赖全局 allow_private 测试开关的两个 web_fetch 测试（并行互相覆盖会失败/死锁）。
static WEB_FETCH_TESTS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// §9.1：workspace 内 junction/symlink 指向外部时，写入不得穿过链接逃逸。
///
/// 目标路径不存在时 `canonicalize` 会失败；必须解析最近存在的祖先再判定。
#[cfg(windows)]
#[test]
fn write_through_workspace_junction_is_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let junction = workspace.join("link");
    // 创建指向 workspace 外部的 junction（无需管理员权限）。
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(outside.path())
        .status()
        .expect("mklink runs");
    assert!(status.success(), "mklink /J 创建 junction 失败");

    let mut ctx = test_tool_context(&workspace);
    ctx.allow_outside_workspace = false; // 本测试验证严格模式
    // 目标文件不存在：canonicalize 失败时必须解析最近存在祖先（junction 本身）。
    // write 需要 commit plan（write-ahead 契约），临时文件也落在 link 目录内。
    let target = workspace.join("link").join("escaped.txt");
    let plan = tpi::tool::edit::prepare_commit(&target);
    let outcome = write(
        WriteArgs {
            path: "link/escaped.txt".into(),
            content: "x".into(),
            revision: None,
        },
        &ctx,
        Some(&plan),
    );
    assert_eq!(
        outcome.status,
        ToolStatus::Rejected,
        "通过 junction 写穿 workspace 必须被拒绝: {}",
        outcome.model_text()
    );
    assert!(
        !outside.path().join("escaped.txt").exists(),
        "外部目录不得被写入"
    );
}

#[test]
fn resolve_workspace_path_rejects_parent_escape() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    assert!(resolve_workspace_path(&workspace, "../outside.txt").is_err());
}

#[test]
fn resolve_workspace_path_rejects_absolute_outside_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    #[cfg(windows)]
    let outside = r"C:\Windows\System32\drivers\etc\hosts".to_string();
    #[cfg(not(windows))]
    let outside = "/etc/passwd".to_string();
    assert!(resolve_workspace_path(&workspace, &outside).is_err());
}

#[test]
fn resolve_workspace_path_allows_relative_inside_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let resolved = resolve_workspace_path(&workspace, "src/main.rs").unwrap();
    assert!(resolved.starts_with(&workspace));
}

#[test]
fn read_rejects_path_outside_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut ctx = test_tool_context(&workspace);
    ctx.allow_outside_workspace = false; // 本测试验证严格模式
    let outcome = read(
        ReadArgs {
            path: "../secret.txt".into(),
            start_line: 1,
            line_count: 10,
        },
        &ctx,
    );
    assert_eq!(outcome.status, ToolStatus::Rejected);
}

#[test]
fn read_rejects_artifact_path_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut ctx = test_tool_context(&workspace);
    ctx.allow_outside_workspace = false; // 本测试验证严格模式
    let outcome = read(
        ReadArgs {
            path: "@artifact/../evil/id".into(),
            start_line: 1,
            line_count: 10,
        },
        &ctx,
    );
    assert_eq!(outcome.status, ToolStatus::Rejected);
}

#[test]
fn validate_artifact_component_rejects_separators() {
    assert!(!validate_artifact_component("../session"));
    assert!(!validate_artifact_component("bad/id"));
    assert!(validate_artifact_component("valid-id_01"));
}

#[test]
fn write_rejects_outside_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut ctx = test_tool_context(&workspace);
    ctx.allow_outside_workspace = false; // 本测试验证严格模式
    let outcome = write(
        WriteArgs {
            path: "../../escape.txt".into(),
            content: "x".into(),
            revision: None,
        },
        &ctx,
        None,
    );
    assert_eq!(outcome.status, ToolStatus::Rejected);
}

#[tokio::test]
async fn web_fetch_blocks_private_targets_by_default() {
    let _guard = WEB_FETCH_TESTS_LOCK.lock().await;
    assert!(validate_fetch_url("http://127.0.0.1:8080/").is_err());
    assert!(validate_fetch_url("http://192.168.0.1/").is_err());
}

#[tokio::test]
async fn web_fetch_localhost_is_blocked_without_test_override() {
    let _guard = WEB_FETCH_TESTS_LOCK.lock().await;
    tpi::tool::web::set_allow_private_web_targets_for_tests(false);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut ctx = test_tool_context(&workspace);
    ctx.allow_outside_workspace = false; // 本测试验证严格模式
    let outcome = web_fetch(
        WebFetchArgs {
            url: "http://127.0.0.1:1/".into(),
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Failed);
    assert!(outcome.model_text().contains("ssrf_blocked"));
}

#[tokio::test]
async fn web_fetch_allows_localhost_when_test_override_enabled() {
    let _guard = WEB_FETCH_TESTS_LOCK.lock().await;
    tpi::tool::web::set_allow_private_web_targets_for_tests(true);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut ctx = test_tool_context(&workspace);
    ctx.allow_outside_workspace = false; // 本测试验证严格模式
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let body = "<html><head><title>Test Page</title></head><body><h1>Hello TPI</h1></body></html>";
    let handle = std::thread::spawn(move || {
        // 非阻塞轮询 + 超时：即使请求被 SSRF 拦截/失败也返回，避免 accept 永久阻塞。
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    use std::io::{Read, Write};
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() > deadline {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    });
    let outcome = web_fetch(
        WebFetchArgs {
            url: format!("http://{addr}/page"),
        },
        &ctx,
    )
    .await;
    handle.join().unwrap();
    tpi::tool::web::set_allow_private_web_targets_for_tests(false);
    assert_eq!(outcome.status, ToolStatus::Succeeded);
    assert!(outcome.model_text().contains("Hello TPI"));
}

#[test]
fn scheduler_locks_use_resolved_paths() {
    use tpi::agent::scheduler::{AccessMode, FileScope, ResourceId, ToolAccess, tool_access};
    use tpi::tool::{BuiltinTool, ValidatedArgs, edit::EditArgs};

    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().join("project")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/a.rs"), "fn main() {}\n").unwrap();

    let access_a = tool_access(
        BuiltinTool::Edit,
        &ValidatedArgs::Edit(EditArgs {
            path: "src/a.rs".into(),
            revision: String::new(),
            replacements: Vec::new(),
        }),
        &workspace,
    );
    let access_b = tool_access(
        BuiltinTool::Edit,
        &ValidatedArgs::Edit(EditArgs {
            path: "./src/a.rs".into(),
            revision: String::new(),
            replacements: Vec::new(),
        }),
        &workspace,
    );

    let lock_a = match access_a {
        ToolAccess::Resources(locks) => locks,
        _ => panic!("expected resource lock"),
    };
    let lock_b = match access_b {
        ToolAccess::Resources(locks) => locks,
        _ => panic!("expected resource lock"),
    };
    assert_eq!(lock_a, lock_b);
    assert!(matches!(
        lock_a[0].resource,
        ResourceId::File(FileScope::Exact(_))
    ));
    assert_eq!(lock_a[0].mode, AccessMode::Write);
}

/// §9.1 自由模式（allow_outside_workspace=true，默认）：read 允许 workspace 外绝对路径。
#[test]
fn read_allows_outside_absolute_path_when_freedom_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().join("project")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, "外部文件内容\n").unwrap();
    let mut ctx = test_tool_context(&workspace);
    ctx.allow_outside_workspace = true; // 默认即 true；显式声明

    let outcome = read(
        ReadArgs {
            path: outside.to_string_lossy().into_owned(),
            start_line: 1,
            line_count: 10,
        },
        &ctx,
    );
    assert_eq!(outcome.status, ToolStatus::Succeeded);
    assert!(
        outcome.model_text().contains("外部文件内容"),
        "自由模式下必须能读取 workspace 外文件: {}",
        outcome.model_text()
    );
}

/// §9.1 自由模式：resolve_tool_path 对 workspace 外绝对路径返回 Ok。
#[test]
fn resolve_tool_path_accepts_outside_when_freedom_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().join("project")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let mut ctx = test_tool_context(&workspace);
    ctx.allow_outside_workspace = true;
    let outside = dir.path().join("elsewhere/x.txt");
    let resolved = tpi::tool::resolve_tool_path(&ctx, &outside.to_string_lossy()).unwrap();
    assert_eq!(resolved.as_std_path(), outside.as_path());

    // 严格模式仍拒绝。
    ctx.allow_outside_workspace = false;
    assert!(tpi::tool::resolve_tool_path(&ctx, &outside.to_string_lossy()).is_err());
}
