//! 进程层（文档 §11）。
//!
//! M1：direct process（tokio Command）。
//! M2：单二进制 process-host handshake + Job Object 进程树取消（§11.5）+ 受控 launcher。

pub mod host;

use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::tool::command::RunArgs;

/// process-host 的控制/数据消息。
const MSG_START: u8 = 0;
const MSG_OUTPUT: u8 = 1;
const MSG_EXIT: u8 = 2;

/// host 输出中的流标识。
pub const STREAM_STDOUT: u8 = 0;
pub const STREAM_STDERR: u8 = 1;

/// 进程结束方式（§11.3：timeout 和 cancellation 是独立状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Exited,
    Cancelled,
    TimedOut,
}

/// 通过 process-host 执行命令的结果。
///
/// `stdout`/`stderr` 只保留有界 tail（模型预算，§8.4）；完整输出在 artifact。
pub struct HostRunOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_total: u64,
    pub stderr_total: u64,
    /// 结束方式（§11.3：timeout/cancellation 独立于 exit code）。
    pub ended_by: EndReason,
    /// 实际使用的 launcher（§11.1：`.cmd/.bat` 标记 `cmd-script`）。
    pub launcher: Option<&'static str>,
}

/// 模型侧输出预算（§8.4：run/bash 24 KiB，保留错误相关 tail）。
pub const OUTPUT_BUDGET: usize = 24 * 1024;

fn append_bounded(buffer: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() >= OUTPUT_BUDGET {
        buffer.clear();
        buffer.extend_from_slice(&bytes[bytes.len() - OUTPUT_BUDGET..]);
    } else {
        let total = buffer.len() + bytes.len();
        if total > OUTPUT_BUDGET {
            buffer.drain(..total - OUTPUT_BUDGET);
        }
        buffer.extend_from_slice(bytes);
    }
}

/// 进程隔离不可用（§11.5：归组受上层 Job 限制而失败，target 启动前返回）。
#[derive(Debug, thiserror::Error)]
#[error("process isolation unavailable: {0}")]
pub struct IsolationError(pub String);

/// 通过 process-host 执行（§11.5 握手协议）。
///
/// 1. 启动隐藏的 `tpi.exe __process-host`；
/// 2. 创建 Job Object（KILL_ON_JOB_CLOSE、不允许 breakaway），把 host 加入 Job；
/// 3. AssignProcessToJobObject 成功后才发送 process spec 和 start token；
/// 4. host 创建 target；target 及其后代自动继承 Job 归属；
/// 5. 取消/超时 → TerminateJobObject，host 与整棵进程树一起退出。
///
/// 归组失败时返回 [`IsolationError`]，绝不静默降级为 unmanaged process（§11.5）。
pub async fn run_in_host(
    args: &RunArgs,
    resolved_program: &PathBuf,
    launcher: Option<&'static str>,
    cancel: CancellationToken,
    timeout: std::time::Duration,
    _session_id: &str,
    mut artifact: Option<&mut crate::session::artifact::ArtifactWriter>,
) -> Result<HostRunOutput, String> {
    // 单二进制 process-host（§11.5）：默认用自身；测试用 TPI_PROCESS_HOST 指向真实 tpi.exe。
    let exe = std::env::var_os("TPI_PROCESS_HOST")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| "process-host executable unavailable".to_string())?;
    let mut host = Command::new(&exe)
        .arg("__process-host")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .creation_flags(create_no_window_flag())
        .spawn()
        .map_err(|e| format!("spawn process-host: {e}"))?;

    // §11.5 第 2-3 步：host 尚未创建 target 时归组。
    let job = Job::create().map_err(|e| IsolationError(e.to_string()).0)?;
    job.assign_process(
        host.id()
            .ok_or_else(|| "process-host pid unavailable".to_string())?,
    )
    .map_err(|e| IsolationError(format!("assign host to job: {e}")).0)?;

    let mut stdin = host
        .stdin
        .take()
        .ok_or_else(|| "process-host stdin unavailable".to_string())?;
    let mut stdout = host
        .stdout
        .take()
        .ok_or_else(|| "process-host stdout unavailable".to_string())?;

    // 发送 Start spec（framed：len + kind + payload）。
    let spec = serde_json::json!({
        "program": resolved_program,
        "args": args.args,
        "cwd": args.cwd,
        "env": args.env,
    });
    let payload = serde_json::to_vec(&spec).map_err(|e| format!("spec json: {e}"))?;
    let mut header = [0u8; 5];
    header[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    header[4] = MSG_START;
    stdin
        .write_all(&header)
        .await
        .map_err(|e| format!("write spec: {e}"))?;
    stdin
        .write_all(&payload)
        .await
        .map_err(|e| format!("write spec payload: {e}"))?;
    stdin
        .flush()
        .await
        .map_err(|e| format!("flush spec: {e}"))?;
    drop(stdin);

    let mut output = HostRunOutput {
        exit_code: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        stdout_total: 0,
        stderr_total: 0,
        ended_by: EndReason::Exited,
        launcher,
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let mut exited = false;
    let mut terminated = false;

    // 读取 framed 消息：Output / Exit。取消/超时 → TerminateJobObject（§11.5 第 5 步）。
    loop {
        tokio::select! {
            _ = cancel.cancelled(), if !terminated => {
                job.terminate(1);
                terminated = true;
                output.ended_by = EndReason::Cancelled;
                // host 已被终止，管道将 EOF；继续读完残余帧。
            }
            _ = tokio::time::sleep_until(deadline), if !terminated => {
                job.terminate(1);
                terminated = true;
                output.ended_by = EndReason::TimedOut;
            }
            read = read_frame(&mut stdout) => {
                match read {
                    Ok(Some((MSG_OUTPUT, payload))) if payload.len() >= 5 => {
                        let stream = payload[0];
                        let bytes = &payload[1..];
                        if let Some(writer) = artifact.as_mut() {
                            let _ = writer.write(
                                if stream == STREAM_STDOUT {
                                    "stdout"
                                } else {
                                    "stderr"
                                },
                                bytes,
                            );
                        }
                        if stream == STREAM_STDOUT {
                            output.stdout_total += bytes.len() as u64;
                            append_bounded(&mut output.stdout, bytes);
                        } else {
                            output.stderr_total += bytes.len() as u64;
                            append_bounded(&mut output.stderr, bytes);
                        }
                    }
                    Ok(Some((MSG_EXIT, payload))) if payload.len() >= 4 => {
                        let code = i32::from_le_bytes(
                            payload[..4]
                                .try_into()
                                .map_err(|_| "invalid exit payload".to_string())?,
                        );
                        output.exit_code = Some(code);
                        exited = true;
                    }
                    Ok(Some((kind, _))) => {
                        tracing::warn!(kind, "unknown process-host message");
                    }
                    Ok(None) => break, // EOF：host 已退出
                    Err(e) => {
                        tracing::warn!(error = %e, "process-host read error");
                        break;
                    }
                }
                if exited {
                    break;
                }
            }
        }
    }

    let _ = host.kill().await;
    let _ = host.wait().await;
    Ok(output)
}

/// 读取一条 framed 消息：`[u32 LE len][u8 kind][payload]`。
async fn read_frame<R: AsyncReadExt + Unpin>(
    stream: &mut R,
) -> Result<Option<(u8, Vec<u8>)>, String> {
    let mut header = [0u8; 5];
    let mut read = 0usize;
    while read < 5 {
        let n = stream
            .read(&mut header[read..])
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(None);
        }
        read += n;
    }
    let len = u32::from_le_bytes(
        header[..4]
            .try_into()
            .map_err(|_| "invalid frame header".to_string())?,
    ) as usize;
    let kind = header[4];
    if len > 64 * 1024 * 1024 {
        return Err("process-host frame too large".into());
    }
    let mut payload = vec![0u8; len];
    let mut read = 0usize;
    while read < len {
        let n = stream
            .read(&mut payload[read..])
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("process-host frame truncated".into());
        }
        read += n;
    }
    Ok(Some((kind, payload)))
}

