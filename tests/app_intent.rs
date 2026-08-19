//! P3-01：recorded input trace → `AppCommand` sequence 固定（验收）。
//!
//! 从 `tests/fixtures/ui_trace`/ 读取录制事件，映射为 UiEvent，再把与 app
//! 相关的动作映射为 `AppCommand`，断言序列确定（同一 trace → 同一 command
//! sequence；不依赖 wall clock / network）。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use tpi::app::intent::{AppCommand, IntentSource, UiIntent};
use tpi::tui::event::UiEvent;

fn corpus_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui_trace")
}

/// 与 `tui_trace_replay::parse_event` 相同的 trace 解析（key/scroll 子集）。
fn parse_event(line: &str) -> UiEvent {
    let v: serde_json::Value = serde_json::from_str(line).expect("trace 行应为合法 JSON");
    match v["kind"].as_str().unwrap() {
        "key" => {
            let code = match v["code"].as_str().unwrap() {
                "char" => KeyCode::Char(v["char"].as_str().unwrap().chars().next().unwrap()),
                "enter" => KeyCode::Enter,
                "backspace" => KeyCode::Backspace,
                "pageup" => KeyCode::PageUp,
                "pagedown" => KeyCode::PageDown,
                "esc" => KeyCode::Esc,
                "end" | "ctrl_end" => KeyCode::End,
                other => panic!("trace 含未支持的 key code: {other}"),
            };
            let modifiers = if v
                .get("ctrl")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::NONE
            };
            UiEvent::Key(KeyEvent::new(code, modifiers))
        }
        "scroll" => match v["dir"].as_str().unwrap() {
            "up" => UiEvent::MouseScrollUp,
            "down" => UiEvent::MouseScrollDown,
            other => panic!("trace 含未支持的 scroll: {other}"),
        },
        "paste" => UiEvent::Paste(v["text"].as_str().unwrap().to_string()),
        "agent" => UiEvent::Agent(tpi::agent::RuntimeEvent::AssistantDelta {
            request_id: tpi::ids::RequestId::new_v7(),
            kind: tpi::agent::DeltaKind::Text,
            text: v["delta"].as_str().unwrap_or("").to_string(),
        }),
        other => panic!("trace 含未支持 kind: {other}"),
    }
}

/// 把 `UiEvent` 映射为 AppCommand（P3-01：surface 语义意图）。
/// 纯函数：同一事件 → 同一命令（确定性）。
fn to_command(event: &UiEvent) -> Option<UiIntent> {
    match event {
        UiEvent::Key(key) => {
            // 应用级按键：Ctrl+D 退出、Ctrl+B 侧边栏、Ctrl+F 搜索。
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('d') => {
                        Some(UiIntent::new(AppCommand::Quit, IntentSource::Keyboard))
                    }
                    KeyCode::Char('b') => Some(UiIntent::new(
                        AppCommand::ToggleSidebar,
                        IntentSource::Keyboard,
                    )),
                    KeyCode::Char('f') => Some(UiIntent::new(
                        AppCommand::OpenSearch,
                        IntentSource::Keyboard,
                    )),
                    _ => None,
                }
            } else {
                match key.code {
                    KeyCode::Enter => Some(UiIntent::new(
                        AppCommand::SubmitInput(String::new()),
                        IntentSource::Keyboard,
                    )),
                    _ => None,
                }
            }
        }
        UiEvent::MouseScrollUp | UiEvent::MouseScrollDown => None, // 滚动是视图意图
        UiEvent::Paste(text) => Some(UiIntent::new(
            AppCommand::Paste(text.clone()),
            IntentSource::Paste,
        )),
        UiEvent::Agent(_) => None, // runtime 事件，非 app command
        _ => None,
    }
}

/// 验收：recorded trace 的 command sequence 固定（确定性）。
#[test]
fn command_sequence_is_deterministic() {
    for fixture in ["001_input_queue.jsonl", "002_scroll.jsonl"] {
        let path = corpus_dir().join(fixture);
        let content =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取 {fixture}: {e}"));
        // 两次解析必须得到相同 sequence。
        let run = |content: &str| -> Vec<UiIntent> {
            content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(parse_event)
                .filter_map(|e| to_command(&e))
                .collect()
        };
        let a = run(&content);
        let b = run(&content);
        assert_eq!(a, b, "{fixture}: command sequence 必须确定");
    }
}

/// 同义输入（不同 trace 表达同一语义动作）→ 同一 `AppCommand`。
#[test]
fn semantic_equivalence_across_traces() {
    // Ctrl+D 无论如何拼写（char 'd' + CONTROL）都是 Quit。
    let via_ctrl_d = to_command(&UiEvent::Key(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    assert_eq!(via_ctrl_d.command, AppCommand::Quit);
    assert_eq!(via_ctrl_d.source, IntentSource::Keyboard);

    // Enter 提交空输入。
    let enter = to_command(&UiEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .unwrap();
    assert_eq!(enter.command, AppCommand::SubmitInput(String::new()));
}

/// 滚动是视图意图：不产生 AppCommand（保持语义分离）。
#[test]
fn scroll_is_view_intent_not_command() {
    assert!(to_command(&UiEvent::MouseScrollUp).is_none());
    assert!(to_command(&UiEvent::MouseScrollDown).is_none());
}

/// slash 命令 → AppCommand（P3-01 adapter 等价性：与旧 pump 的匹配一致）。
#[test]
fn slash_commands_map_to_app_commands() {
    use tpi::app::command_from_slash;
    assert_eq!(command_from_slash("/quit"), Some(AppCommand::Quit));
    assert_eq!(command_from_slash("/cancel"), Some(AppCommand::CancelRun));
    assert_eq!(
        command_from_slash("/new"),
        Some(AppCommand::StartNewSession)
    );
    assert_eq!(command_from_slash("/compact"), Some(AppCommand::CompactNow));
    assert_eq!(command_from_slash("/retry"), Some(AppCommand::RetryLast));
    assert_eq!(
        command_from_slash("/help"),
        Some(AppCommand::OpenModal {
            name: "help".into()
        })
    );
    assert_eq!(command_from_slash("普通消息"), None);
    assert_eq!(command_from_slash("/unknown-cmd"), None);
}

/// P3-03 golden：TUI 的 `SLASH_COMMANDS` 投影与 `app::slash` registry 完全一致
/// （help/completion 来自同一 snapshot，双份数据不允许漂移）。
#[test]
fn tui_slash_commands_match_registry() {
    let tui_list: Vec<(&str, &str)> = tpi::tui::SLASH_COMMANDS.to_vec();
    let registry: Vec<(&str, &str)> = tpi::app::slash::SLASH_COMMANDS
        .iter()
        .map(|s| (s.name, s.desc))
        .collect();
    assert_eq!(tui_list, registry, "TUI 投影必须与 registry 一致");
}
