//! Git Bash 子进程与 Windows Job Object 生命周期。
//!
//! M1：direct process（tokio Command）。
//! M2：单二进制 process-host handshake + Job Object 进程树取消（§11.5）+ 受控 launcher。

pub mod capture;
pub mod host;
pub mod managed;
pub mod supervisor;

use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::tool::command::RunArgs;

/// 实时输出回调（bash 执行中逐帧转发到 UI；由工具层构造，进程层不感知 UI 细节）。
pub type StreamSink = dyn Fn(u8, &[u8]) + Sync;

/// process-host 的控制/数据消息。
const MSG_START: u8 = 0;
const MSG_OUTPUT: u8 = 1;
const MSG_EXIT: u8 = 2;
/// P2：host 成功 spawn target 后发送（payload = target pid LE bytes）。
/// 前台调用忽略；后台启动用它确认“进程已真正启动”（区分 spawn 失败 Exit(-2)）。
const MSG_STARTED: u8 = 3;

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
    /// 状态捕获段内容（`capture_nonce` 指定时；BEGIN..END 之间的原始字节，
    /// 已从 stdout/artifact/UI 剥离，任务书 §22）。
    pub capture: Option<Vec<u8>>,
}

fn terminal_without_start(ended_by: EndReason, launcher: Option<&'static str>) -> HostRunOutput {
    HostRunOutput {
        exit_code: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        stdout_total: 0,
        stderr_total: 0,
        ended_by,
        launcher,
        capture: None,
    }
}

/// 模型侧输出预算（§8.4：run/bash 24 KiB，保留错误相关 tail）。
pub const OUTPUT_BUDGET: usize = 24 * 1024;
const MAX_OUTPUT_BUDGET: usize = 16 * 1024 * 1024;

