//! `undo` 工具（§B1 Mutation Journal 的模型侧入口）。
//!
//! 模型可自行撤销/重做本 session 已提交的文件变更，不完全依赖 git：
//! journal 由 edit/write 成功提交时自动记录 before/after 快照，
//! undo 经 CAS（current==after → 恢复 before；任一 Conflict 整体拒绝）。
//!
//! 与 CLI `tpi undo` 共用同一 journal 数据源与 CAS 语义
//!（[`tpi_session::journal`]），差异仅在：
//! - 默认操作**当前 session** 的 journal（CLI 默认 workspace 最近有变更的会话）；
//! - journal Tainted 时拒绝（同 CLI 非 --force 路径）。
//!
//! 注意：undo/redo 成功后**不回写** journal——journal 是“正向编辑史”，
//! undo/redo 在其上做游标式回滚/重放；把撤销记为普通 mutation 会反转
//! redo_last 的方向性（详见下方 execute 内的设计说明，与 CLI/runtime 同语义）。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::{ModelPayload, ToolContext, ToolOutcome, ToolStatus};
use tpi_session::journal::CasVerdict;

/// `undo` 参数。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct UndoArgs {
    /// 操作：undo=撤销（默认）、redo=重做。
    #[serde(default)]
    pub action: Option<String>,
    /// 范围：last=最近一条 mutation（默认）、all=整个 session 全量。
    #[serde(default)]
    pub scope: Option<String>,
}

impl UndoArgs {
    fn action_is_redo(&self) -> bool {
        self.action.as_deref() == Some("redo")
    }
    fn scope_is_all(&self) -> bool {
        self.scope.as_deref() == Some("all")
    }
}

fn rejected(message: String) -> ToolOutcome {
    ToolOutcome::failed(
        "undo",
        ModelPayload {
            status: ToolStatus::Rejected,
            program: Some("undo".into()),
            exit_code: None,
            duration_ms: 0,
            output: message,
            effect: None,
            artifact: None,
        },
    )
}

fn failed(message: String) -> ToolOutcome {
    ToolOutcome::failed(
        "undo",
        ModelPayload {
            status: ToolStatus::Failed,
            program: Some("undo".into()),
            exit_code: Some(1),
            duration_ms: 0,
            output: message,
            effect: None,
            artifact: None,
        },
    )
}

