//! Loopback SSH 测试 server（russh server 端；exec + SFTP）。
//!
//! 本机无 sshd / 无真实主机时，用 russh 自带的 server 端在 127.0.0.1 起
//! 测试 SSH server，client 连上去做集成测试。SFTP 后端用临时目录 + `std::fs`
//!（测试专用，非产品代码）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::keys::ssh_key;
use russh::server::Server as _;
use russh::server::{Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
};
use tokio::net::TcpListener;
use tpi::remote::ssh::{RemoteHost, SshClient};

pub const TEST_PASSWORD: &str = "test-pw";

/// 每个连接的 handler：持有 SFTP 根目录（临时目录）。
#[derive(Clone)]
struct TestServer {
    root: Arc<PathBuf>,
    posix_root: String,
}

impl russh::server::Server for TestServer {
    type Handler = TestHandler;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
        TestHandler {
            root: self.root.clone(),
            posix_root: self.posix_root.clone(),
            channels: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }
}

#[derive(Clone)]
struct TestHandler {
    root: Arc<PathBuf>,
    posix_root: String,
    channels: Arc<tokio::sync::Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl russh::server::Handler for TestHandler {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    async fn auth_password(&mut self, _: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if password == TEST_PASSWORD {
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
        let shell = if cfg!(windows) {
            std::env::var_os("TPI_TEST_BASH")
                .map(std::path::PathBuf::from)
                .filter(|p| p.is_file())
                .or_else(|| {
                    [
                        r"C:\Program Files\Git\bin\bash.exe",
                        r"C:\Program Files\Git\usr\bin\bash.exe",
                    ]
                    .into_iter()
                    .map(std::path::PathBuf::from)
                    .find(|p| p.is_file())
                })
                .unwrap_or_else(|| std::path::PathBuf::from("bash"))
        } else {
            std::path::PathBuf::from("bash")
        };
        let output = tokio::process::Command::new(shell)
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
                posix_root: self.posix_root.clone(),
                handles: std::collections::HashMap::new(),
                dirs: std::collections::HashMap::new(),
            };
            russh_sftp::server::run(channel.into_stream(), handler).await;
        } else {
            let _ = session.channel_failure(channel_id);
        }
        Ok(())
    }
}

/// SFTP 文件系统后端（临时目录，测试用）。
///
/// 模拟一个远端 FS：虚拟根 = tempdir。客户端可能用相对路径（"data.txt"）
/// 或绝对 POSIX 路径（"/tmp/.tmpXXX/data.txt"，其前缀即 tempdir 的 POSIX
/// 形式）——两者都要映射到 tempdir 内。
struct SftpBackend {
    root: Arc<PathBuf>,
    posix_root: String,
    handles: std::collections::HashMap<String, (PathBuf, OpenFlags, u64)>,
    /// opendir 句柄 → (目录路径, 是否已返回过)。readdir 第一次返回全部条目，
    /// 之后返回 EOF（russh-sftp 客户端迭代器以 EOF 结束）。
    dirs: std::collections::HashMap<String, (PathBuf, bool)>,
}

