//! P3-04：Platform effects adapter——`AppEffect` 的执行边界。
//!
//! - [`PlatformEffects`] trait：clipboard/open URL/terminal title/file picker/
//!   notify，返回 `Result`（**错误反馈回 controller，不 `let _ =` 静默失败**）；
//! - [`LocalPlatformEffects`]：Windows/Linux 本地实现；
//! - [`apply_effect`]：执行单个 [`AppEffect`]，错误以 `Err(String)` 返回。
//!
//! 验收：Windows/Linux fake + 少量人工（fake 在测试中实现 trait 验证错误反馈）。

use crate::app::intent::AppEffect;

/// 平台副作用执行器（controller 之外的副作用边界）。
pub trait PlatformEffects {
    /// 复制文本到剪贴板。
    fn copy_to_clipboard(&self, text: &str) -> Result<(), String>;
    /// 打开 URL（仅 http/https；错误反馈，不静默）。
    fn open_url(&self, url: &str) -> Result<(), String>;
    /// 设置终端标题。
    fn set_terminal_title(&self, title: &str) -> Result<(), String>;
    /// 一次性通知（无错误路径；仅观测）。
    fn notify(&self, message: &str);
}

/// 本地实现（Windows/Linux）。
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalPlatformEffects;

impl PlatformEffects for LocalPlatformEffects {
    fn copy_to_clipboard(&self, text: &str) -> Result<(), String> {
        if crate::clipboard::set_text(text) {
            Ok(())
        } else {
            Err("剪贴板写入失败".into())
        }
    }

    fn open_url(&self, url: &str) -> Result<(), String> {
        let lower = url.trim().to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            return Err(format!("仅支持 http/https 链接，已拒绝打开：{url}"));
        }
        use std::process::Command;
        #[cfg(windows)]
        let result = Command::new("cmd").args(["/C", "start", "", url]).spawn();
        #[cfg(not(windows))]
        let result = Command::new("xdg-open").arg(url).spawn();
        result
            .map(|_| ())
            .map_err(|e| format!("打开 URL 失败: {e}"))
    }

    fn set_terminal_title(&self, title: &str) -> Result<(), String> {
        // 终端标题经 ANSI escape（OSC 0）；错误反馈（写入失败）。
        use std::io::Write;
        let mut out = std::io::stdout();
        write!(out, "\x1b]0;{title}\x07")
            .map(|()| ())
            .map_err(|e| format!("设置终端标题失败: {e}"))
    }

    fn notify(&self, message: &str) {
        // 一次性通知：状态栏/提示（本实现写入 stderr 调试；TUI adapter 覆盖）。
        eprintln!("[tpi] {message}");
    }
}

/// 执行一个 AppEffect（Draw 忽略；副作用经 `PlatformEffects` 执行，错误返回）。
pub fn apply_effect(platform: &dyn PlatformEffects, effect: &AppEffect) -> Result<(), String> {
    match effect {
        AppEffect::Draw => Ok(()), // 渲染由 surface adapter 处理
        AppEffect::CopyToClipboard(text) => platform.copy_to_clipboard(text),
        AppEffect::OpenUrl(url) => {
            // scheme 校验在 effects 边界统一执行（不依赖 platform 实现）。
            let lower = url.trim().to_ascii_lowercase();
            if !(lower.starts_with("http://") || lower.starts_with("https://")) {
                return Err(format!("仅支持 http/https 链接，已拒绝打开：{url}"));
            }
            platform.open_url(url)
        }
        AppEffect::SetTerminalTitle(title) => platform.set_terminal_title(title),
        AppEffect::OpenFilePicker { filter } => {
            // file picker 无本地实现：反馈未支持（不静默）。
            let _ = filter;
            Err("文件选择器未在本地实现（P3-04 仅留接口）".into())
        }
        AppEffect::Notify(message) => {
            platform.notify(message);
            Ok(())
        }
    }
}
