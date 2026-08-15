//! Agent Skills（README2 §16-§25）。
//!
//! Skill 是 Instructions/Workflow/Knowledge（**不是 Tool**）；模型通过内置
//! `activate_skill` 工具选择并激活（Level 2 才读 SKILL.md 全文）。
//! Progressive Disclosure：启动只加载 metadata（Level 1）。

pub mod activate;
pub mod discovery;
pub mod manager;
pub mod parser;

pub use manager::SkillManager;
