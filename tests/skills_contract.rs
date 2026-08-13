//! Skills V1 集成测试（README2 §25：Discovery / Activation / Reference / Context）。
//!
//! 测试 skill：hello-skill / rust-review / bevy-debug，放 workspace 项目级
//! `.agent/skills/` 下。

mod fixtures;

use camino::Utf8PathBuf;

fn write_skill(root: &std::path::Path, name: &str, description: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(dir.join("references")).unwrap();
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
    )
    .unwrap();
}

fn setup_workspace_with_skills() -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let skills_root = dir.path().join(".agent/skills");
    std::fs::create_dir_all(&skills_root).unwrap();
    write_skill(
        &skills_root,
        "hello-skill",
        "问候示例：演示 skill 激活流程",
        "# Hello Skill\n\n激活后请输出问候语。",
    );
    write_skill(
        &skills_root,
        "rust-review",
        "Rust 代码审查：关注抽象边界、错误处理与可维护性",
        "# Rust Review\n\n## 步骤\n1. 读取目标代码\n2. 检查错误处理\n\n## 参考\n见 references/error-handling.md",
    );
    write_skill(
        &skills_root,
        "bevy-debug",
        "Bevy 游戏调试：查询实体状态、模拟输入、诊断移动问题",
        "# Bevy Debug\n\n1. 构建并启动游戏\n2. 查询 Player Transform\n3. 模拟 W 输入\n4. 对比状态",
    );
    // references + scripts 文件。
    std::fs::write(
        dir.path().join(".agent/skills/rust-review/references/error-handling.md"),
        "# 错误处理要点\n\n- 优先 thiserror\n- 不 panic\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".agent/skills/bevy-debug/scripts/query.py"),
        "print('query player')",
    )
    .unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    (dir, workspace)
}

/// §25 Discovery：metadata-only 启动（不读 body）。
#[tokio::test]
async fn discovery_is_metadata_only() {
    let (_dir, workspace) = setup_workspace_with_skills();
    let manager = tpi::skills::SkillManager::global();
    let mut manager = manager.lock().unwrap();
    manager.refresh(&workspace);
    let metas = manager.available();
    assert_eq!(metas.len(), 3);
    let names: Vec<&str> = metas.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, vec!["bevy-debug", "hello-skill", "rust-review"]);
    // metadata-only：未激活时无 body（activate 才读）。
    assert!(!manager.is_activated("hello-skill"));
}

/// §25 Activation：activate_skill 返回完整 SKILL.md + references/scripts。
#[tokio::test]
async fn activate_skill_returns_full_body_and_assets() {
    let (_dir, workspace) = setup_workspace_with_skills();
    let manager = tpi::skills::SkillManager::global();
    let mut manager = manager.lock().unwrap();
    manager.refresh(&workspace);

    let skill = manager.activate("rust-review").unwrap();
    assert!(skill.body.contains("Rust Review"));
    assert!(skill.body.contains("references/error-handling.md"));
    assert_eq!(skill.references, vec!["error-handling.md"]);
    assert!(manager.is_activated("rust-review"));

    // 未知 skill → 错误。
    assert!(manager.activate("no-such").is_err());
}

/// §25 Reference：按需读取 reference（Level 3）。
#[tokio::test]
async fn read_reference_on_demand() {
    let (_dir, workspace) = setup_workspace_with_skills();
    let manager = tpi::skills::SkillManager::global();
    let mut manager = manager.lock().unwrap();
    manager.refresh(&workspace);
    let content = manager.read_reference("rust-review", "error-handling.md").unwrap();
    assert!(content.contains("thiserror"));
    assert!(manager.read_reference("rust-review", "missing.md").is_err());
}

/// §25 Context：activate_skill 工具经 ToolContext 可调用（端到端工具级）。
#[tokio::test]
async fn activate_skill_tool_executes_via_tool() {
    let (_dir, workspace) = setup_workspace_with_skills();
    let manager = tpi::skills::SkillManager::global();
    manager.lock().unwrap().refresh(&workspace);

    let ctx = fixtures::test_tool_context(&workspace);
    let outcome = tpi::tool::execute(
        tpi::tool::BuiltinTool::ActivateSkill,
        tpi::tool::ValidatedArgs::ActivateSkill(tpi::skills::activate::ActivateSkillArgs {
            name: "bevy-debug".into(),
        }),
        &ctx,
        None,
    )
    .await;
    assert_eq!(outcome.status, tpi::tool::outcome::ToolStatus::Succeeded);
    let out = &outcome.model_payload.output;
    assert!(out.contains("Bevy Debug"), "{out}");
    assert!(out.contains("query.py"), "scripts 清单：{out}");
}

/// §25 Context：几十个大 skill 不随总文本量线性增长（metadata-only 原则）。
#[tokio::test]
async fn metadata_only_keeps_startup_cost_flat() {
    let dir = tempfile::tempdir().unwrap();
    let skills_root = dir.path().join(".agent/skills");
    std::fs::create_dir_all(&skills_root).unwrap();
    // 30 个 skill，每个 body 2KB。
    for i in 0..30 {
        let name = format!("skill-{i:02}");
        write_skill(
            &skills_root,
            &name,
            &format!("第 {i} 个测试 skill"),
            &"x".repeat(2048),
        );
    }
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let manager = tpi::skills::SkillManager::global();
    let mut manager = manager.lock().unwrap();
    manager.refresh(&workspace);
    let metas = manager.available();
    assert_eq!(metas.len(), 30);
    // metadata 总大小 = 30 * (name + description)，远小于 30 * 2KB body。
    let meta_bytes: usize = metas.iter().map(|m| m.name.len() + m.description.len()).sum();
    assert!(
        meta_bytes < 2048 * 30 / 4,
        "metadata 必须远小于全量 body（progressive disclosure）：{meta_bytes} bytes"
    );
}
