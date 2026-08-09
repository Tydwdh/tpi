//! 终端生命周期（TPI_TUI_V2_TASK §29-31）：TerminalDriver。
//!
//! 负责 raw mode、alternate screen、bracketed paste、mouse capture、
//! cursor、draw、autoresize、restore 与 panic restore。
//! Renderer 不再直接触碰 crossterm 终端状态（§29）。

use std::io::{BufWriter, Stdout};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste};
use ratatui::crossterm::execute;

/// 视口模式（§1：默认 fullscreen；inline 仅为兼容模式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Fullscreen,
    Inline,
}

impl ViewMode {
    /// 从配置字符串解析（`[ui] mode`）；未知值回退 fullscreen。
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "inline" => ViewMode::Inline,
            _ => ViewMode::Fullscreen,
        }
    }
}

/// inline 模式的活动区高度（约 2/5 屏，随终端行数自适应；§16.1 保留）。
fn inline_activity_height(rows: u16) -> u16 {
    if rows < 12 {
        return rows.max(1);
    }
    let proportional = ((u32::from(rows) * 2) / 5) as u16;
    proportional
        .clamp(12, rows.saturating_sub(12).max(12))
        .min(rows)
}

/// 终端生命周期唯一所有者：初始化/重绘/恢复全在这里（§29）。
///
/// fullscreen：`EnterAlternateScreen` + `Viewport::Fullscreen`，draw 整个
/// 终端；退出时逆序恢复（show_cursor → 关闭 capture/paste → leave
/// alternate screen → disable raw mode），任何失败路径都尽力恢复（§30）。
pub struct TerminalDriver {
    terminal: Terminal<CrosstermBackend<BufWriter<Stdout>>>,
    mode: ViewMode,
    /// inline 模式的活动区高度（resize 时重建 viewport 用）。
    activity_rows: u16,
}

