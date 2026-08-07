//! `__process-host` 模式（§11.5 第 4 步）。
//!
//! host 在控制管道（stdin）上阻塞等待 Start spec；创建 target 后，
//! target 的 stdout/stderr 经 framed 消息转发到 stdout；target 退出后发 Exit 并退出。
//! host 自身是同步进程（无需 async runtime）。

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Deserialize;

use super::{MSG_EXIT, MSG_OUTPUT, MSG_START, STREAM_STDERR, STREAM_STDOUT};

#[derive(Deserialize)]
struct StartSpec {
    program: std::path::PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    env: std::collections::HashMap<String, String>,
}

/// 运行 process-host（返回进程退出码）。
pub fn run_host() -> i32 {
    let Some(spec) = read_start_spec() else {
        eprintln!("process-host: no start spec");
        return 2;
    };

    // 创建 target；stdout/stderr piped 给 host 转发。
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            // spawn 失败：发 Exit(码 -2，进程不可能返回的哨兵值) 并退出。
            let payload = (-2i32).to_le_bytes().to_vec();
            write_message(MSG_EXIT, &payload);
            eprintln!("process-host: spawn failed: {error}");
            return 1;
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // 转发线程：stdout → kind=MSG_OUTPUT/STREAM_STDOUT；stderr → STREAM_STDERR。
    // 每个 pump 退出时递增 done 计数，供 host 判断残余输出是否已转发完。
    let done = Arc::new(AtomicUsize::new(0));
    let stdout_handle = spawn_pump(stdout, STREAM_STDOUT, done.clone());
    let stderr_handle = spawn_pump(stderr, STREAM_STDERR, done.clone());

    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("process-host: wait failed: {error}");
            return 1;
        }
    };
    // target 已退出。若 target 派生的后台进程仍持有输出管道句柄，pump 线程不会 EOF；
    // 只等待有限时间收集残余输出，超时则放弃 join（进程退出时线程被终止）——
    // 否则 host 永远不发送 MSG_EXIT，主进程会干等到命令超时（§11.5）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while done.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if stdout_handle.is_finished() {
        let _ = stdout_handle.join();
    }
    if stderr_handle.is_finished() {
        let _ = stderr_handle.join();
    }

    let code = status.code().unwrap_or(1);
    let payload = code.to_le_bytes().to_vec();
    write_message(MSG_EXIT, &payload);
    code
}

/// 读取 Start spec（framed：`[u32 LE len][u8 kind][payload]`，kind 必须是 MSG_START）。
fn read_start_spec() -> Option<StartSpec> {
    let mut stdin = std::io::stdin().lock();
    let mut header = [0u8; 5];
    let mut read = 0usize;
    while read < 5 {
        match stdin.read(&mut header[read..]) {
            Ok(0) => return None,
            Ok(n) => read += n,
            Err(_) => return None,
        }
    }
    if header[4] != MSG_START {
        eprintln!("process-host: expected MSG_START, got {}", header[4]);
        return None;
    }
    let len = u32::from_le_bytes(header[..4].try_into().ok()?) as usize;
    if len > 16 * 1024 * 1024 {
        eprintln!("process-host: spec too large");
        return None;
    }
    let mut payload = vec![0u8; len];
    let mut read = 0usize;
    while read < len {
        match stdin.read(&mut payload[read..]) {
            Ok(0) => return None,
            Ok(n) => read += n,
            Err(_) => return None,
        }
    }
    serde_json::from_slice(&payload).ok()
}

/// 写一条 framed 消息到 stdout。
///
/// header 与 payload 必须**一次** write_all 完成：两个 pump 线程与主线程
/// 并发写同一 stdout，分两次调用会被其他线程的写入插到中间（帧撕裂），
/// 主进程 read_frame 会把拼接的帧解析成损坏数据。
fn write_message(kind: u8, payload: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    let mut message = Vec::with_capacity(5 + payload.len());
    message.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    message.push(kind);
    message.extend_from_slice(payload);
    let _ = stdout.write_all(&message);
    let _ = stdout.flush();
}

fn spawn_pump<R: std::io::Read + Send + 'static>(
    stream: Option<R>,
    stream_kind: u8,
    done: Arc<AtomicUsize>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Some(mut stream) = stream else {
            done.fetch_add(1, Ordering::SeqCst);
            return;
        };
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let mut payload = Vec::with_capacity(n + 1);
                    payload.push(stream_kind);
                    payload.extend_from_slice(&buffer[..n]);
                    write_message(MSG_OUTPUT, &payload);
                }
                Err(_) => break,
            }
        }
        done.fetch_add(1, Ordering::SeqCst);
    })
}
