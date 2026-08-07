//! Windows executable resolver（§11.1）。
//!
//! 按 PATH/PATHEXT 找到实际目标并记录；`.cmd/.bat` 本质需要 `cmd.exe`，
//! 用受控 batch handling 执行独立参数，标记 `launcher=cmd-script`，
//! 不得伪装成原生 executable，也不得接受一整段 CMD 命令字符串（§11.1）。

use std::path::{Path, PathBuf};

/// 解析结果：实际程序路径 + launcher 标记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProgram {
    pub path: PathBuf,
    pub launcher: Option<&'static str>,
}

/// 解析 program 为实际可执行目标（Windows：PATH + PATHEXT；Unix：PATH）。
pub fn resolve(program: &str) -> ResolvedProgram {
    #[cfg(windows)]
    {
        resolve_windows(program)
    }
    #[cfg(not(windows))]
    {
        resolve_unix(program)
    }
}

#[cfg(windows)]
fn resolve_windows(program: &str) -> ResolvedProgram {
    let program_path = Path::new(program);
    let pathext: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect();

    let candidate = |path: &Path| -> Option<ResolvedProgram> {
        if !path.exists() {
            return None;
        }
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase());
        if let Some(ext) = ext
            && (ext == "bat" || ext == "cmd")
        {
            return Some(ResolvedProgram {
                path: path.to_path_buf(),
                launcher: Some("cmd-script"),
            });
        }
        Some(ResolvedProgram {
            path: path.to_path_buf(),
            launcher: None,
        })
    };

    // 显式路径（含分隔符或绝对路径）。
    if program_path.components().count() > 1 || program_path.is_absolute() {
        // 无扩展名时尝试 PATHEXT 补全。
        for ext in &pathext {
            let with_ext = program_path.with_extension(ext.trim_start_matches('.'));
            if let Some(resolved) = candidate(&with_ext) {
                return resolved;
            }
        }
        if let Some(resolved) = candidate(program_path) {
            return resolved;
        }
        return ResolvedProgram {
            path: program_path.to_path_buf(),
            launcher: None,
        };
    }

    // 按 PATH 查找（Windows CreateProcess 语义）。
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let base = dir.join(program);
        for ext in &pathext {
            let candidate_path = if program.contains('.') {
                base.clone()
            } else {
                base.with_extension(ext.trim_start_matches('.'))
            };
            if let Some(resolved) = candidate(&candidate_path) {
                return resolved;
            }
        }
        // 无扩展名直接匹配（如 bash.exe 的 base 名）。
        if let Some(resolved) = candidate(&base) {
            return resolved;
        }
    }
    ResolvedProgram {
        path: program_path.to_path_buf(),
        launcher: None,
    }
}

#[cfg(not(windows))]
fn resolve_unix(program: &str) -> ResolvedProgram {
    let program_path = Path::new(program);
    if program_path.components().count() > 1 || program_path.is_absolute() {
        return ResolvedProgram {
            path: program_path.to_path_buf(),
            launcher: None,
        };
    }
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return ResolvedProgram {
                path: candidate,
                launcher: None,
            };
        }
    }
    ResolvedProgram {
        path: program_path.to_path_buf(),
        launcher: None,
    }
}

/// 受控 batch launcher（§11.1）：`cmd.exe /d /s /c ""<script>" <args...>"`。
///
/// 只接受独立参数数组，不接受一整段 CMD 命令字符串。
pub fn build_cmd_launcher(resolved: &Path, args: &[String]) -> (String, Vec<String>) {
    let mut command_args = Vec::with_capacity(args.len() + 3);
    command_args.push("/d".to_string());
    command_args.push("/s".to_string());
    command_args.push("/c".to_string());
    // cmd /c 引号规则：整条命令行包在引号内。
    let mut quoted = format!("\"{}\"", resolved.display());
    for arg in args {
        quoted.push(' ');
        quoted.push_str(&quote_cmd_arg(arg));
    }
    command_args.push(quoted);
    ("cmd.exe".to_string(), command_args)
}

fn quote_cmd_arg(arg: &str) -> String {
    // 简单保守转义：含空格/引号时包引号并转义内部引号。
    if arg.contains(' ') || arg.contains('"') || arg.is_empty() {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_launcher_keeps_arguments_individual() {
        let (program, args) = build_cmd_launcher(
            Path::new("C:\\tools\\script.cmd"),
            &["a b".to_string(), "c".to_string()],
        );
        assert_eq!(program, "cmd.exe");
        assert_eq!(args[0], "/d");
        assert_eq!(args[2], "/c");
        assert!(args[3].contains("script.cmd"));
        assert!(args[3].contains("\"a b\""));
    }
}