#[cfg(windows)]
fn create_no_window_flag() -> u32 {
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    CREATE_NO_WINDOW
}

#[cfg(not(windows))]
fn create_no_window_flag() -> u32 {
    0
}

/// Windows Job Object 封装（§11.5）。
pub struct Job {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// HANDLE 是进程内可跨线程使用的值（CreateJobObjectW 的句柄语义），
// 允许 run_in_host 的 async future 跨 tokio 线程持有 Job。
#[cfg(windows)]
unsafe impl Send for Job {}

impl Job {
    #[cfg(windows)]
    pub fn create() -> std::io::Result<Self> {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::JobObjects::{
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        };
        let handle = unsafe {
            windows_sys::Win32::System::JobObjects::CreateJobObjectW(
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if handle == HANDLE::default() {
            return Err(std::io::Error::last_os_error());
        }
        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation:
                windows_sys::Win32::System::JobObjects::JOBOBJECT_BASIC_LIMIT_INFORMATION {
                    LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    ..Default::default()
                },
            ..Default::default()
        };
        // 不允许 breakaway：不设置 BREAKAWAY_OK / SILENT_BREAKAWAY_OK。
        let result = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if result == 0 {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    #[cfg(windows)]
    pub fn assign_process(&self, pid: u32) -> std::io::Result<()> {
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        use windows_sys::Win32::System::Threading::OpenProcess;
        let process_handle = unsafe {
            OpenProcess(
                windows_sys::Win32::System::Threading::PROCESS_SET_QUOTA
                    | windows_sys::Win32::System::Threading::PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        if process_handle == windows_sys::Win32::Foundation::HANDLE::default() {
            return Err(std::io::Error::last_os_error());
        }
        let result = unsafe { AssignProcessToJobObject(self.handle, process_handle) };
        unsafe { windows_sys::Win32::Foundation::CloseHandle(process_handle) };
        if result == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn terminate(&self, exit_code: u32) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.handle, exit_code);
        }
    }

    #[cfg(not(windows))]
    pub fn create() -> std::io::Result<Self> {
        Ok(Self {})
    }

    #[cfg(not(windows))]
    pub fn assign_process(&self, _pid: u32) -> std::io::Result<()> {
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn terminate(&self, _exit_code: u32) {}
}

impl Drop for Job {
    #[cfg(windows)]
    fn drop(&mut self) {
        // §11.5：关闭 job handle 触发 KILL_ON_JOB_CLOSE，父进程崩溃时整棵树退出。
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
    #[cfg(not(windows))]
    fn drop(&mut self) {}
}
