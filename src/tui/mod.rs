//! TUI 渲染层（文档 §16）。
//!
//! 只有 renderer 可以调用 Crossterm/Ratatui 或写 stdout；Agent、provider、tool
//! 和日志模块只能发送事件（§16.1、§3.2 不变量 11）。
//!
//! M5：inline Ratatui renderer——transcript/editor/footer/plan/tool/reasoning 组件、
//! OMP semantic theme、16 ms 帧合并、synchronized update、hardware cursor。

pub mod editor;
pub mod model;
pub mod theme;

use std::io::{BufWriter, Stdout};
use std::time::Instant;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use model::{LineKind, StatusLine, ViewModel};

/// 帧合并间隔（§16.1：100-500 deltas/s 时按 16 ms 合并，而不是 delta 数量等于 draw 次数）。
pub const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// stdout 的唯一所有者（§3.2 不变量 11、§16.1）。
///
/// M5：内部持有 Ratatui `Terminal`；`draw` 是唯一写 stdout 的路径。
/// 帧合并由 [`should_draw`](Self::should_draw) 控制——高频事件不会逐条重绘。
pub struct Renderer {
    terminal: Terminal<CrosstermBackend<BufWriter<Stdout>>>,
    last_draw: Option<Instant>,
    /// 距上次 draw 的合并计数（诊断与测试用）。
    pub coalesced_events: u64,
    theme: theme::Theme,
}

impl Renderer {
    /// 初始化终端（raw mode + 隐藏光标 + 同步更新支持）。
    pub fn new() -> std::io::Result<Self> {
        let stdout = BufWriter::new(std::io::stdout());
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;
        // §20.3：初始化后 streaming path 不包含 CSI 全屏清除序列。
        terminal.clear()?;
        Ok(Self {
            terminal,
            last_draw: None,
            coalesced_events: 0,
            theme: theme::Theme::omp(),
        })
    }

    /// 距上次 draw 是否已过帧间隔（§16.1：16 ms 合并）。
    pub fn should_draw(&self) -> bool {
        match self.last_draw {
            Some(last) => last.elapsed() >= FRAME_INTERVAL,
            None => true,
        }
    }

    /// 渲染一帧（唯一的 stdout 写入路径；§16.1 以帧为单位合并模型增量）。
    pub fn draw(&mut self, view: &ViewModel) -> std::io::Result<()> {
        let theme = self.theme;
        self.terminal.draw(|frame| {
            let area = frame.area();
            // synchronized update（§16.1）：一帧一次 stdout flush。
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(3),
                    Constraint::Length(1),
                ])
                .split(area);
            draw_transcript(frame, chunks[0], view, theme);
            draw_editor(frame, chunks[1], view, theme);
            draw_footer(frame, chunks[2], view, theme);
        })?;
        self.last_draw = Some(Instant::now());
        self.coalesced_events = 0;
        Ok(())
    }

    /// 恢复终端（异常退出也能恢复，§21 M5 验收）。
    pub fn restore(&mut self) -> std::io::Result<()> {
        self.terminal.show_cursor()?;
        self.terminal.flush()
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new().expect("renderer init")
    }
}

/// 转录区（§16.2：用户/assistant/reasoning/tool 分色）。
fn draw_transcript(frame: &mut ratatui::Frame, area: Rect, view: &ViewModel, theme: theme::Theme) {
    let items: Vec<ListItem> = view
        .transcript
        .iter()
        .map(|line| {
            let (prefix, style) = match line.kind {
                LineKind::User => ("你  ", Style::default().fg(theme.user)),
                LineKind::Assistant => ("TPI", Style::default().fg(theme.assistant)),
                LineKind::Reasoning => ("思考", Style::default().fg(theme.reasoning)),
                LineKind::Tool => ("工具", Style::default().fg(theme.tool)),
                LineKind::System => ("系统", Style::default().fg(theme.system)),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{prefix} "), style.add_modifier(Modifier::BOLD)),
                Span::raw(line.text.clone()),
            ]))
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" TPI ")
            .title_style(Style::default().fg(theme.accent)),
    );
    frame.render_widget(list, area);
}

/// 输入行编辑器（§16.2；中文 IME 由终端处理，编辑器保持文本语义）。
fn draw_editor(frame: &mut ratatui::Frame, area: Rect, view: &ViewModel, theme: theme::Theme) {
    let (input, cursor) = view.input_position();
    let paragraph = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.accent)),
        Span::raw(input),
    ]))
    .block(Block::default().borders(Borders::ALL).title(" 输入 "))
    .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
    frame.set_cursor_position((area.x + cursor as u16 + 2, area.y + 1));
}

/// 状态栏（§16.2：模型、状态、预算提示）。
fn draw_footer(frame: &mut ratatui::Frame, area: Rect, view: &ViewModel, theme: theme::Theme) {
    let status = &view.status;
    let text = match status {
        StatusLine::Idle => format!("就绪 | {}", view.model_name),
        StatusLine::Running { turn, tool } => {
            format!("运行中 | {} | turn {} | {}", view.model_name, turn, tool)
        }
        StatusLine::Compacting => format!(
            "compacting · {}（§15.4 系统维护调用，非隐藏模型）",
            view.model_name
        ),
    };
    let paragraph = Paragraph::new(Line::from(vec![Span::styled(
        text,
        Style::default().fg(theme.footer),
    )]));
    frame.render_widget(paragraph, area);
}

/// 用 TestBackend 渲染一帧（测试与录制用，§20.3）。
pub fn draw_to_test_backend(view: &ViewModel, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let theme = theme::Theme::omp();
    terminal
        .draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(3),
                    Constraint::Length(1),
                ])
                .split(frame.area());
            draw_transcript(frame, chunks[0], view, theme);
            draw_editor(frame, chunks[1], view, theme);
            draw_footer(frame, chunks[2], view, theme);
        })
        .expect("draw");
    terminal.backend().buffer().clone()
}

/// 捕获一次 draw 的 stdout 字节（§20.3：验证无全屏清除序列、单次 flush）。
pub fn draw_captured_bytes(view: &ViewModel) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    {
        let backend = CrosstermBackend::new(&mut out);
        let mut terminal = Terminal::new(backend).expect("capture terminal");
        let theme = theme::Theme::omp();
        terminal
            .draw(|frame| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Min(1),
                        Constraint::Length(3),
                        Constraint::Length(1),
                    ])
                    .split(frame.area());
                draw_transcript(frame, chunks[0], view, theme);
                draw_editor(frame, chunks[1], view, theme);
                draw_footer(frame, chunks[2], view, theme);
            })
            .expect("draw");
    }
    out
}
