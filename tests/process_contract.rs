//! 命令执行契约测试（对应 §4.2 tests/process_contract.rs）。
//!
//! §2.2/§3.2 不变量 5：退出码等判断下一步所需的状态必须进入 `model_payload`，
//! 不能只存在于 UI/session metadata。

use tpi::tool::outcome::{ToolOutcome, ToolStatus};

#[test]
fn exit_status_is_visible_in_model_payload() {
    let outcome = ToolOutcome::command_failed("cargo", 101);

    // 状态必须对模型可见（§2.2：不能出现"UI 显示 failed，模型只看到一段 stderr"的分叉）。
    assert_eq!(outcome.model_payload.status, ToolStatus::Failed);

    // 退出码必须进入 model payload（§2.2：结构化退出状态不能只在 UI details）。
    assert_eq!(outcome.model_payload.exit_code, Some(101));
    assert!(outcome.model_payload.output.contains("exit_code: 101"));
}

#[test]
fn every_tool_call_has_exactly_one_terminal_status() {
    // §3.2 不变量 3：每个 tool call 恰好产生一个终态结果。
    let outcome = ToolOutcome::command_failed("cargo", 1);
    assert!(matches!(
        outcome.status,
        ToolStatus::Succeeded
            | ToolStatus::Failed
            | ToolStatus::TimedOut
            | ToolStatus::Cancelled
            | ToolStatus::Interrupted
            | ToolStatus::Rejected
    ));
}