impl SftpBackend {
    fn join_root(&self, path: &str) -> PathBuf {
        let trimmed = path.trim_start_matches('/');
        // 绝对 POSIX 路径且以虚拟根开头 → 去掉前缀（远端视角的绝对路径）。
        if let Some(rest) = trimmed.strip_prefix(self.posix_root.trim_start_matches('/')) {
            let rest = rest.trim_start_matches('/');
            if rest.is_empty() {
                return self.root.as_path().to_path_buf();
            }
            return self.root.join(rest);
        }
        self.root.join(trimmed)
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
        let handle = format!("h{id}");
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
        let Some((path, _, _)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        let mut f = std::fs::File::open(path).map_err(|_| StatusCode::Failure)?;
        use std::io::{Read, Seek, SeekFrom};
        f.seek(SeekFrom::Start(offset))
            .map_err(|_| StatusCode::Failure)?;
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
            .truncate(false) // sftp write 语义：按 offset 覆盖，不截断其余内容
            .open(path)
            .map_err(|_| StatusCode::Failure)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|_| StatusCode::Failure)?;
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

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
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
        let handle = format!("dir{id}");
        self.dirs.insert(handle.clone(), (p, false));
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let Some((dir, done)) = self.dirs.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        if *done {
            // 已返回过：EOF 结束迭代（russh-sftp 客户端 read_dir 约定）。
            return Err(StatusCode::Eof);
        }
        *done = true;
        let mut files = Vec::new();
        let rd = std::fs::read_dir(dir).map_err(|_| StatusCode::Failure)?;
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

/// Windows 路径 → msys/Git Bash POSIX 形式（`cygpath -u` 的最小等价实现）。
///
/// - `C:\foo\bar` → `/c/foo/bar`（盘符小写）；
/// - `\\server\share\foo` → `//server/share/foo`（UNC）；
/// - 已是 POSIX 绝对路径（如 Linux 的 `/tmp/.tmpXXX`）→ 原样返回。
///
/// 只用于测试 fixture：server 与各 remote 测试用**同一**转换，保证绝对
/// 路径前缀匹配一致；不依赖外部 `cygpath`（P0-02：消除隐式工具前提，
/// Windows 无 Git Bash 时也能跑 remote 测试）。
pub fn win_to_posix(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.starts_with("//") {
        return s; // UNC：已是 //server/share
    }
    if s.starts_with('/') {
        return s; // 已是 POSIX 绝对路径
    }
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        let drive = bytes[0].to_ascii_lowercase() as char;
        // 对齐 cygpath：`C:` → `/c`，`C:/`（盘符根）→ `/c/`。
        let tail = &s[2..];
        if tail.is_empty() {
            format!("/{drive}")
        } else {
            format!("/{drive}/{}", tail.trim_start_matches('/'))
        }
    } else {
        s
    }
}

/// 启动一个测试 SSH server，返回 (port, `tempdir_root`, `known_hosts` 文件路径)。
pub async fn start_test_server() -> (u16, tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let known_hosts = root.path().join("known_hosts");

    let config = {
        let mut c = russh::server::Config::default();
        c.keys.push(
            russh::keys::PrivateKey::random(&mut rand::rng(), ssh_key::Algorithm::Ed25519).unwrap(),
        );
        Arc::new(c)
    };

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let root_path = Arc::new(root.path().to_path_buf());
    // 远端 POSIX root（msys 形式，供绝对路径映射）；纯 Rust 转换，不依赖 cygpath。
    let posix_root = win_to_posix(root.path());
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let mut server = TestServer {
                root: root_path.clone(),
                posix_root: posix_root.clone(),
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

/// 构造连向测试 server 的 client（密码认证）。
pub async fn test_client(port: u16, known_hosts: &Path) -> SshClient {
    let mut host = RemoteHost::direct("127.0.0.1", port, "test");
    host.known_hosts_path = known_hosts.to_path_buf();
    host.password = Some(TEST_PASSWORD.into());
    SshClient::new(host)
}

#[cfg(test)]
mod tests {
    use super::win_to_posix;
    use std::path::Path;

    #[test]
    fn win_to_posix_drive_letter() {
        assert_eq!(
            win_to_posix(Path::new("C:\\Users\\foo\\Temp\\abc")),
            "/c/Users/foo/Temp/abc"
        );
        // 正斜杠输入与大小写混用也应归一化盘符。
        assert_eq!(win_to_posix(Path::new("D:/Work/x.rs")), "/d/Work/x.rs");
    }

    #[test]
    fn win_to_posix_unc() {
        assert_eq!(
            win_to_posix(Path::new("\\\\server\\share\\dir")),
            "//server/share/dir"
        );
    }

    #[test]
    fn win_to_posix_already_posix() {
        // Linux 上 tempdir 即 POSIX 绝对路径，必须原样返回。
        assert_eq!(win_to_posix(Path::new("/tmp/.tmpXYZ")), "/tmp/.tmpXYZ");
        assert_eq!(win_to_posix(Path::new("relative")), "relative");
    }

    #[test]
    fn win_to_posix_drive_root() {
        // 对齐 cygpath：`C:` → `/c`，`C:/`（盘符根）→ `/c/`。
        assert_eq!(win_to_posix(Path::new("C:\\")), "/c/");
        assert_eq!(win_to_posix(Path::new("C:")), "/c");
    }
}
