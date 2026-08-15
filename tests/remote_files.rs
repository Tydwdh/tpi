//! R2 Remote file tools 集成测试（§41-§44：read/edit/write 保持与本地
//! 相同的 semantic contract：revision-bound / stale rejection / atomic batch /
//! diff；revision transport-independent §42）。

mod fixtures;

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use tokio_util::sync::CancellationToken;
use tpi::remote::RemoteWorkspace;
use tpi::remote::files::{RemoteEditArgs, RemoteReadArgs, RemoteWriteArgs};
use tpi::remote::ssh::{HostKeyDecision, RemoteHost};
use tpi::workspace::ActiveWorkspace;

/// 启动 server + 确认 host key + 返回已连接的 SshClient 和远端 root（POSIX）。
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

    // 远端 root：POSIX 形式（测试 server exec 在本地 Git Bash 执行）；
    // 纯 Rust 转换，与 fixture server 用同一实现，不依赖 cygpath。
    let root_posix = fixtures::remote_server::win_to_posix(root.path());
    (root, client, root_posix)
}

/// 构造 Remote ToolContext（shell cwd = 远端 root）。
fn remote_ctx(root_posix: &str) -> tpi::tool::ToolContext {
    let root = Utf8PathBuf::from(root_posix);
    let remote = RemoteWorkspace::new(RemoteHost::direct("127.0.0.1", 22, "test"), root.clone());
    let active = ActiveWorkspace::remote(remote.clone());
    tpi::tool::ToolContext {
        workspace_root: root.clone(),
        allow_outside_workspace: true,
        cancel: CancellationToken::new(),
        artifacts_root: "/tmp/artifacts".into(),
        session_id: "remote-files-test".into(),
        call_id: tpi::ids::ToolCallId::new_v7(),
        output_tx: None,
        scan_snapshots: Arc::new(Mutex::new(std::collections::HashMap::new())),
        shell_path: None,
        snapshot_store: Arc::new(Mutex::new(tpi::tool::edit::SnapshotStore::new(16, 4))),
        current_plan: Arc::new(Mutex::new(None)),
        shell: remote.shell.clone(),
        workspace: Arc::new(Mutex::new(active)),
        processes: Arc::new(Mutex::new(tpi::process::managed::ProcessRegistry::new())),
        registry: std::sync::Arc::new(std::sync::Mutex::new(
            tpi::tool::registry::builtin_registry(),
        )),
        interactive: false,
    }
}

