//! SSH transport（任务书 §31-§34、§37-§40、§50）。
//!
//! R0 交付：connect / disconnect / reconnect / exec / file read / file write
//! primitives，**不接 Tool**（R1 起按 ActiveWorkspace 分发）。
//!
//! 设计要点：
//! - §37：Connection（transport）与 ShellSessionState（logical state）分离——
//!   socket 断开不代表 cwd/env 消失，重连后恢复 logical state；
//! - §38：persistent transport connection + fresh exec channel per command
//!   （TCP 连接可复用，shell 进程不复用）；
//! - §34：host key verification 强制——未知 host 必须由用户确认（Agent 不得
//!   自行信任）；key 变化（KeyChanged）直接拒绝；
//! - §33：优先复用用户 `~/.ssh/config`（russh-config 解析 Host 别名）。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use russh::client::{self, Handle};
use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::ssh_key;
use russh_sftp::protocol::{FileAttributes, OpenFlags};

/// R0 模块的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("ssh config 解析失败: {0}")]
    Config(String),
    #[error("主机未在 ~/.ssh/config 中找到: {alias}")]
    HostNotFound { alias: String },
    #[error("连接失败: {0}")]
    Connect(String),
    #[error("认证失败: {0}")]
    Auth(String),
    #[error("host key 已变更（可能的 MITM）: {0}")]
    HostKeyChanged(String),
    #[error("命令执行失败: {0}")]
    Exec(String),
    #[error("SFTP 失败: {0}")]
    Sftp(String),
    /// SFTP 明确返回 SSH_FX_NO_SUCH_FILE(2)："文件不存在"是确定事实。
    /// 单独区分——网络/权限/协议等其他 SFTP 错误**不得**被当作"文件不存在"
    /// 处理（否则 remote_write 会在网络抖动时跳过 revision 校验覆盖远端文件）。
    #[error("SFTP 文件不存在: {0}")]
    SftpNoSuchFile(String),
    #[error("IO 失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("未连接")]
    NotConnected,
}

/// 远端主机描述（§33：从 ~/.ssh/config 解析，或显式直连参数）。
/// ISSUE-038：手动实现 Debug——`password` 必须打码，绝不允许出现在日志中。
#[derive(Clone)]
pub struct RemoteHost {
    /// 用户输入的别名（如 `gpu`）；无 config 时即 hostname。
    pub alias: String,
    pub hostname: String,
    pub port: u16,
    pub user: String,
    /// IdentityFile（config 指定或默认探测 ~/.ssh/id_*）。
    pub identity_file: Option<PathBuf>,
    /// 用户 known_hosts 文件（config UserKnownHostsFile 或默认）。
    pub known_hosts_path: PathBuf,
    /// StrictHostKeyChecking（§34：未知 host 是否可确认；默认 yes=严格）。
    pub strict_host_key_checking: bool,
    /// 可选密码（config 不存密码；由用户显式提供）。
    pub password: Option<String>,
}

