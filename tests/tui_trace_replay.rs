//! P0-04：recorded UI trace 回放测试。
//!
//! 从 `tests/fixtures/ui_trace`/ 读取录制的事件序列，逐行映射为 `UiEvent` 并
//! 依次应用 `reducer::update`，断言终态（见 manifest 的 `assert` 字段）。
//! 回放不依赖 wall clock / network（trace 无时间戳，逐行立即应用）。
//!
//! 这是 P6 TUI 组件化的回归网：组件化必须保持这些 trace 的语义不变。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use tpi::agent::{DeltaKind, RuntimeEvent};
use tpi::ids::RequestId;
use tpi::tui::event::UiEvent;
use tpi::tui::model::ViewModel;
use tpi::tui::reducer;
use tpi::tui::scroll::ScrollMode;
use tpi::tui::state::UiState;

fn corpus_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui_trace")
}

/// 把 trace 中的一行 JSON 映射为 UiEvent（当前支持子集，见 README.md）。
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
            other => panic!("trace 含未支持的 scroll dir: {other}"),
        },
        "paste" => UiEvent::Paste(v["text"].as_str().unwrap().to_string()),
        "agent" => UiEvent::Agent(RuntimeEvent::AssistantDelta {
            request_id: RequestId::new_v7(),
            kind: DeltaKind::Text,
            text: v["delta"].as_str().unwrap().to_string(),
        }),
        other => panic!("trace 含未支持的 event kind: {other}"),
    }
}

fn load_manifest() -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(corpus_dir().join("manifest.json")).unwrap())
        .unwrap()
}

#[test]
fn replay_all_traces() {
    let manifest = load_manifest();
    for t in manifest["traces"].as_array().unwrap() {
        let id = t["id"].as_str().unwrap();
        let file = t["file"].as_str().unwrap();
        let lines: Vec<String> = std::fs::read_to_string(corpus_dir().join(file))
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert!(!lines.is_empty(), "{id}: trace 为空");

        let mut state = UiState::new(ViewModel::default());
        for line in &lines {
            let event = parse_event(line);
            reducer::update(&mut state, event);
        }

        match t["assert"].as_str().unwrap() {
            "pending_messages" => {
                let expected: Vec<String> = t["expected"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e.as_str().unwrap().to_string())
                    .collect();
                assert_eq!(
                    state.pending_messages.iter().cloned().collect::<Vec<_>>(),
                    expected,
                    "{id}: pending_messages 与 expected 不一致"
                );
            }
            "scroll_mode_follow" => {
                assert_eq!(
                    state.view.scroll_mode,
                    ScrollMode::Follow,
                    "{id}: 回放结束后应恢复 follow"
                );
            }
            "no_panic" => {}
            other => panic!("manifest 含未支持的 assert 类型: {other}"),
        }
    }
}

#[test]
fn trace_files_match_manifest() {
    // 防止 trace 文件被手改后 manifest 失步：每个 trace 文件都存在且可解析。
    let manifest = load_manifest();
    for t in manifest["traces"].as_array().unwrap() {
        let file = t["file"].as_str().unwrap();
        let path = corpus_dir().join(file);
        assert!(path.exists(), "manifest 引用的 trace 文件缺失: {file}");
        for line in std::fs::read_to_string(&path).unwrap().lines() {
            let _ = parse_event(line); // 解析失败会 panic
        }
    }
}
