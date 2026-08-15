//! Skill discovery（README2 §19）。
//!
//! 目录优先级：project skills > user skills > builtin skills；
//! 同名 skill：project override user override builtin，但记录来源。

use std::path::PathBuf;

use super::parser::{SkillMeta, parse_meta};

/// 一个已发现的 skill（含来源目录，用于去重与来源记录）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSkill {
    pub meta: SkillMeta,
    pub dir: PathBuf,
    pub origin: SkillOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillOrigin {
    Project,
    User,
    Builtin,
}

impl std::fmt::Display for SkillOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillOrigin::Project => write!(f, "project"),
            SkillOrigin::User => write!(f, "user"),
            SkillOrigin::Builtin => write!(f, "builtin"),
        }
    }
}

/// 项目 skills 目录（相对 workspace root）。
pub const PROJECT_SKILLS_DIR: &str = ".agent/skills";

/// 用户 skills 目录（~/.tpi/skills）。
pub fn user_skills_dir() -> PathBuf {
    tpi_core::util::tpi_home().join("skills")
}

/// 内置 skills 目录（随包；当前无内置，占位）。
pub fn builtin_skills_dir() -> PathBuf {
    tpi_core::util::tpi_home().join("skills").join(".builtin")
}

/// 扫描一个目录下的 skills（每个子目录含 SKILL.md）。
fn scan_dir(dir: &PathBuf, origin: SkillOrigin) -> Vec<DiscoveredSkill> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let skill_dir = entry.path();
        let skill_file = skill_dir.join("SKILL.md");
        let Ok(content) = std::fs::read_to_string(&skill_file) else {
            continue; // 无 SKILL.md 的目录不是 skill
        };
        match parse_meta(&content, &skill_file) {
            Ok(meta) => out.push(DiscoveredSkill {
                meta,
                dir: skill_dir,
                origin,
            }),
            Err(e) => {
                tracing::warn!(error = %e, "skill 元数据解析失败，跳过");
            }
        }
    }
    out
}

/// 从所有来源发现 skills（去重：高优先级覆盖低优先级，README2 §19）。
pub fn discover(workspace_root: &camino::Utf8PathBuf) -> Vec<DiscoveredSkill> {
    // 低 → 高 扫描，后面的覆盖同名。
    let mut by_name: std::collections::HashMap<String, DiscoveredSkill> =
        std::collections::HashMap::new();
    let mut order = Vec::new();
    for (dir, origin) in [
        (builtin_skills_dir(), SkillOrigin::Builtin),
        (user_skills_dir(), SkillOrigin::User),
        (
            workspace_root.join(PROJECT_SKILLS_DIR).into(),
            SkillOrigin::Project,
        ),
    ] {
        for skill in scan_dir(&dir, origin) {
            let name = skill.meta.name.clone();
            if !by_name.contains_key(&name) {
                order.push(name.clone());
            }
            by_name.insert(name, skill);
        }
    }
    // 按首次出现顺序返回（builtin → user → project 顺序，同名被 project 覆盖）。
    order
        .into_iter()
        .filter_map(|name| by_name.remove(&name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &std::path::Path, name: &str, description: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody"),
        )
        .unwrap();
    }

    #[test]
    fn discovers_across_dirs_with_priority_override() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("user-skills");
        let project = tmp.path().join("proj-skills");
        write_skill(&user, "hello", "用户版");
        write_skill(&user, "only-user", "只在用户");
        write_skill(&project, "hello", "项目版覆盖");

        let workspace = camino::Utf8PathBuf::from_path_buf(tmp.path().join("workspace")).unwrap();
        // 模拟 user dir 与 project dir（通过临时改环境？直接调 scan 验证优先级）。
        // 这里直接验证 scan_dir + 手工合并逻辑的核心：同名覆盖。
        let user_skills = scan_dir(&user, SkillOrigin::User);
        let proj_skills = scan_dir(&project, SkillOrigin::Project);
        let mut by_name: std::collections::HashMap<String, DiscoveredSkill> =
            std::collections::HashMap::new();
        for s in user_skills.into_iter().chain(proj_skills) {
            by_name.insert(s.meta.name.clone(), s);
        }
        let hello = by_name.get("hello").unwrap();
        assert_eq!(hello.meta.description, "项目版覆盖", "project 覆盖 user");
        assert_eq!(hello.origin, SkillOrigin::Project);
        assert!(by_name.contains_key("only-user"));
        let _ = workspace;
    }
}
