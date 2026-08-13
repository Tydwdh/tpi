//! R0 SSH transport 集成测试（loopback russh server）。
//!
//! 本机无 sshd / 无真实主机，用 russh 自带的 **server 端**在 127.0.0.1 起
//! 一个测试 SSH server（exec + SFTP），client 连上去验证：
//! connect / host key 校验 / exec / file read/write / disconnect / reconnect
//! （任务书 §31 primitives + §34 host key + §60 远程矩阵的基础）。
//!
//! SFTP server 端用临时目录 + std::fs 实现（测试专用，非产品代码）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use russh::keys::ssh_key;
use russh::server::{Auth, ChannelOpenHandle, Msg, Session};
use russh::server::Server as _;
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
};
use tokio::net::TcpListener;
use tpi::remote::ssh::{ConnectionState, HostKeyDecision, RemoteHost, SshClient};

// ---------------------------------------------------------------------------
// 测试 SSH server
// ---------------------------------------------------------------------------

/// 每个连接的 handler：持有 SFTP 根目录（临时目录）。
#[derive(Clone)]
struct TestServer {
    root: Arc<PathBuf>,
}

impl russh::server::Server for TestServer {
    type Handler = TestHandler;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
        TestHandler {
            root: self.root.clone(),
            channels: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }
}

#[derive(Clone)]
struct TestHandler {
    root: Arc<PathBuf>,
    channels: Arc<tokio::sync::Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl russh::server::Handler for TestHandler {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    async fn auth_password(&mut self, _: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if password == "test-pw" {
            Auth::Accept
        } else {
            Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            }
        })
    }

    async fn auth_publickey(
        &mut self,
        _: &str,
        _: &ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        // 测试 server 接受任意 pubkey（client 默认走 identity_file 或密码）。
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data);
        // 在远端（测试 server 所在进程）执行；用 bash 支持管道等。
        let output = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(cmd.as_ref())
            .output()
            .await
            .unwrap();
        let _ = session.data(channel, output.stdout.clone());
        if !output.stderr.is_empty() {
            let _ = session.extended_data(channel, 1, output.stderr.clone());
        }
        let code = output.status.code().unwrap_or(1) as u32;
        let _ = session.exit_status_request(channel, code);
        let _ = session.eof(channel);
        let _ = session.channel_success(channel);
        let _ = session.close(channel);
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "sftp" {
            let channel = self.channels.lock().await.remove(&channel_id).unwrap();
            let _ = session.channel_success(channel_id);
            let handler = SftpBackend {
                root: self.root.clone(),
                handles: std::collections::HashMap::new(),
            };
            russh_sftp::server::run(channel.into_stream(), handler).await;
        } else {
            let _ = session.channel_failure(channel_id);
        }
        Ok(())
    }
}

/// SFTP 文件系统后端（临时目录，测试用）。
struct SftpBackend {
    root: Arc<PathBuf>,
    handles: std::collections::HashMap<String, (PathBuf, OpenFlags, u64)>,
}

impl SftpBackend {
    fn join_root(&self, path: &str) -> PathBuf {
        let p = path.trim_start_matches('/');
        self.root.join(p)
    }
}

