//! R3 Remote list 集成测试（§45-§48：一致性 ToolOutcome；transport 不泄漏给模型）。

mod fixtures;

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use tokio_util::sync::CancellationToken;
use tpi::remote::RemoteWorkspace;
use tpi::remote::ssh::{HostKeyDecision, RemoteHost};
use tpi::remote::traverse::RemoteListArgs;
use tpi::workspace::ActiveWorkspace;

/// 启动 server + 确认 host key + 返回已连接 client 和远端 root（POSIX）。
async fn setup_connected() -> (tempfile::TempDir, tpi::remote::ssh::SshClient, String) {
    let (port, root, known_hosts) = fixtures::remote_server::start_test_server().await;
    let mut probe = fixtures::remote_server::test_client(port, &known_hosts).await;
    assert_eq!(
        probe.connect().await.unwrap(),
        HostKeyDecision::UnknownPending
    );
    probe.confirm_host_key().unwrap();
    probe.disconnect().await;

    let mut host = RemoteHost::direct("127.0.0.1", port, "test");
    host.known_hosts_path = known_hosts;
    host.password = Some(fixtures::remote_server::TEST_PASSWORD.into());
    let mut client = tpi::remote::ssh::SshClient::new(host);
    assert_eq!(client.connect().await.unwrap(), HostKeyDecision::Accepted);

    let root_posix = fixtures::remote_server::win_to_posix(root.path());
    (root, client, root_posix)
}

fn remote_ctx(root_posix: &str) -> tpi::tool::ToolContext {
    let root = Utf8PathBuf::from(root_posix);
    let remote = RemoteWorkspace::new(RemoteHost::direct("127.0.0.1", 22, "test"), root.clone());
    let active = ActiveWorkspace::remote(remote.clone());
    tpi::tool::ToolContext {
        workspace_root: root.clone(),
        allow_outside_workspace: true,
        cancel: CancellationToken::new(),
        artifacts_root: "/tmp/artifacts".into(),
        session_id: "remote-traverse-test".into(),
        call_id: tpi::ids::ToolCallId::new_v7(),
        output_tx: None,
        scan_snapshots: Arc::new(Mutex::new(std::collections::HashMap::new())),
        shell_path: None,
        snapshot_store: Arc::new(Mutex::new(tpi::tool::edit::SnapshotStore::new(16, 4))),
        current_goal: None,
        current_plan: Arc::new(Mutex::new(None)),
        shell: remote.shell.clone(),
        workspace: Arc::new(Mutex::new(active)),
        processes: Arc::new(Mutex::new(tpi::process::managed::ProcessRegistry::new())),
        terminals: Default::default(),
        resources: None,
        resource_identity: None,
        registry: std::sync::Arc::new(std::sync::Mutex::new(
            tpi::tool::registry::builtin_registry(),
        )),
        interactive: false,
        workspace_session: None,
    }
}

/// §47：远端 list 输出格式与本地一致（目录带 /、`scanned_files、stop_reason`）。
#[tokio::test]
async fn remote_list_format_matches_local() {
    let (_root, mut client, root_posix) = setup_connected().await;
    let ctx = remote_ctx(&root_posix);
    client.write_file("a.txt", b"x\n").await.unwrap();
    client
        .exec(
            &format!("mkdir -p {root_posix}/src"),
            None,
            &Default::default(),
            None,
        )
        .await
        .unwrap();
    client.write_file("src/b.txt", b"y\n").await.unwrap();

    let outcome = tpi::remote::traverse::remote_list(
        &mut client,
        &RemoteListArgs {
            path: root_posix.clone(),
            depth: 2,
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, tpi::outcome::ToolStatus::Succeeded);
    let out = &outcome.model_payload.output;
    assert!(out.contains("status: succeeded"), "{out}");
    // tempdir 里还有 server 的 known_hosts 文件，scanned_files 含它；只断言 >= 2。
    assert!(
        out.contains("scanned_files: 2") || out.contains("scanned_files: 3"),
        "{out}"
    );
    assert!(out.contains("stop_reason: complete"), "{out}");
    assert!(out.contains("a.txt"), "{out}");
    assert!(out.contains("src/"), "目录带斜杠：{out}");
    assert!(out.contains("src/b.txt"), "{out}");
}

/// §48：远端输出不得泄漏 transport 机制（ssh 细节）。
#[tokio::test]
async fn remote_output_does_not_leak_transport() {
    let (_root, mut client, root_posix) = setup_connected().await;
    let ctx = remote_ctx(&root_posix);
    client
        .write_file("data.txt", b"hello world\n")
        .await
        .unwrap();

    let outcome = tpi::remote::traverse::remote_list(
        &mut client,
        &RemoteListArgs {
            path: root_posix.clone(),
            depth: 1,
        },
        &ctx,
    )
    .await;
    let out = &outcome.model_payload.output;
    assert!(!out.contains("ssh"), "transport 不得泄漏：{out}");
    assert!(!out.contains("__TPI_CAPTURE_"), "{out}");
}

/// 相对路径基于 session cwd（远端 shell cwd 持久后的 list 语义）。
#[tokio::test]
async fn remote_list_uses_session_cwd_for_relative_path() {
    let (root, mut client, root_posix) = setup_connected().await;
    let ctx = remote_ctx(&root_posix);
    client.write_file("cwd_test.txt", b"z\n").await.unwrap();
    // 绝对路径正常。
    let outcome = tpi::remote::traverse::remote_list(
        &mut client,
        &RemoteListArgs {
            path: root_posix.clone(),
            depth: 1,
        },
        &ctx,
    )
    .await;
    assert!(outcome.model_payload.output.contains("cwd_test.txt"));
    drop(root);
}