impl std::fmt::Debug for RemoteHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteHost")
            .field("alias", &self.alias)
            .field("hostname", &self.hostname)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("identity_file", &self.identity_file)
            .field("known_hosts_path", &self.known_hosts_path)
            .field("strict_host_key_checking", &self.strict_host_key_checking)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl RemoteHost {
    /// 从 `~/.ssh/config` 解析 Host 别名（§33）。
    pub fn from_alias(alias: &str) -> Result<Self, SshError> {
        let config = russh_config::parse_home(alias).map_err(|e| match e {
            russh_config::Error::HostNotFound => SshError::HostNotFound {
                alias: alias.to_string(),
            },
            other => SshError::Config(other.to_string()),
        })?;
        let hc = &config.host_config;
        let identity_file = hc
            .identity_file
            .as_deref()
            .and_then(|files| files.first().cloned());
        let known_hosts_path = hc
            .user_known_hosts_file
            .clone()
            .unwrap_or_else(default_known_hosts_path);
        Ok(Self {
            alias: alias.to_string(),
            hostname: config.host().to_string(),
            port: config.port(),
            user: config.user(),
            identity_file,
            known_hosts_path,
            strict_host_key_checking: hc.strict_host_key_checking.unwrap_or(true),
            password: None,
        })
    }

    /// 无 config 直连（hostname/port/user 显式）。
    pub fn direct(hostname: &str, port: u16, user: &str) -> Self {
        Self {
            alias: hostname.to_string(),
            hostname: hostname.to_string(),
            port,
            user: user.to_string(),
            identity_file: None,
            known_hosts_path: default_known_hosts_path(),
            strict_host_key_checking: true,
            password: None,
        }
    }

    /// 探测默认私钥路径（~/.ssh/id_ed25519 / id_rsa / id_ecdsa）。
    fn probe_identity_file() -> Option<PathBuf> {
        for name in ["id_ed25519", "id_rsa", "id_ecdsa"] {
            let p = std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(std::path::PathBuf::from)
                .map(|home| home.join(".ssh").join(name));
            if let Some(p) = p
                && p.is_file()
            {
                return Some(p);
            }
        }
        None
    }
}

fn default_known_hosts_path() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".ssh")
        .join("known_hosts")
}

/// 连接状态（§50：runtime state，非 logical state）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Failed(String),
}

/// host key 校验结果（§34）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// known_hosts 匹配，已接受。
    Accepted,
    /// 未知 host：需要用户确认后才能继续（Agent 不得自行选择信任）。
    UnknownPending,
    /// known_hosts 中记录不同 key（MITM 风险），拒绝。
    Changed,
}

/// 一条远端命令的结果。
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub exit_code: Option<u32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// 输出在传输层被硬上限截断（ISSUE-008：与本地 `MAX_OUTPUT_BUDGET` 对齐，
    /// 防止远端 `cat /dev/urandom` 式输出耗尽进程内存）。
    pub truncated: bool,
}

/// 待用户确认的 server key（connect 失败后由 confirm_host_key 消费）。
#[derive(Debug, Clone)]
pub struct PendingHostKey {
    pub host: String,
    pub port: u16,
    pub public_key: ssh_key::PublicKey,
}

/// russh client Handler：host key 校验 + 记录 pending key。
#[derive(Clone)]
struct ClientHandler {
    host: RemoteHost,
    pending: Arc<Mutex<Option<PendingHostKey>>>,
    /// host key 校验结果（未知 → 需确认）。
    decision: Arc<Mutex<HostKeyDecision>>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let known = check_known_hosts_path(
            &self.host.hostname,
            self.host.port,
            server_public_key,
            &self.host.known_hosts_path,
        );
        match known {
            Ok(true) => {
                *self.decision.lock().unwrap() = HostKeyDecision::Accepted;
                Ok(true)
            }
            // 未知 → 记录 pending，返回 false（连接中断，由调用方询问用户）。
            Ok(false) => {
                *self.decision.lock().unwrap() = HostKeyDecision::UnknownPending;
                *self.pending.lock().unwrap() = Some(PendingHostKey {
                    host: self.host.hostname.clone(),
                    port: self.host.port,
                    public_key: server_public_key.clone(),
                });
                Ok(false)
            }
            Err(russh::keys::Error::KeyChanged { .. }) => {
                *self.decision.lock().unwrap() = HostKeyDecision::Changed;
                Ok(false)
            }
            Err(_) => {
                *self.decision.lock().unwrap() = HostKeyDecision::Changed;
                Ok(false)
            }
        }
    }
}

/// SSH transport 客户端封装（§38：persistent connection + fresh channel）。
pub struct SshClient {
    host: RemoteHost,
    session: Option<Handle<ClientHandler>>,
    state: Arc<Mutex<ConnectionState>>,
    pending: Arc<Mutex<Option<PendingHostKey>>>,
    decision: Arc<Mutex<HostKeyDecision>>,
}

