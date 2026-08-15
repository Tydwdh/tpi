//! activate_skill 内置工具（README2 §20）。
//!
//! 模型看到 Available skills（metadata）后，调用 `activate_skill(name)` 获取
//! 完整 SKILL.md（Level 2）；Skill 不是 Tool，激活返回的是 Instructions。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::ToolContext;
use tpi_core::outcome::{ModelPayload, ToolOutcome, ToolStatus};

/// activate_skill 参数。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct ActivateSkillArgs {
    /// Skill 名称（来自 Available skills 列表）。
    pub name: String,
}

/// 执行 activate_skill：读取并返回完整 SKILL.md（+ references/scripts 清单）。
pub fn activate_skill(args: ActivateSkillArgs, ctx: &ToolContext) -> ToolOutcome {
    // 确保 catalog 已发现（workspace root 来自 ctx）。
    let manager = crate::skills::SkillManager::global();
    {
        let mut manager = manager.lock().unwrap();
        // 每次激活前 refresh（项目 skills 可能新增）；轻量元数据扫描。
        manager.refresh(&ctx.workspace_root);
        match manager.activate(&args.name) {
            Ok(skill) => {
                let mut output = format!(
                    "status: succeeded\ntool: activate_skill\nskill: {}\n\n{}",
                    skill.name, skill.body
                );
                if !skill.references.is_empty() {
                    output.push_str(&format!(
                        "\n\nreferences（{}，可按需读取 skills/<name>/references/ 下文件）:\n  {}",
                        skill.references.len(),
                        skill.references.join("\n  ")
                    ));
                }
                if !skill.scripts.is_empty() {
                    output.push_str(&format!("\n\nscripts:\n  {}", skill.scripts.join("\n  ")));
                }
                let mut outcome = ToolOutcome::succeeded("activate_skill", output);
                outcome.session_metadata = tpi_core::outcome::ToolMetadata {
                    tool: "activate_skill".into(),
                    target: Some(skill.name.clone()),
                    ..Default::default()
                };
                outcome
            }
            Err(error) => ToolOutcome::failed(
                "activate_skill",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: activate_skill\nerror: skill_not_found\n\n{error}"
                    ),
                    effect: None,
                    artifact: None,
                },
            ),
        }
    }
}