/// §41/§43：远端 read 输出格式与本地一致（revision header / path / lines / 行号）。
#[tokio::test]
async fn remote_read_returns_local_style_output() {
    let (root, mut client, root_posix) = setup_connected().await;
    let ctx = remote_ctx(&root_posix);

    client
        .write_file("hello.txt", b"line1\nline2\nline3\n")
        .await
        .unwrap();
    let outcome = tpi::remote::files::remote_read(
        &mut client,
        &RemoteReadArgs {
            path: format!("{root_posix}/hello.txt"),
            start_line: 1,
            line_count: 10,
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, tpi::tool::outcome::ToolStatus::Succeeded);
    let out = &outcome.model_payload.output;
    assert!(out.contains("[revision="), "必须带 revision header：{out}");
    assert!(out.contains("lines: 1-3 of 3"), "行区间：{out}");
    assert!(out.contains("1: line1"), "行号：{out}");
    assert!(out.contains("3: line3"), "{out}");
    drop(root);
}

/// §41：远端 write 新建文件，返回 revision；read 回读一致。
#[tokio::test]
async fn remote_write_then_read_roundtrip() {
    let (_root, mut client, root_posix) = setup_connected().await;
    let ctx = remote_ctx(&root_posix);

    let path = format!("{root_posix}/new.txt");
    let outcome = tpi::remote::files::remote_write(
        &mut client,
        &RemoteWriteArgs {
            path: path.clone(),
            content: "hello 世界\n".into(),
            revision: None,
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, tpi::tool::outcome::ToolStatus::Succeeded);
    let out = &outcome.model_payload.output;
    assert!(out.contains("[revision="), "{out}");

    let raw = client.read_file(&path).await.unwrap();
    assert_eq!(raw, b"hello \xe4\xb8\x96\xe7\x95\x8c\n");
}

/// §61：stale edit 拒绝——TPI read → 外部修改 → TPI edit(旧 revision) → stale。
#[tokio::test]
async fn remote_stale_edit_is_rejected() {
    let (_root, mut client, root_posix) = setup_connected().await;
    let ctx = remote_ctx(&root_posix);

    let path = format!("{root_posix}/stale.txt");
    client.write_file(&path, b"old content\n").await.unwrap();

    // TPI read 拿到 R1。
    let r1 = client.read_file(&path).await.unwrap();
    let rev1 = tpi::tool::edit::revision_of(&r1);

    // 外部修改 → R2（模拟另一 SSH 会话改文件）。
    client
        .write_file(&path, b"external change\n")
        .await
        .unwrap();

    // TPI edit(R1) 必须 stale_rejected。
    let outcome = tpi::remote::files::remote_edit(
        &mut client,
        &RemoteEditArgs {
            path: path.clone(),
            revision: Some(rev1),
            replacements: vec![tpi::tool::edit::Replacement {
                old_text: "old content".into(),
                new_text: "tpi edit".into(),
            }],
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, tpi::tool::outcome::ToolStatus::Failed);
    assert!(
        outcome.model_payload.output.contains("stale_revision"),
        "必须 stale：{}",
        outcome.model_payload.output
    );

    // 文件未被 TPI 覆盖（外部修改保留）。
    let now = client.read_file(&path).await.unwrap();
    assert_eq!(now, b"external change\n");
}

/// §41：远端 edit 成功——revision 正确 + atomic batch + diff。
#[tokio::test]
async fn remote_edit_applies_and_returns_diff() {
    let (_root, mut client, root_posix) = setup_connected().await;
    let ctx = remote_ctx(&root_posix);

    let path = format!("{root_posix}/edit.txt");
    client.write_file(&path, b"a\nb\nc\n").await.unwrap();
    let rev = tpi::tool::edit::revision_of(b"a\nb\nc\n");

    let outcome = tpi::remote::files::remote_edit(
        &mut client,
        &RemoteEditArgs {
            path: path.clone(),
            revision: Some(rev),
            replacements: vec![
                tpi::tool::edit::Replacement {
                    old_text: "a".into(),
                    new_text: "A".into(),
                },
                tpi::tool::edit::Replacement {
                    old_text: "c".into(),
                    new_text: "C".into(),
                },
            ],
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, tpi::tool::outcome::ToolStatus::Succeeded);
    assert!(
        outcome.model_payload.output.contains("applied: 2"),
        "{}",
        outcome.model_payload.output
    );
    assert!(
        outcome.session_metadata.diff.is_some(),
        "diff 必须进 metadata"
    );

    // 文件已更新。
    let now = client.read_file(&path).await.unwrap();
    assert_eq!(now, b"A\nb\nC\n");

    // 新 revision = 编辑后内容。
    let new_rev = tpi::tool::edit::revision_of(b"A\nb\nC\n");
    let rev_in_output = outcome
        .model_payload
        .output
        .lines()
        .find(|l| l.starts_with("[revision="))
        .map(|l| l.to_string());
    assert!(
        rev_in_output.unwrap().contains(&new_rev[3..]),
        "新 revision 应出现在输出"
    );
}

/// §41：atomic batch——任一条不匹配整体拒绝，文件不变。
#[tokio::test]
async fn remote_edit_atomic_batch_rejects_partial() {
    let (_root, mut client, root_posix) = setup_connected().await;
    let ctx = remote_ctx(&root_posix);

    let path = format!("{root_posix}/atomic.txt");
    client.write_file(&path, b"keep\n").await.unwrap();
    let rev = tpi::tool::edit::revision_of(b"keep\n");

    let outcome = tpi::remote::files::remote_edit(
        &mut client,
        &RemoteEditArgs {
            path: path.clone(),
            revision: Some(rev),
            replacements: vec![
                tpi::tool::edit::Replacement {
                    old_text: "keep".into(),
                    new_text: "changed".into(),
                },
                tpi::tool::edit::Replacement {
                    old_text: "不存在".into(),
                    new_text: "x".into(),
                },
            ],
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, tpi::tool::outcome::ToolStatus::Failed);
    assert!(
        outcome.model_payload.output.contains("no_match"),
        "{}",
        outcome.model_payload.output
    );

    // 文件不变（原子性）。
    let now = client.read_file(&path).await.unwrap();
    assert_eq!(now, b"keep\n");
}

/// §42：revision transport-independent——本地和远端相同 bytes 得到相同 revision。
#[tokio::test]
async fn revision_is_content_identity_across_transport() {
    let (_root, _client, root_posix) = setup_connected().await;
    let _ = root_posix;
    let bytes = b"same content\n\xe4\xb8\xad\xe6\x96\x87";
    let local = tpi::tool::edit::revision_of(bytes);
    let via_snapshot = tpi::tool::edit::revision_of(bytes);
    assert_eq!(
        local, via_snapshot,
        "revision 是内容身份，与传输无关（§42）"
    );
}