impl SshClient {
    pub fn new(host: RemoteHost) -> Self {
        Self {
            host,
            session: None,
            state: Arc::new(Mutex::new(ConnectionState::Disconnected)),
            pending: Arc::new(Mutex::new(None)),
            decision: Arc::new(Mutex::new(HostKeyDecision::Changed)),
        }
    }

    pub fn connection_state(&self) -> ConnectionState {
        self.state.lock().unwrap().clone()
    }

    pub fn pending_host_key(&self) -> Option<PendingHostKey> {
        self.pending.lock().unwrap().clone()
    }

    /// 用户确认未知 host key（§34）：写入 known_hosts。
    pub fn confirm_host_key(&self) -> Result<(), SshError> {
        let pending = self
            .pending
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| SshError::Connect("没有待确认的 host key".into()))?;
        if let Some(parent) = self.host.known_hosts_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        learn_known_hosts_path(
            &pending.host,
            pending.port,
            &pending.public_key,
            &self.host.known_hosts_path,
        )
        .map_err(|e| SshError::Connect(format!("写入 known_hosts 失败: {e}")))?;
        Ok(())
    }

    /// 连接（含 host key 校验 + 认证）。
    ///
    /// 返回 [`HostKeyDecision`]：`Accepted` 表示已连接成功；
    /// `UnknownPending` 表示未知 host，调用方应询问用户后 [`confirm_host_key`]
    /// 再重连；`Changed` 表示 host key 变化（拒绝）。
    pub async fn connect(&mut self) -> Result<HostKeyDecision, SshError> {
        *self.state.lock().unwrap() = ConnectionState::Connecting;
        let config = Arc::new(client::Config::default());
        let handler = ClientHandler {
            host: self.host.clone(),
            pending: self.pending.clone(),
            decision: self.decision.clone(),
        };
        let addr = (self.host.hostname.as_str(), self.host.port);
        // russh 在 check_server_key 返回 false（未知/变更）时直接返回
        // "Unknown server key" 错误——此时以 handler 记录的 decision 为准：
        // 未知 → 请求用户确认；变更 → 拒绝（§34）。
        let mut session = match client::connect(config, addr, handler).await {
            Ok(session) => session,
            Err(e) => {
                let decision = self.decision.lock().unwrap().clone();
                match decision {
                    HostKeyDecision::Accepted => return Err(SshError::Connect(e.to_string())),
                    HostKeyDecision::UnknownPending | HostKeyDecision::Changed => {
                        *self.state.lock().unwrap() = ConnectionState::Disconnected;
                        return Ok(decision);
                    }
                }
            }
        };
        let decision = self.decision.lock().unwrap().clone();
        match decision {
            HostKeyDecision::Accepted => {}
            HostKeyDecision::UnknownPending | HostKeyDecision::Changed => {
                // host key 未通过：立即断开，不继续认证（§34）。
                let _ = session
                    .disconnect(
                        russh::Disconnect::HostKeyNotVerifiable,
                        "host key not verified",
                        "en",
                    )
                    .await;
                *self.state.lock().unwrap() = ConnectionState::Disconnected;
                return Ok(decision);
            }
        }
        self.authenticate(&mut session).await?;
        self.session = Some(session);
        *self.state.lock().unwrap() = ConnectionState::Connected;
        Ok(HostKeyDecision::Accepted)
    }

    async fn authenticate(&self, session: &mut Handle<ClientHandler>) -> Result<(), SshError> {
        // 1. identity_file（config 或默认探测）。
        let identity = self
            .host
            .identity_file
            .clone()
            .or_else(RemoteHost::probe_identity_file);
        if let Some(path) = identity
            && path.is_file()
            && let Ok(key_pair) = russh::keys::load_secret_key(&path, None)
        {
            let auth = session
                .authenticate_publickey(
                    &self.host.user,
                    russh::keys::PrivateKeyWithHashAlg::new(
                        Arc::new(key_pair),
                        session
                            .best_supported_rsa_hash()
                            .await
                            .ok()
                            .flatten()
                            .flatten(),
                    ),
                )
                .await
                .map_err(|e| SshError::Auth(e.to_string()))?;
            if auth.success() {
                return Ok(());
            }
        }
        // 2. 密码。
        if let Some(password) = &self.host.password {
            let auth = session
                .authenticate_password(&self.host.user, password)
                .await
                .map_err(|e| SshError::Auth(e.to_string()))?;
            if auth.success() {
                return Ok(());
            }
            return Err(SshError::Auth("密码认证失败".into()));
        }
        Err(SshError::Auth(format!(
            "认证失败：无可用凭据（user={}）",
            self.host.user
        )))
    }

    pub async fn disconnect(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "", "en")
                .await;
        }
        *self.state.lock().unwrap() = ConnectionState::Disconnected;
    }

    /// 重连（§49：reconnect on demand）。
    pub async fn reconnect(&mut self) -> Result<HostKeyDecision, SshError> {
        *self.state.lock().unwrap() = ConnectionState::Reconnecting;
        let _ = self.disconnect().await;
        let result = self.connect().await;
        if result.is_ok() {
            *self.state.lock().unwrap() = ConnectionState::Connected;
        }
        result
    }

    /// 执行一条远端命令（§38：fresh exec channel，不持久 shell 进程）。
    ///
    /// `cwd`/`env` 由调用方决定如何注入（R1 起由 ShellSessionState 驱动，
    /// 通过命令前缀或 env 传递）。`cancel` 可选：取消时 best-effort 关闭
    /// channel（§39：无法保证远端 descendants 已终止，如实记录）。
    pub async fn exec(
        &mut self,
        command: &str,
        cwd: Option<&str>,
        env: &std::collections::HashMap<String, String>,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<ExecResult, SshError> {
        let session = self.session.as_mut().ok_or(SshError::NotConnected)?;
        let mut channel = session
            .channel_open_session()
            .await
            .map_err(|e| SshError::Exec(e.to_string()))?;
        // cwd 通过 cd 前缀；env 通过 export 前缀（远端 shell 每次 fresh）。
        // 注意：`shell_quote` 已产出 `'value'`，不能再套 `{:?}`（Debug 引号会
        // 变成字面量进入值，且 `"..."` 内 `$` 会被远端 shell 展开）。
        // ISSUE-039：env key 同样 shell_quote（远端 env 变量名可能含 shell
        // 元字符，未引用直接拼进 export 前缀可被展开执行）。
        let mut prefix = String::new();
        for (k, v) in env {
            prefix.push_str(&format!("export {}={}; ", shell_quote(k), shell_quote(v)));
        }
        if let Some(dir) = cwd {
            prefix.push_str(&format!("cd {} || exit 127; ", shell_quote(dir)));
        }
        let full = format!("{prefix}{command}");
        channel
            .exec(true, full.clone())
            .await
            .map_err(|e| SshError::Exec(e.to_string()))?;
        let mut result = ExecResult {
            exit_code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
        };
        // ISSUE-008：传输层硬上限（与本地 MAX_OUTPUT_BUDGET 一致）。超限后
        // 继续读会耗尽内存；截断后丢弃剩余数据，只标记 truncated 由调用方展示。
        const REMOTE_EXEC_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
        loop {
            let msg = if let Some(cancel) = cancel {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        // §39：best-effort 取消——关闭 channel 通知远端，
                        // 但不保证远端 descendants 已终止。
                        drop(channel);
                        return Err(SshError::Exec("cancelled".into()));
                    }
                    msg = channel.wait() => msg,
                }
            } else {
                channel.wait().await
            };
            let Some(msg) = msg else {
                break;
            };
            use russh::ChannelMsg;
            match msg {
                ChannelMsg::Data { ref data } => {
                    if result.stdout.len() < REMOTE_EXEC_OUTPUT_LIMIT {
                        let room = REMOTE_EXEC_OUTPUT_LIMIT - result.stdout.len();
                        result
                            .stdout
                            .extend_from_slice(&data[..data.len().min(room)]);
                        if data.len() > room {
                            result.truncated = true;
                        }
                    } else {
                        result.truncated = true;
                    }
                }
                ChannelMsg::ExtendedData { ref data, .. } => {
                    if result.stderr.len() < REMOTE_EXEC_OUTPUT_LIMIT {
                        let room = REMOTE_EXEC_OUTPUT_LIMIT - result.stderr.len();
                        result
                            .stderr
                            .extend_from_slice(&data[..data.len().min(room)]);
                        if data.len() > room {
                            result.truncated = true;
                        }
                    } else {
                        result.truncated = true;
                    }
                }
                ChannelMsg::ExitStatus { exit_status } => result.exit_code = Some(exit_status),
                _ => {}
            }
        }
        Ok(result)
    }

    /// 打开 SFTP 会话（每次调用新建 channel；复用 transport 连接）。
    async fn sftp(&mut self) -> Result<russh_sftp::client::SftpSession, SshError> {
        let session = self.session.as_mut().ok_or(SshError::NotConnected)?;
        let channel = session
            .channel_open_session()
            .await
            .map_err(|e| SshError::Sftp(e.to_string()))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| SshError::Sftp(e.to_string()))?;
        russh_sftp::client::SftpSession::new(channel.into_stream())
            .await
            .map_err(sftp_error)
    }

    /// 读取远端文件（§R0 primitive；R2 接入 read 工具）。
    /// ISSUE-008：有界读取——先 stat 拒绝超大文件，再 `.take()` 兜底防 TOCTOU
    /// 超限分配（与本地 `MAX_SNAPSHOT_BYTES` 一致）。
    pub async fn read_file(&mut self, path: &str) -> Result<Vec<u8>, SshError> {
        const REMOTE_FILE_LIMIT: u64 = 64 * 1024 * 1024;
        let sftp = self.sftp().await?;
        let meta = sftp.metadata(path).await.map_err(sftp_error)?;
        if meta.size.unwrap_or(0) > REMOTE_FILE_LIMIT {
            return Err(SshError::Sftp(format!(
                "远端文件超过 {REMOTE_FILE_LIMIT} 字节上限：{path}"
            )));
        }
        let file = sftp.open(path).await.map_err(sftp_error)?;
        let mut bytes = Vec::new();
        use tokio::io::AsyncReadExt;
        file.take(REMOTE_FILE_LIMIT.saturating_add(1))
            .read_to_end(&mut bytes)
            .await
            .map_err(SshError::Io)?;
        if bytes.len() as u64 > REMOTE_FILE_LIMIT {
            return Err(SshError::Sftp(format!(
                "远端文件超过 {REMOTE_FILE_LIMIT} 字节上限：{path}"
            )));
        }
        Ok(bytes)
    }

    /// 写入远端文件（§44：temp + atomic rename；目标已存在则覆盖）。
    pub async fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), SshError> {
        let sftp = self.sftp().await?;
        let tmp = format!("{path}.tpi-tmp-{}", uuid::Uuid::now_v7().simple());
        let mut file = sftp
            .open_with_flags(
                &tmp,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
            )
            .await
            .map_err(sftp_error)?;
        use tokio::io::AsyncWriteExt;
        file.write_all(bytes).await.map_err(SshError::Io)?;
        file.close().await.map_err(SshError::Io)?;
        // atomic rename（覆盖目标；远端 FS 不支持时由调用方降级——任务书 §44）。
        // ISSUE-023：rename 失败时尽力清理 temp，避免远端累积 `.tpi-tmp-*` 垃圾。
        if let Err(e) = sftp.rename(&tmp, path).await {
            let _ = sftp.remove_file(&tmp).await;
            return Err(SshError::Sftp(format!("rename {} -> {}: {e}", tmp, path)));
        }
        Ok(())
    }

    /// 远端文件元数据（R3 list 用）。
    pub async fn stat(&mut self, path: &str) -> Result<FileAttributes, SshError> {
        let sftp = self.sftp().await?;
        sftp.metadata(path).await.map_err(sftp_error)
    }

    /// 列出远端目录（R3 list 用）：返回 (名称, 是否目录)。
    pub async fn read_dir(&mut self, path: &str) -> Result<Vec<(String, bool)>, SshError> {
        let sftp = self.sftp().await?;
        let entries = sftp.read_dir(path).await.map_err(sftp_error)?;
        Ok(entries
            .map(|entry| (entry.file_name(), entry.file_type().is_dir()))
            .collect())
    }

    /// 删除远端文件（edit/write 回滚用）。
    pub async fn remove_file(&mut self, path: &str) -> Result<(), SshError> {
        let sftp = self.sftp().await?;
        sftp.remove_file(path).await.map_err(sftp_error)
    }
}

