//! Provider trace（TPI_STABILIZATION_TASK §8）：真实 provider 出问题时的
//! 本地调试能力——`TPI_TRACE_PROVIDER=1` 时把每次请求的元数据写入
//! `~/.tpi/logs/provider-*.jsonl`。
//!
//! 记录：request_start / response_status / sse_event / tool_call_started /
//! tool_arguments_completed / finish / error。
//! 禁止记录：Authorization header、API key、凭据管理器内容。
//! `TPI_TRACE_PROVIDER=body` 时额外记录 request body（本地调试；可能含用户代码）。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// 当前 trace 模式：None = 关闭，Some(false) = 元数据，Some(true) = 含 body。
static TRACE_MODE: OnceLock<Option<bool>> = OnceLock::new();
static TRACE_FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();

fn log_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("TPI_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("logs");
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".tpi").join("logs")
}

/// TPI_TRACE_PROVIDER=1（元数据）或 =body（含 request body）时启用。
pub fn enabled() -> bool {
    mode().is_some()
}

/// 是否额外记录 request body。
pub fn include_body() -> bool {
    mode() == Some(true)
}

fn mode() -> Option<bool> {
    *TRACE_MODE.get_or_init(|| match std::env::var("TPI_TRACE_PROVIDER") {
        Ok(value) if value == "body" || value == "full" => Some(true),
        Ok(value) if value == "1" || value == "true" => Some(false),
        _ => None,
    })
}

fn writer() -> Option<&'static Mutex<File>> {
    if !enabled() {
        return None;
    }
    TRACE_FILE
        .get_or_init(|| {
            let dir = log_dir();
            std::fs::create_dir_all(&dir).ok();
            let path = dir.join(format!("provider-{}.jsonl", std::process::id()));
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => Some(Mutex::new(file)),
                Err(error) => {
                    // 无法打开日志文件时丢弃 trace（不阻塞主流程），并上报日志。
                    tracing::error!(error = %error, path = %path.display(), "provider trace 无法打开日志文件；trace 已禁用");
                    None
                }
            }
        })
        .as_ref()
}

/// 写入一条 trace 记录（JSON 单行）。
pub fn log(kind: &str, mut fields: serde_json::Map<String, serde_json::Value>) {
    let Some(writer) = writer() else { return };
    fields.insert(
        "ts_ms".into(),
        serde_json::json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        ),
    );
    fields.insert("kind".into(), serde_json::json!(kind));
    let line = serde_json::Value::Object(fields).to_string() + "\n";
    let mut guard = crate::util::lock_mutex(writer, "provider_trace");
    let _ = guard.write_all(line.as_bytes());
    let _ = guard.flush();
}