impl russh_sftp::server::Handler for SftpBackend {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = self.join_root(&filename);
        let mut opts = std::fs::OpenOptions::new();
        if pflags.contains(OpenFlags::READ) {
            opts.read(true);
        }
        if pflags.contains(OpenFlags::WRITE)
            || pflags.contains(OpenFlags::CREATE)
            || pflags.contains(OpenFlags::TRUNCATE)
        {
            opts.write(true);
        }
        if pflags.contains(OpenFlags::CREATE) {
            opts.create(true);
        }
        if pflags.contains(OpenFlags::TRUNCATE) {
            opts.truncate(true);
        }
        if pflags.contains(OpenFlags::APPEND) {
            opts.append(true);
        }
        opts.open(&path).map_err(|_| StatusCode::Failure)?;
        let handle = format!("h{}", id);
        self.handles.insert(handle.clone(), (path, pflags, 0));
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let Some((path, _, pos)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        *pos = offset;
        let mut f = std::fs::File::open(path).map_err(|_| StatusCode::Failure)?;
        use std::io::{Read, Seek, SeekFrom};
        f.seek(SeekFrom::Start(offset)).map_err(|_| StatusCode::Failure)?;
        let mut buf = vec![0u8; len as usize];
        let n = f.read(&mut buf).map_err(|_| StatusCode::Failure)?;
        buf.truncate(n);
        Ok(Data { id, data: buf })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let Some((path, _, _)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(path)
            .map_err(|_| StatusCode::Failure)?;
        f.seek(SeekFrom::Start(offset)).map_err(|_| StatusCode::Failure)?;
        f.write_all(&data).map_err(|_| StatusCode::Failure)?;
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let p = self.join_root(&path);
        let meta = std::fs::metadata(&p).map_err(|_| StatusCode::NoSuchFile)?;
        let mut attrs = FileAttributes {
            size: Some(meta.len()),
            uid: None,
            user: None,
            gid: None,
            group: None,
            permissions: Some(0o644),
            atime: None,
            mtime: None,
        };
        if meta.is_dir() {
            attrs.set_dir(true);
        } else {
            attrs.set_regular(true);
        }
        Ok(Attrs { id, attrs })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let Some((path, _, _)) = self.handles.get(&handle) else {
            return Err(StatusCode::Failure);
        };
        let meta = std::fs::metadata(path).map_err(|_| StatusCode::NoSuchFile)?;
        let mut attrs = FileAttributes {
            size: Some(meta.len()),
            uid: None,
            user: None,
            gid: None,
            group: None,
            permissions: Some(0o644),
            atime: None,
            mtime: None,
        };
        if meta.is_dir() {
            attrs.set_dir(true);
        } else {
            attrs.set_regular(true);
        }
        Ok(Attrs { id, attrs })
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let old = self.join_root(&oldpath);
        let new = self.join_root(&newpath);
        std::fs::rename(&old, &new).map_err(|_| StatusCode::Failure)?;
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        })
    }

    async fn remove(
        &mut self,
        id: u32,
        filename: String,
    ) -> Result<Status, Self::Error> {
        let p = self.join_root(&filename);
        std::fs::remove_file(&p).map_err(|_| StatusCode::Failure)?;
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let p = self.join_root(&path);
        if !p.is_dir() {
            return Err(StatusCode::NoSuchFile);
        }
        Ok(Handle {
            id,
            handle: format!("dir{id}"),
        })
    }

    async fn readdir(&mut self, id: u32, _handle: String) -> Result<Name, Self::Error> {
        // 简化：客户端 read_dir 返回 entries；此处用 id 回查目录已不可行，
        // 改为读取 root（测试只在一个目录下操作）。
        let mut files = Vec::new();
        let rd = std::fs::read_dir(self.root.as_path()).map_err(|_| StatusCode::Failure)?;
        for entry in rd.flatten() {
            let meta = entry.metadata().map_err(|_| StatusCode::Failure)?;
            let mut attrs = FileAttributes {
                size: Some(meta.len()),
                uid: None,
                user: None,
                gid: None,
                group: None,
                permissions: Some(0o644),
                atime: None,
                mtime: None,
            };
            if meta.is_dir() {
                attrs.set_dir(true);
            } else {
                attrs.set_regular(true);
            }
            files.push(File {
                filename: entry.file_name().to_string_lossy().into_owned(),
                longname: String::new(),
                attrs,
            });
        }
        Ok(Name { id, files })
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let p = self.join_root(&path);
        Ok(Name {
            id,
            files: vec![File {
                filename: p.to_string_lossy().into_owned(),
                longname: String::new(),
                attrs: FileAttributes::default(),
            }],
        })
    }
}

// ---------------------------------------------------------------------------
// 测试工具
// ---------------------------------------------------------------------------

/// 启动一个测试 SSH server，返回 (port, tempdir_root, known_hosts 文件路径)。
async fn start_test_server() -> (u16, tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let known_hosts = root.path().join("known_hosts");

    let config = {
        let mut c = russh::server::Config::default();
        c.keys.push(
            russh::keys::PrivateKey::random(&mut rand::rng(), ssh_key::Algorithm::Ed25519)
                .unwrap(),
        );
        Arc::new(c)
    };

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let root_path = Arc::new(root.path().to_path_buf());
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let mut server = TestServer {
                root: root_path.clone(),
            };
            let handler = server.new_client(None);
            let config = config.clone();
            tokio::spawn(async move {
                let _ = russh::server::run_stream(config, stream, handler).await;
            });
        }
    });
    (port, root, known_hosts)
}

async fn test_client(port: u16, known_hosts: &PathBuf) -> SshClient {
    let mut host = RemoteHost::direct("127.0.0.1", port, "test");
    host.known_hosts_path = known_hosts.clone();
    host.password = Some("test-pw".into());
    SshClient::new(host)
}

// ---------------------------------------------------------------------------
// 测试用例（§31 primitives + §34 host key）
// ---------------------------------------------------------------------------

