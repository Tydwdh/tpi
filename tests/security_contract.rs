//! 安全边界契约：workspace 路径沙箱、artifact 引用与 web SSRF。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::test_tool_context;
use tpi::tool::files::{ReadArgs, WriteArgs, read, write};
use tpi::tool::outcome::ToolStatus;
use tpi::tool::web::{WebFetchArgs, validate_fetch_url, web_fetch};
use tpi::tool::{resolve_workspace_path, validate_artifact_component};

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
    let ctx = test_tool_context(&workspace);
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
    let ctx = test_tool_context(&workspace);
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
    let ctx = test_tool_context(&workspace);
    let outcome = write(
        WriteArgs {
            path: "../../escape.txt".into(),
            content: "x".into(),
        },
        &ctx,
        None,
    );
    assert_eq!(outcome.status, ToolStatus::Rejected);
}

#[test]
fn web_fetch_blocks_private_targets_by_default() {
    assert!(validate_fetch_url("http://127.0.0.1:8080/").is_err());
    assert!(validate_fetch_url("http://192.168.0.1/").is_err());
}

#[tokio::test]
async fn web_fetch_localhost_is_blocked_without_test_override() {
    tpi::tool::web::set_allow_private_web_targets_for_tests(false);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = test_tool_context(&workspace);
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
    tpi::tool::web::set_allow_private_web_targets_for_tests(true);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = test_tool_context(&workspace);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let body = "<html><head><title>Test Page</title></head><body><h1>Hello TPI</h1></body></html>";
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        use std::io::{Read, Write};
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
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