/// 工具入口：加载当前 session 的 journal，按 action/scope 执行 CAS 回滚/重放。
pub fn undo(args: UndoArgs, ctx: &ToolContext) -> ToolOutcome {
    // 参数白名单校验：未知值明确拒绝而不是静默当默认。
    if let Some(action) = &args.action
        && action != "undo"
        && action != "redo"
    {
        return rejected(format!(
            "status: rejected\ntool: undo\nerror: invalid_arguments\n\n未知 action: {action:?}（可用: undo / redo）"
        ));
    }
    if let Some(scope) = &args.scope
        && scope != "last"
        && scope != "all"
    {
        return rejected(format!(
            "status: rejected\ntool: undo\nerror: invalid_arguments\n\n未知 scope: {scope:?}（可用: last / all）"
        ));
    }

    let jpath = tpi_session::journal::journal_path(&ctx.artifacts_root, &ctx.session_id);
    if !jpath.exists() {
        let verb = if args.action_is_redo() {
            "重做"
        } else {
            "撤销"
        };
        return rejected(format!(
            "status: rejected\ntool: undo\nerror: empty_journal\n\n当前 session 没有 journal 变更记录（无可{verb}的编辑）。"
        ));
    }
    let state = match tpi_session::journal::load_journal(&jpath) {
        Ok(state) => state,
        Err(e) => {
            return failed(format!(
                "status: failed\ntool: undo\nerror: journal_read\n\n{e}"
            ));
        }
    };
    // §B3：journal 损坏时拒绝 destructive 操作（同 CLI 非 --force 路径；
    // 工具侧不提供 force——损坏 journal 的修复是用户决策）。
    if let Err(e) = tpi_session::journal::assert_can_mutate(&state, false) {
        return rejected(format!(
            "status: rejected\ntool: undo\nerror: journal_tainted\n\njournal 损坏（{} 行无法解析）：{e}\n请用户运行 `tpi undo --force` 或先修复 journal。",
            state.corrupt_lines
        ));
    }
    if state.mutations.is_empty() {
        return rejected(
            "status: rejected\ntool: undo\nerror: empty_journal\n\n当前 session 的 journal 为空（没有已记录的文件变更）。".into(),
        );
    }

    let result = match (args.action_is_redo(), args.scope_is_all()) {
        (false, true) => {
            tpi_session::journal::undo_all(&state.mutations, ctx.workspace_root.as_std_path())
        }
        (false, false) => {
            tpi_session::journal::undo_last(&state.mutations, ctx.workspace_root.as_std_path())
        }
        (true, true) => {
            tpi_session::journal::redo_all(&state.mutations, ctx.workspace_root.as_std_path())
        }
        (true, false) => {
            tpi_session::journal::redo_last(&state.mutations, ctx.workspace_root.as_std_path())
        }
    };
    let verdicts = match result {
        Ok(verdicts) => verdicts,
        Err(e) => return failed(format!("status: failed\ntool: undo\nerror: {e}")),
    };

    // 输出逐文件判定 + 总体状态。Conflict = 未写任何文件（原子性保证）。
    let conflicts = verdicts
        .iter()
        .filter(|(_, v)| *v == CasVerdict::Conflict)
        .count();
    let applied = verdicts
        .iter()
        .filter(|(_, v)| *v == CasVerdict::Applied)
        .count();
    let already = verdicts
        .iter()
        .filter(|(_, v)| *v == CasVerdict::AlreadyDone)
        .count();
    let verb = if args.action_is_redo() {
        "redo"
    } else {
        "undo"
    };

    let mut output = format!(
        "status: {}\ntool: undo\naction: {verb}\napplied: {applied}\nalready_done: {already}\nconflicts: {conflicts}",
        if conflicts > 0 {
            "conflict"
        } else {
            "succeeded"
        },
    );
    for (path, verdict) in &verdicts {
        output.push_str(&format!("\n  {verdict:?} {path}"));
    }
    if conflicts > 0 {
        output.push_str(
            "\n\n存在 Conflict 文件：本次未写入任何文件（CAS 原子性）。\n\
             文件已被外部修改（如手动编辑或 bash 写入），journal 快照不再匹配当前内容。\n\
             请 read 确认现状后决定：直接 edit 修正，或让用户处理。",
        );
        return ToolOutcome::failed(
            "undo",
            ModelPayload {
                status: ToolStatus::Failed,
                program: Some("undo".into()),
                exit_code: Some(1),
                duration_ms: 0,
                output,
                effect: None,
                artifact: None,
            },
        );
    }
    if applied == 0 {
        output.push_str("\n\n所有文件均已是目标状态（幂等：无需变更）。");
    }
    // 设计说明：undo/redo 本身**不回写** journal——journal 是“正向编辑史”，
    // undo/redo 在其上做游标式回滚/重放；把撤销记为普通 mutation 会让
    // redo_last 的方向性反转（重做撤销 ≠ 重做原编辑），破坏 redo 链。
    // 与 CLI `tpi undo` / runtime `cmd_undo` 保持同一语义。
    ToolOutcome::succeeded("undo", output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::edit;
    use crate::tool::files::{self, ReadArgs};
    use camino::Utf8PathBuf;

    /// 构造与 files 测试同构的 ToolContext（journal 落在 tempdir/artifacts）。
    fn undo_ctx(dir: &tempfile::TempDir) -> ToolContext {
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let local = crate::workspace::LocalWorkspace::new(workspace.clone(), true);
        ToolContext {
            workspace_root: workspace.clone(),
            shell: local.shell.clone(),
            workspace: std::sync::Arc::new(std::sync::Mutex::new(
                crate::workspace::ActiveWorkspace::local(local),
            )),
            cancel: tokio_util::sync::CancellationToken::new(),
            artifacts_root: dir.path().join("artifacts"),
            session_id: "test-session".into(),
            call_id: tpi_core::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: Default::default(),
            shell_path: None,
            snapshot_store: Default::default(),
            current_plan: Default::default(),
            current_goal: None,
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            terminals: Default::default(),
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
            )),
            interactive: false,
            allow_outside_workspace: true,
            workspace_session: None,
        }
    }

    /// 用真实 edit 工具做一次变更（journal 自动记录）。
    fn do_edit(ctx: &ToolContext, path: &Utf8PathBuf, from: &str, to: &str) {
        std::fs::write(path.as_std_path(), from).unwrap();
        let read = files::read(
            ReadArgs {
                path: path.to_string(),
                start_line: 1,
                line_count: 10,
                depth: None,
            },
            ctx,
        );
        assert_eq!(read.status, ToolStatus::Succeeded);
        let plan = edit::prepare_commit(path);
        let outcome = files::edit(
            edit::EditArgs {
                path: path.to_string(),
                replacements: vec![edit::Replacement {
                    old_text: from.into(),
                    new_text: to.into(),
                }],
            },
            ctx,
            Some(&plan),
        );
        assert_eq!(
            outcome.status,
            ToolStatus::Succeeded,
            "{}",
            outcome.model_payload.output
        );
    }

    /// 端到端：edit → undo（工具路径）恢复 before；再 redo 恢复 after。
    #[test]
    fn undo_tool_restores_last_edit_and_redo_reapplies() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = undo_ctx(&dir);
        let target = dir.path().join("a.rs");
        do_edit(
            &ctx,
            &Utf8PathBuf::from_path_buf(target.clone()).unwrap(),
            "fn a() {}\n",
            "fn b() {}\n",
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fn b() {}\n");

        // undo：恢复 before。
        let outcome = undo(
            UndoArgs {
                action: None,
                scope: None,
            },
            &ctx,
        );
        assert_eq!(
            outcome.status,
            ToolStatus::Succeeded,
            "{}",
            outcome.model_payload.output
        );
        assert!(std::fs::read_to_string(&target).unwrap().contains("fn a()"));

        // redo：重新应用 after。
        let outcome = undo(
            UndoArgs {
                action: Some("redo".into()),
                scope: None,
            },
            &ctx,
        );
        assert_eq!(outcome.status, ToolStatus::Succeeded);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fn b() {}\n");
    }

    /// 空 journal：明确拒绝（empty_journal），不是失败。
    #[test]
    fn empty_journal_is_rejected_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = undo_ctx(&dir);
        let outcome = undo(
            UndoArgs {
                action: None,
                scope: None,
            },
            &ctx,
        );
        assert_eq!(outcome.status, ToolStatus::Rejected);
        assert!(outcome.model_payload.output.contains("empty_journal"));
    }

    /// 外部修改后 CAS 冲突：整体拒绝、不写任何文件（原子性）。
    #[test]
    fn conflict_blocks_write_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = undo_ctx(&dir);
        let target = dir.path().join("b.rs");
        do_edit(
            &ctx,
            &Utf8PathBuf::from_path_buf(target.clone()).unwrap(),
            "v1\n",
            "v2\n",
        );
        // 外部（bash 场景）改写：journal 快照失配。
        std::fs::write(&target, "external\n").unwrap();

        let outcome = undo(
            UndoArgs {
                action: None,
                scope: None,
            },
            &ctx,
        );
        assert_eq!(outcome.status, ToolStatus::Failed);
        assert!(
            outcome.model_payload.output.contains("Conflict"),
            "必须报告冲突: {}",
            outcome.model_payload.output
        );
        // 未写入任何文件：内容保持外部版本。
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "external\n");
    }

    /// 参数白名单：未知 action/scope 明确拒绝。
    #[test]
    fn invalid_args_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = undo_ctx(&dir);
        let bad = undo(
            UndoArgs {
                action: Some("rollback".into()),
                scope: None,
            },
            &ctx,
        );
        assert_eq!(bad.status, ToolStatus::Rejected);
        assert!(bad.model_payload.output.contains("invalid_arguments"));
    }
}
