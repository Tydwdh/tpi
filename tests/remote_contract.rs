//! R0 SSH transport 集成测试（loopback russh server）。
//!
//! 验证 connect / host key 校验 / exec / file read-write / disconnect /
//! reconnect（任务书 §31 primitives + §34 host key + §60 远程矩阵基础）。
//! server 与 SFTP 后端在 `fixtures::remote_server`。

mod fixtures;

use std::path::PathBuf;

use tpi::remote::ssh::{ConnectionState, HostKeyDecision, SshClient};

/// §34：未知 host 首次连接 → UnknownPending；用户确认（confirm_host_key）后
/// 重连 → Accepted 且可执行命令。Agent 不得自动信任（decision 必须显式确认）。
#[tokio::test]
async fn host_key_unknown_requires_confirmation_then_connects() {
    let (port, _root, known_hosts) = fixtures::remote_server::start_test_server().await;
    let mut client = fixtures::remote_server::test_client(port, &known_hosts).await;

    // 首次：未知 host。
    let decision = client.connect().await.unwrap();
    assert_eq!(decision, HostKeyDecision::UnknownPending);
    assert!(client.pending_host_key().is_some(), "必须记录待确认 key");
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);

    // 用户确认。
    client.confirm_host_key().unwrap();
    assert!(client.pending_host_key().is_none());

    // 重连：known_hosts 已记录 → Accepted。
    let decision = client.connect().await.unwrap();
    assert_eq!(decision, HostKeyDecision::Accepted);
    assert_eq!(client.connection_state(), ConnectionState::Connected);

    // 能执行命令。
    let result = client
        .exec("echo hello", None, &Default::default(), None)
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "hello");
    assert_eq!(result.exit_code, Some(0));

    client.disconnect().await;
}

/// 已确认的主机再次连接（known_hosts 命中）→ 直接 Accepted（不重复询问）。
#[tokio::test]
async fn known_host_connects_directly() {
    let (port, _root, known_hosts) = fixtures::remote_server::start_test_server().await;
    let mut client = fixtures::remote_server::test_client(port, &known_hosts).await;

    // 第一次确认。
    assert_eq!(
        client.connect().await.unwrap(),
        HostKeyDecision::UnknownPending
    );
    client.confirm_host_key().unwrap();

    // 新 client（同 known_hosts 文件）直接 Accepted。
    let mut client2 = fixtures::remote_server::test_client(port, &known_hosts).await;
    assert_eq!(client2.connect().await.unwrap(), HostKeyDecision::Accepted);
    client2.disconnect().await;
}

/// §38：exec 返回 stdout/stderr/exit code；fresh channel 每命令。
#[tokio::test]
async fn exec_returns_output_and_exit_code() {
    let (port, _root, known_hosts) = fixtures::remote_server::start_test_server().await;
    let mut client = fixtures::remote_server::test_client(port, &known_hosts).await;
    assert_eq!(
        client.connect().await.unwrap(),
        HostKeyDecision::UnknownPending
    );
    client.confirm_host_key().unwrap();
    assert_eq!(client.connect().await.unwrap(), HostKeyDecision::Accepted);

    // 成功命令。
    let r = client
        .exec("printf 'out1\\nout2'", None, &Default::default(), None)
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(String::from_utf8_lossy(&r.stdout), "out1\nout2");

    // 失败命令（exit 非 0）。
    let r = client
        .exec("exit 7", None, &Default::default(), None)
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(7));

    // 连续两条命令（fresh channel 复用 transport 连接）。
    let r1 = client
        .exec("echo one", None, &Default::default(), None)
        .await
        .unwrap();
    let r2 = client
        .exec("echo two", None, &Default::default(), None)
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&r1.stdout).trim(), "one");
    assert_eq!(String::from_utf8_lossy(&r2.stdout).trim(), "two");

    client.disconnect().await;
}

/// §R0 primitive：write_file（temp+rename）与 read_file 往返一致。
#[tokio::test]
async fn write_and_read_file_roundtrip() {
    let (port, root, known_hosts) = fixtures::remote_server::start_test_server().await;
    let mut client = fixtures::remote_server::test_client(port, &known_hosts).await;
    assert_eq!(
        client.connect().await.unwrap(),
        HostKeyDecision::UnknownPending
    );
    client.confirm_host_key().unwrap();
    assert_eq!(client.connect().await.unwrap(), HostKeyDecision::Accepted);

    let content = b"hello remote\nsecond line\n";
    client.write_file("data.txt", content).await.unwrap();

    // 远端文件确实存在（temp + rename 已提交）。
    let on_disk = std::fs::read(root.path().join("data.txt")).unwrap();
    assert_eq!(on_disk, content);

    let read_back = client.read_file("data.txt").await.unwrap();
    assert_eq!(read_back, content);

    client.disconnect().await;
}

/// §49/§50：disconnect → reconnect 恢复 transport（exec 再次可用）。
#[tokio::test]
async fn disconnect_then_reconnect() {
    let (port, _root, known_hosts) = fixtures::remote_server::start_test_server().await;
    let mut client = fixtures::remote_server::test_client(port, &known_hosts).await;
    assert_eq!(
        client.connect().await.unwrap(),
        HostKeyDecision::UnknownPending
    );
    client.confirm_host_key().unwrap();
    assert_eq!(client.connect().await.unwrap(), HostKeyDecision::Accepted);

    let r = client
        .exec("echo before", None, &Default::default(), None)
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "before");

    client.disconnect().await;
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);

    // 未连接时 exec 报 NotConnected。
    let err = client
        .exec("echo x", None, &Default::default(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, tpi::remote::ssh::SshError::NotConnected));

    // reconnect 恢复。
    let decision = client.reconnect().await.unwrap();
    assert_eq!(decision, HostKeyDecision::Accepted);
    assert_eq!(client.connection_state(), ConnectionState::Connected);
    let r = client
        .exec("echo after", None, &Default::default(), None)
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "after");

    client.disconnect().await;
}

/// 密码错误 → Auth 失败。
#[tokio::test]
async fn wrong_password_fails_auth() {
    let (port, _root, known_hosts) = fixtures::remote_server::start_test_server().await;
    let mut host = tpi::remote::ssh::RemoteHost::direct("127.0.0.1", port, "test");
    host.known_hosts_path = known_hosts.clone();
    host.password = Some("wrong".into());
    let mut client = SshClient::new(host);

    assert_eq!(
        client.connect().await.unwrap(),
        HostKeyDecision::UnknownPending
    );
    client.confirm_host_key().unwrap();
    let err = client.connect().await.unwrap_err();
    assert!(
        matches!(err, tpi::remote::ssh::SshError::Auth(_)),
        "错误密码必须 Auth 失败：{err:?}"
    );
}

#[allow(dead_code)]
fn _pathbuf_keep(_: PathBuf) {}