/// 把 russh-sftp 的 client error 归类为 [`SshError`]，保留 SFTP 状态码语义：
/// 明确返回 SSH_FX_NO_SUCH_FILE(2) 的才算"文件不存在"，其余（权限/网络/协议）
/// 都归为普通 SFTP 错误。调用方据此区分"文件确实不存在"与"无法确认文件状态"，
/// 不得把网络抖动当作文件不存在处理（否则会跳过 revision 校验覆盖远端文件）。
fn sftp_error(error: russh_sftp::client::error::Error) -> SshError {
    match &error {
        russh_sftp::client::error::Error::Status(status)
            if status.status_code == russh_sftp::protocol::StatusCode::NoSuchFile =>
        {
            SshError::SftpNoSuchFile(error.to_string())
        }
        _ => SshError::Sftp(error.to_string()),
    }
}

/// shell 单引号引用（防注入：远端命令内嵌 cwd/env 时）。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn remote_host_direct_has_defaults() {
        let host = RemoteHost::direct("192.168.1.10", 22, "dev");
        assert_eq!(host.hostname, "192.168.1.10");
        assert_eq!(host.port, 22);
        assert_eq!(host.user, "dev");
        assert!(host.strict_host_key_checking);
    }

    /// ISSUE-002：只有 SFTP 明确返回 NoSuchFile 才算"文件不存在"；
    /// 权限/网络/其他状态码错误都不得被误判（否则 remote_write 会在网络
    /// 抖动时跳过 revision 校验覆盖远端文件）。
    #[test]
    fn sftp_error_classifies_only_no_such_file() {
        use russh_sftp::client::error::Error as SftpClientError;
        use russh_sftp::protocol::{Status, StatusCode};

        let no_such = SshError::SftpNoSuchFile("no such file".into());
        assert!(
            matches!(&no_such, SshError::SftpNoSuchFile(_)),
            "NoSuchFile 必须单独分类"
        );

        // 权限错误 → 普通 Sftp 错误（不是 no_such_file）。
        let permission = SftpClientError::Status(Status {
            id: 1,
            status_code: StatusCode::PermissionDenied,
            error_message: "denied".into(),
            language_tag: String::new(),
        });
        assert!(matches!(sftp_error(permission), SshError::Sftp(_)));

        // 连接类错误 → 普通 Sftp 错误。
        assert!(matches!(
            sftp_error(SftpClientError::IO("connection lost".into())),
            SshError::Sftp(_)
        ));

        // 明确 NoSuchFile → SftpNoSuchFile。
        let no_such_status = SftpClientError::Status(Status {
            id: 1,
            status_code: StatusCode::NoSuchFile,
            error_message: "no such file".into(),
            language_tag: String::new(),
        });
        assert!(matches!(
            sftp_error(no_such_status),
            SshError::SftpNoSuchFile(_)
        ));
    }
}