fn append_bounded(buffer: &mut Vec<u8>, bytes: &[u8], budget: usize) {
    if bytes.len() >= budget {
        buffer.clear();
        buffer.extend_from_slice(&bytes[bytes.len() - budget..]);
    } else {
        let total = buffer.len() + bytes.len();
        if total > budget {
            buffer.drain(..total - budget);
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
pub struct HostRunRequest<'a> {
    pub args: &'a RunArgs,
    pub resolved_program: &'a PathBuf,
    pub launcher: Option<&'static str>,
    pub cancel: CancellationToken,
    pub timeout: std::time::Duration,
    /// stdout/stderr 各自保留的 tail 预算；完整输出仍可选写入 artifact。
    pub output_budget: usize,
    pub artifact: Option<&'a mut tpi_session::artifact::ArtifactWriter>,
    pub stream_sink: Option<&'a StreamSink>,
    /// 可选：状态捕获 nonce（§22）。设置后从 stdout 剥离 BEGIN/END 包裹的
    /// 捕获段到 [`HostRunOutput::capture`]，不进模型输出/artifact/UI。
    pub capture_nonce: Option<&'a str>,
}

pub async fn run_in_host(request: HostRunRequest<'_>) -> Result<HostRunOutput, String> {
    let HostRunRequest {
        args,
        resolved_program,
        launcher,
        cancel,
        timeout,
        output_budget,
        mut artifact,
        stream_sink,
        capture_nonce,
    } = request;
    if output_budget == 0 || output_budget > MAX_OUTPUT_BUDGET {
        return Err(format!(
            "process output budget must be in 1..={MAX_OUTPUT_BUDGET}"
        ));
    }
    if cancel.is_cancelled() {
        return Ok(terminal_without_start(EndReason::Cancelled, launcher));
    }
    let deadline = tokio::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "command timeout exceeds platform clock range".to_string())?;
    // 单二进制 process-host（§11.5）：默认用自身；测试用 TPI_PROCESS_HOST 指向真实 tpi.exe。
    let exe = std::env::var_os("TPI_PROCESS_HOST")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| "process-host executable unavailable".to_string())?;
    let mut host = Command::new(&exe)
        .arg("__process-host")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // §PointerHit 12：host 诊断不 inherit（TUI 下会污染终端）——
        // 改 piped 由下方 pump 转发到 tracing 日志（§19.2）。
        .stderr(std::process::Stdio::piped())
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

    // host 自身的诊断输出（非 target 的 stderr）转发到 tracing 日志，不写终端。
    // ISSUE-025：不遗留 detached spawn（ADR-006）——Drop 守卫在任何提前返回
    // 路径（Err/取消）都 abort 该任务；正常路径 host 死亡后管道 EOF 自然结束，
    // abort 只是兜底（最多丢几条 host 诊断，无副作用）。
    if let Some(host_stderr) = host.stderr.take() {
        struct StderrForwarder(Option<tokio::task::JoinHandle<()>>);
        impl Drop for StderrForwarder {
            fn drop(&mut self) {
                if let Some(handle) = self.0.take() {
                    handle.abort();
                }
            }
        }
        let _stderr_guard = StderrForwarder(Some(tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(host_stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(line = %line, "process-host stderr");
            }
        })));
    }

    // 归组期间可能已取消或耗尽 timeout；发送 Start 前再检查，避免目标进程
    // 获得哪怕一个短暂的副作用窗口。
    if cancel.is_cancelled() {
        return Ok(terminal_without_start(EndReason::Cancelled, launcher));
    }
    if tokio::time::Instant::now() >= deadline {
        return Ok(terminal_without_start(EndReason::TimedOut, launcher));
    }

    // 发送 Start spec（framed：len + kind + payload）。
    let spec = serde_json::json!({
        "program": resolved_program,
        "args": args.args,
        "cwd": args.cwd,
        "env": args.env,
        "env_remove": args.env_remove,
    });
    let payload = serde_json::to_vec(&spec).map_err(|e| format!("spec json: {e}"))?;
    let payload_len = u32::try_from(payload.len())
        .map_err(|_| "process-host start spec exceeds protocol limit".to_string())?;
    let mut header = [0u8; 5];
    header[..4].copy_from_slice(&payload_len.to_le_bytes());
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
        capture: None,
    };
    let mut exited = false;
    let mut terminated = false;
    // 捕获段剥离（§22）：capture_nonce 设置时从 stdout 剥离 control 段。
    let mut capture_scanner = capture_nonce.map(capture::CaptureScanner::new);

    // 读取 framed 消息：Output / Exit。取消/超时 → TerminateJobObject（§11.5 第 5 步）。
    loop {
        tokio::select! {
            _ = cancel.cancelled(), if !terminated => {
                job.terminate(1)
                    .map_err(|error| format!("terminate cancelled process tree: {error}"))?;
                terminated = true;
                output.ended_by = EndReason::Cancelled;
                // host 已被终止，管道将 EOF；继续读完残余帧。
            }
            _ = tokio::time::sleep_until(deadline), if !terminated => {
                job.terminate(1)
                    .map_err(|error| format!("terminate timed-out process tree: {error}"))?;
                terminated = true;
                output.ended_by = EndReason::TimedOut;
            }
            read = read_frame(&mut stdout) => {
                match read {
                    Ok(Some((MSG_STARTED, _))) => {
                        // P2：host 已成功 spawn target（后台确认信号）；前台调用忽略。
                        continue;
                    }
                    // payload = [stream][bytes]?子进程输出 1-3 字节的小块时
                    // 框长度为 2-4（正常有效）。此前要求 >= 5
                    // 会把这些小输出丢帧并刷“unknown process-host message kind=1”告警。
                    Ok(Some((MSG_OUTPUT, payload))) if !payload.is_empty() => {
                        let stream = payload[0];
                        if stream != STREAM_STDOUT && stream != STREAM_STDERR {
                            return Err(format!("invalid process-host stream id {stream}"));
                        }
                        let bytes = &payload[1..];
                        // §22：stdout 先过捕获段剥离器，control 段不进用户数据。
                        // `owned` 延迟初始化，仅在剥离分支使用（stderr 直接用 `bytes`）。
                        let owned;
                        let user_bytes: &[u8] = if stream == STREAM_STDOUT {
                            match capture_scanner.as_mut() {
                                Some(scanner) => {
                                    owned = scanner.feed(bytes);
                                    if owned.is_empty() {
                                        continue;
                                    }
                                    &owned
                                }
                                None => bytes,
                            }
                        } else {
                            bytes
                        };
                        if let Some(sink) = stream_sink {
                            // 实时转发（bash 执行中 UI 可见增量输出；同步回调不阻塞读循环）。
                            sink(stream, user_bytes);
                        }
                        if let Some(writer) = artifact.as_mut() {
                            writer.write(
                                if stream == STREAM_STDOUT {
                                    "stdout"
                                } else {
                                    "stderr"
                                },
                                user_bytes,
                            )
                            .map_err(|error| format!("write command artifact: {error}"))?;
                        }
                        if stream == STREAM_STDOUT {
                            output.stdout_total = output
                                .stdout_total
                                .saturating_add(user_bytes.len() as u64);
                            append_bounded(&mut output.stdout, user_bytes, output_budget);
                        } else {
                            output.stderr_total = output
                                .stderr_total
                                .saturating_add(user_bytes.len() as u64);
                            append_bounded(&mut output.stderr, user_bytes, output_budget);
                        }
                    }
                    Ok(Some((MSG_EXIT, payload))) if payload.len() == 4 => {
                        let code = i32::from_le_bytes(
                            payload[..4]
                                .try_into()
                                .map_err(|_| "invalid exit payload".to_string())?,
                        );
                        output.exit_code = Some(code);
                        exited = true;
                    }
                    Ok(Some((MSG_OUTPUT, _))) => {
                        return Err("invalid empty process-host output frame".into());
                    }
                    Ok(Some((MSG_EXIT, _))) => {
                        return Err("invalid process-host exit frame".into());
                    }
                    Ok(Some((kind, _))) => {
                        return Err(format!("unknown process-host message kind {kind}"));
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
    if !terminated && !exited {
        return Err("process-host exited without an Exit frame".into());
    }
    // 命令结束：flush 滞留的 stdout 用户数据（跨帧检测的尾部），再取捕获段。
    if let Some(scanner) = capture_scanner.as_mut() {
        let tail = scanner.finish();
        if !tail.is_empty() {
            if let Some(sink) = stream_sink {
                sink(STREAM_STDOUT, &tail);
            }
            if let Some(writer) = artifact.as_mut() {
                writer
                    .write("stdout", &tail)
                    .map_err(|error| format!("write command artifact: {error}"))?;
            }
            output.stdout_total = output.stdout_total.saturating_add(tail.len() as u64);
            append_bounded(&mut output.stdout, &tail, output_budget);
        }
        output.capture = scanner.take_capture();
    }
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
            return if read == 0 {
                Ok(None)
            } else {
                Err("process-host frame header truncated".into())
            };
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

// SAFETY: Job has unique ownership of a Windows kernel HANDLE. Windows kernel
// handles may be used and closed from any process thread; no thread-affine state
// or borrowed memory is stored in this wrapper.
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
        // SAFETY: Both optional pointer arguments are null, which requests an
        // unnamed job with the default security descriptor.
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
        // SAFETY: handle is live and owned by this function. `info` has the
        // exact structure and byte length required for this information class
        // and remains alive for the duration of the call.
        let result = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if result == 0 {
            let error = std::io::Error::last_os_error();
            // SAFETY: handle was returned successfully above, remains owned by
            // this function, and has not previously been closed.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(error);
        }
        Ok(Self { handle })
    }

    #[cfg(windows)]
    pub fn assign_process(&self, pid: u32) -> std::io::Result<()> {
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        use windows_sys::Win32::System::Threading::OpenProcess;
        // SAFETY: OpenProcess accepts any process id value and has no pointer
        // arguments; failure is represented by a null handle below.
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
        // SAFETY: self owns a live job handle and process_handle is live with
        // the access rights requested above.
        let result = unsafe { AssignProcessToJobObject(self.handle, process_handle) };
        let error = (result == 0).then(std::io::Error::last_os_error);
        // SAFETY: process_handle is locally owned and has not been closed yet.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(process_handle) };
        if let Some(error) = error {
            return Err(error);
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn terminate(&self, exit_code: u32) -> std::io::Result<()> {
        // SAFETY: self owns a live job handle for the lifetime of this call.
        let result = unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.handle, exit_code)
        };
        if result == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
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
    pub fn terminate(&self, _exit_code: u32) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Job {
    #[cfg(windows)]
    fn drop(&mut self) {
        // §11.5：关闭 job handle 触发 KILL_ON_JOB_CLOSE，父进程崩溃时整棵树退出。
        // SAFETY: Job uniquely owns this live handle and Drop runs exactly once.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
    #[cfg(not(windows))]
    fn drop(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// 回归：read_frame 必须能解析小于 5 字节 payload 的 MSG_OUTPUT 帧
    /// （子进程输出 1-3 字节时框长度 2-4；此前被当“unknown message”丢帧）。
    #[tokio::test]
    async fn read_frame_parses_tiny_output_frames() {
        // len=2 LE + kind=MSG_OUTPUT + payload=[0(stream), b'x']
        let wire = [2u8, 0, 0, 0, MSG_OUTPUT, 0, b'x'];
        let (mut tx, mut rx) = tokio::io::duplex(64);
        tx.write_all(&wire).await.unwrap();
        drop(tx);
        let frame = read_frame(&mut rx).await.unwrap().expect("frame");
        assert_eq!(frame.0, MSG_OUTPUT, "小帧 kind 必须保持 MSG_OUTPUT");
        assert_eq!(frame.1, vec![0, b'x'], "小帧 payload 不得丢失");
    }

    #[tokio::test]
    async fn read_frame_rejects_truncated_header_but_accepts_clean_eof() {
        let (mut tx, mut rx) = tokio::io::duplex(64);
        tx.write_all(&[1, 0]).await.unwrap();
        drop(tx);
        assert!(
            read_frame(&mut rx)
                .await
                .unwrap_err()
                .contains("header truncated")
        );

        let (tx, mut rx) = tokio::io::duplex(64);
        drop(tx);
        assert!(read_frame(&mut rx).await.unwrap().is_none());
    }
}