/// §34：未知 host 首次连接 → UnknownPending；用户确认（confirm_host_key）后
/// 重连 → Accepted 且可执行命令。Agent 不得自动信任（decision 必须显式确认）。
#[tokio::test]
async fn host_key_unknown_requires_confirmation_then_connects() {
    let (port, _root, known_hosts) = start_test_server().await;
    let mut client = test_client(port, &known_hosts).await;

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
        .exec("echo hello", None, &Default::default())
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "hello");
    assert_eq!(result.exit_code, Some(0));

    client.disconnect().await;
}

/// 已确认的主机再次连接（known_hosts 命中）→ 直接 Accepted（不重复询问）。
#[tokio::test]
async fn known_host_connects_directly() {
    let (port, _root, known_hosts) = start_test_server().await;
    let mut client = test_client(port, &known_hosts).await;

    // 第一次确认。
    assert_eq!(client.connect().await.unwrap(), HostKeyDecision::UnknownPending);
    client.confirm_host_key().unwrap();

    // 新 client（同 known_hosts 文件）直接 Accepted。
    let mut client2 = test_client(port, &known_hosts).await;
    assert_eq!(client2.connect().await.unwrap(), HostKeyDecision::Accepted);
    client2.disconnect().await;
}

/// §38：exec 返回 stdout/stderr/exit code；fresh channel 每命令。
#[tokio::test]
async fn exec_returns_output_and_exit_code() {
    let (port, _root, known_hosts) = start_test_server().await;
    let mut client = test_client(port, &known_hosts).await;
    assert_eq!(client.connect().await.unwrap(), HostKeyDecision::UnknownPending);
    client.confirm_host_key().unwrap();
    assert_eq!(client.connect().await.unwrap(), HostKeyDecision::Accepted);

    // 成功命令。
    let r = client.exec("printf 'out1\\nout2'", None, &Default::default()).await.unwrap();
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(String::from_utf8_lossy(&r.stdout), "out1\nout2");

    // 失败命令（exit 非 0）。
    let r = client.exec("exit 7", None, &Default::default()).await.unwrap();
    assert_eq!(r.exit_code, Some(7));

    // 连续两条命令（fresh channel 复用 transport 连接）。
    let r1 = client.exec("echo one", None, &Default::default()).await.unwrap();
    let r2 = client.exec("echo two", None, &Default::default()).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&r1.stdout).trim(), "one");
    assert_eq!(String::from_utf8_lossy(&r2.stdout).trim(), "two");

    client.disconnect().await;
}

/// §R0 primitive：write_file（temp+rename）与 read_file 往返一致。
#[tokio::test]
async fn write_and_read_file_roundtrip() {
    let (port, root, known_hosts) = start_test_server().await;
    let mut client = test_client(port, &known_hosts).await;
    assert_eq!(client.connect().await.unwrap(), HostKeyDecision::UnknownPending);
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
    let (port, _root, known_hosts) = start_test_server().await;
    let mut client = test_client(port, &known_hosts).await;
    assert_eq!(client.connect().await.unwrap(), HostKeyDecision::UnknownPending);
    client.confirm_host_key().unwrap();
    assert_eq!(client.connect().await.unwrap(), HostKeyDecision::Accepted);

    let r = client.exec("echo before", None, &Default::default()).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "before");

    client.disconnect().await;
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);

    // 未连接时 exec 报 NotConnected。
    let err = client.exec("echo x", None, &Default::default()).await.unwrap_err();
    assert!(matches!(err, tpi::remote::ssh::SshError::NotConnected));

    // reconnect 恢复。
    let decision = client.reconnect().await.unwrap();
    assert_eq!(decision, HostKeyDecision::Accepted);
    assert_eq!(client.connection_state(), ConnectionState::Connected);
    let r = client.exec("echo after", None, &Default::default()).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "after");

    client.disconnect().await;
}

/// 密码错误 → Auth 失败。
#[tokio::test]
async fn wrong_password_fails_auth() {
    let (port, _root, known_hosts) = start_test_server().await;
    let mut host = RemoteHost::direct("127.0.0.1", port, "test");
    host.known_hosts_path = known_hosts.clone();
    host.password = Some("wrong".into());
    let mut client = SshClient::new(host);

    assert_eq!(client.connect().await.unwrap(), HostKeyDecision::UnknownPending);
    client.confirm_host_key().unwrap();
    let err = client.connect().await.unwrap_err();
    assert!(
        matches!(err, tpi::remote::ssh::SshError::Auth(_)),
        "错误密码必须 Auth 失败：{err:?}"
    );
}
