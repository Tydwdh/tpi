//! SkillManager（README2 §16-§21）。
//!
//! - metadata-only 启动（Level 1：只加载 name/description）；
//! - activate_skill：激活后读取完整 SKILL.md（Level 2）注入 context；
//! - references/scripts 按需读取（Level 3）；
//! - Skill 不是 Tool（README2 §21）；Skill 是 Instructions/Workflow/Knowledge。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use camino::Utf8PathBuf;

use super::discovery::{DiscoveredSkill, discover};
use super::parser::{Skill, SkillMeta, parse_full};

/// 进程级单例（skills 是全局能力目录，跨会话共享）。
static MANAGER: OnceLock<Arc<Mutex<SkillManager>>> = OnceLock::new();

fn global() -> Arc<Mutex<SkillManager>> {
    MANAGER
        .get_or_init(|| Arc::new(Mutex::new(SkillManager::new())))
        .clone()
}

/// Skill 运行时状态（README2 §17）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillState {
    Discovered,
    Activated,
}

/// SkillManager：发现 + 激活。
pub struct SkillManager {
    /// name → 已发现 skill（含来源）。
    catalog: HashMap<String, DiscoveredSkill>,
    /// name → 激活状态。
    activated: HashMap<String, SkillState>,
    /// workspace root（发现项目 skills 用）。
    workspace_root: Option<Utf8PathBuf>,
}

impl SkillManager {
    pub fn new() -> Self {
        Self {
            catalog: HashMap::new(),
            activated: HashMap::new(),
            workspace_root: None,
        }
    }

    /// 全局单例（activate_skill 工具用）。
    pub fn global() -> Arc<Mutex<SkillManager>> {
        global()
    }

    /// 发现 skills（metadata-only；不读 body，README2 §18 Level 1）。
    pub fn refresh(&mut self, workspace_root: &Utf8PathBuf) {
        self.catalog.clear();
        self.workspace_root = Some(workspace_root.clone());
        for skill in discover(workspace_root) {
            let name = skill.meta.name.clone();
            self.catalog.insert(name, skill);
        }
        // 激活状态保留（skill 重发现不重置已激活的）。
    }

    /// 全部已发现 skill 的元数据（模型可见：Available skills 列表）。
    pub fn available(&self) -> Vec<SkillMeta> {
        let mut metas: Vec<SkillMeta> = self.catalog.values().map(|s| s.meta.clone()).collect();
        metas.sort_by(|a, b| a.name.cmp(&b.name));
        metas
    }

    pub fn get_meta(&self, name: &str) -> Option<SkillMeta> {
        self.catalog.get(name).map(|s| s.meta.clone())
    }

    /// 激活 skill：读取完整 SKILL.md（Level 2）。
    pub fn activate(&mut self, name: &str) -> Result<Skill, String> {
        let Some(discovered) = self.catalog.get(name).cloned() else {
            return Err(format!(
                "未知 skill: {name}（可用：{}）",
                self.available_names().join(", ")
            ));
        };
        let dir = discovered.dir.clone();
        let content = std::fs::read_to_string(dir.join("SKILL.md"))
            .map_err(|e| format!("读取 {} 失败: {e}", dir.display()))?;
        let skill = parse_full(&content, dir.clone())
            .map_err(|e| format!("解析 {} 失败: {e}", dir.display()))?;
        self.activated
            .insert(name.to_string(), SkillState::Activated);
        Ok(skill)
    }

    pub fn is_activated(&self, name: &str) -> bool {
        self.activated.get(name) == Some(&SkillState::Activated)
    }

    /// 已激活 skill 列表（Phase 5 context 注入用）。
    pub fn activated(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .activated
            .iter()
            .filter(|(_, state)| **state == SkillState::Activated)
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    pub fn available_names(&self) -> Vec<String> {
        self.available().into_iter().map(|m| m.name).collect()
    }

    /// 读取 skill 的 reference 文件（Level 3，按需）。
    /// ISSUE-036：`reference` 必须限定在 `references/` 目录内——拒绝路径分隔
    /// 与 `..`（否则 `../../config` 可读取 skill 目录外的任意文件）。
    pub fn read_reference(&self, name: &str, reference: &str) -> Result<String, String> {
        let Some(skill) = self.catalog.get(name) else {
            return Err(format!("未知 skill: {name}"));
        };
        let trimmed = reference.trim();
        if trimmed.is_empty()
            || trimmed.contains('/')
            || trimmed.contains('\\')
            || trimmed == ".."
            || trimmed.starts_with("..")
            || trimmed.contains('\0')
        {
            return Err(format!(
                "reference 必须是 references/ 目录内的文件名: {reference:?}"
            ));
        }
        let path: PathBuf = skill.dir.join("references").join(trimmed);
        std::fs::read_to_string(&path).map_err(|e| format!("读取 reference {reference} 失败: {e}"))
    }
}

impl Default for SkillManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 便捷：刷新全局 manager（app 启动时调用一次）。
pub fn refresh_global(workspace_root: &Utf8PathBuf) {
    let binding = global();
    let mut manager = tpi_core::util::lock_mutex(&binding, "skill_manager");
    manager.refresh(workspace_root);
}