impl TerminalDriver {
    /// 初始化终端（§30 顺序：raw mode → alternate screen → bracketed paste
    /// → mouse capture → hide cursor → fullscreen viewport）。
    pub fn new(mode: ViewMode) -> std::io::Result<Self> {
        ratatui::crossterm::terminal::enable_raw_mode()?;
        let init = (|| {
            if mode == ViewMode::Fullscreen {
                execute!(
                    std::io::stdout(),
                    ratatui::crossterm::terminal::EnterAlternateScreen
                )?;
            }
            execute!(std::io::stdout(), EnableBracketedPaste)?;
            // §PointerHit：App-managed mouse 模式（单一选择）——启用点击/拖动
            // 捕获（?1000h ?1002h ?1006h），禁用 any-event（?1003h，不报告
            // 未按键的 hover）。注意：启用 1002 后**拖动由应用接收**，终端
            // 不会同时做原生文本选择——这是有意为之的单一选择归属，不依赖
            // "终端同时拖选"的假设。应用内拖选 + Ctrl+C 复制负责文本选择。
            let _ = std::io::Write::write_all(
                &mut std::io::stdout(),
                b"\x1b[?1003l\x1b[?1000h\x1b[?1002h\x1b[?1006h",
            );
            let (_, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
            let height = inline_activity_height(rows);
            let viewport = match mode {
                ViewMode::Fullscreen => ratatui::Viewport::Fullscreen,
                ViewMode::Inline => ratatui::Viewport::Inline(height),
            };
            let mut terminal = Terminal::with_options(
                CrosstermBackend::new(BufWriter::new(std::io::stdout())),
                ratatui::TerminalOptions { viewport },
            )?;
            terminal.hide_cursor()?;
            Ok((terminal, height))
        })();
        match init {
            Ok((terminal, activity_rows)) => Ok(Self {
                terminal,
                mode,
                activity_rows,
            }),
            Err(error) => {
                // 任何失败路径都尽力逆序恢复（§30）。
                Self::restore_global();
                Err(error)
            }
        }
    }

    pub fn mode(&self) -> ViewMode {
        self.mode
    }

    /// 渲染一帧（唯一的 stdout 写入路径）。
    pub fn draw(&mut self, f: impl FnOnce(&mut ratatui::Frame)) -> std::io::Result<()> {
        self.terminal.draw(f).map(|_| ())
    }

    /// 终端 resize 后重算布局。
    ///
    /// fullscreen：ratatui 的 `Viewport::Fullscreen` 自动跟随终端大小，
    /// 只需 autoresize；inline：Viewport::Inline 高度 resize 时保持不变，
    /// 需要按新行数重建 viewport（§16.1 保留行为）。
    pub fn autoresize(&mut self) -> std::io::Result<()> {
        self.terminal.autoresize()?;
        if self.mode == ViewMode::Inline {
            let (_, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
            let height = inline_activity_height(rows);
            if height != self.activity_rows {
                let terminal = Terminal::with_options(
                    CrosstermBackend::new(BufWriter::new(std::io::stdout())),
                    ratatui::TerminalOptions {
                        viewport: ratatui::Viewport::Inline(height),
                    },
                )?;
                self.terminal = terminal;
                self.terminal.hide_cursor()?;
                self.activity_rows = height;
            }
        }
        Ok(())
    }

    /// inline scrollback：把已闭合行提交到活动区上方（§16.1）。
    /// fullscreen 模式不应调用（全屏内直接绘制，无 scrollback 概念）。
    pub fn insert_before(
        &mut self,
        height: u16,
        f: impl FnOnce(&mut ratatui::buffer::Buffer),
    ) -> std::io::Result<()> {
        self.terminal.insert_before(height, f)
    }

    /// 逆序恢复终端（§30）：show_cursor → DisableMouseCapture →
    /// DisableBracketedPaste → LeaveAlternateScreen → disable_raw_mode。
    pub fn restore(&mut self) -> std::io::Result<()> {
        let mut first_error = self.terminal.show_cursor().err();
        if let Err(error) = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
        ) && first_error.is_none()
        {
            first_error = Some(error);
        }
        if self.mode == ViewMode::Fullscreen
            && let Err(error) = execute!(
                std::io::stdout(),
                ratatui::crossterm::terminal::LeaveAlternateScreen
            )
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Err(error) = self.terminal.flush()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Err(error) = ratatui::crossterm::terminal::disable_raw_mode()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        first_error.map_or(Ok(()), Err)
    }

    /// 尽力恢复全局终端状态（不依赖实例；panic hook 与初始化失败路径用）。
    pub fn restore_global() {
        use std::io::Write;
        let _ = execute!(
            std::io::stdout(),
            ratatui::crossterm::cursor::Show,
            DisableMouseCapture,
            DisableBracketedPaste,
            ratatui::crossterm::terminal::LeaveAlternateScreen,
            ratatui::crossterm::style::ResetColor,
        );
        let _ = std::io::stdout().flush();
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
    }
}

impl Drop for TerminalDriver {
    fn drop(&mut self) {
        // app 因错误提前返回时仍尽力还原终端（显式 restore 是正常路径；
        // 这里的重复调用安全且避免用户遗留在 raw mode，§31）。
        let _ = self.terminal.show_cursor();
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
        );
        if self.mode == ViewMode::Fullscreen {
            let _ = execute!(
                std::io::stdout(),
                ratatui::crossterm::terminal::LeaveAlternateScreen
            );
        }
        let _ = self.terminal.flush();
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_mode_parse_accepts_inline_and_defaults_to_fullscreen() {
        assert_eq!(ViewMode::parse("inline"), ViewMode::Inline);
        assert_eq!(ViewMode::parse("  INLINE "), ViewMode::Inline);
        assert_eq!(ViewMode::parse("fullscreen"), ViewMode::Fullscreen);
        assert_eq!(ViewMode::parse(""), ViewMode::Fullscreen);
        assert_eq!(ViewMode::parse("weird"), ViewMode::Fullscreen);
        assert_eq!(ViewMode::default(), ViewMode::Fullscreen);
    }

    #[test]
    fn inline_activity_height_scales_with_rows() {
        assert_eq!(inline_activity_height(24), 12); // (24*2)/5=9 → clamp 12
        assert_eq!(inline_activity_height(60), 24);
        assert_eq!(inline_activity_height(200), 80); // (200*2)/5=80
    }
}
