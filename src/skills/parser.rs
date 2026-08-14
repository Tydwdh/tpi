//! SKILL.md 解析（README2 §16-§18/§25）。
//!
//! 标准格式（Anthropic Agent Skills）：
//! ```markdown
//! ---
//! name: skill-name
//! description: ...
//! ---
//! <body：使用说明/工作流/知识>
//! ```

use std::path::{Path, PathBuf};

/// 一个 Skill 的元数据（启动时只加载这些，README2 §18 Level 1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
}

/// 完整 Skill（激活时加载，README2 §18 Level 2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// SKILL.md 所在目录（references/scripts/assets 相对它）。
    pub dir: PathBuf,
    /// SKILL.md body（frontmatter 之后的内容）。
    pub body: String,
    /// references/* 文件列表（按需读取，README2 §18 Level 3）。
    pub references: Vec<String>,
    /// scripts/* 文件列表。
    pub scripts: Vec<String>,
}

impl Skill {
    pub fn meta(&self) -> SkillMeta {
        SkillMeta {
            name: self.name.clone(),
            description: self.description.clone(),
        }
    }
}

/// 解析失败（frontmatter 缺失/格式错误）。
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("SKILL.md 缺 YAML frontmatter（--- 包裹）: {path}")]
    MissingFrontmatter { path: PathBuf },
    #[error("SKILL.md frontmatter 缺 name 字段: {path}")]
    MissingName { path: PathBuf },
    #[error("SKILL.md frontmatter 缺 description 字段: {path}")]
    MissingDescription { path: PathBuf },
    #[error("SKILL.md 读取失败: {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// 解析 SKILL.md 文本（返回 meta）。
pub fn parse_meta(content: &str, path: &Path) -> Result<SkillMeta, ParseError> {
    let Some((frontmatter, _body)) = split_frontmatter(content) else {
        return Err(ParseError::MissingFrontmatter {
            path: path.to_path_buf(),
        });
    };
    let name = frontmatter_field(frontmatter, "name").ok_or_else(|| ParseError::MissingName {
        path: path.to_path_buf(),
    })?;
    let description = frontmatter_field(frontmatter, "description").ok_or_else(|| {
        ParseError::MissingDescription {
            path: path.to_path_buf(),
        }
    })?;
    Ok(SkillMeta { name, description })
}

/// 解析完整 SKILL.md（激活时）。
pub fn parse_full(content: &str, dir: PathBuf) -> Result<Skill, ParseError> {
    let path = dir.join("SKILL.md");
    let Some((frontmatter, body)) = split_frontmatter(content) else {
        return Err(ParseError::MissingFrontmatter { path });
    };
    let name = frontmatter_field(frontmatter, "name")
        .ok_or_else(|| ParseError::MissingName { path: path.clone() })?;
    let description = frontmatter_field(frontmatter, "description")
        .ok_or(ParseError::MissingDescription { path })?;
    let references = list_subdir(&dir, "references");
    let scripts = list_subdir(&dir, "scripts");
    Ok(Skill {
        name,
        description,
        dir,
        body: body.trim().to_string(),
        references,
        scripts,
    })
}

/// 拆 frontmatter：`---\n...\n---\nbody`。
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    let body = &rest[end + 4..];
    Some((frontmatter, body))
}

/// frontmatter 字段（`key: value`，value 取第一个冒号后 trim）。
fn frontmatter_field(frontmatter: &str, key: &str) -> Option<String> {
    frontmatter.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        if k.trim() == key {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

fn list_subdir(dir: &Path, sub: &str) -> Vec<String> {
    let subdir = dir.join(sub);
    let Ok(rd) = std::fs::read_dir(&subdir) else {
        return Vec::new();
    };
    let mut files: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|f| f != "SKILL.md")
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str =
        "---\nname: hello-skill\ndescription: 问候示例\n---\n# Hello\n\n使用步骤：\n1. 打招呼\n";

    #[test]
    fn parses_meta_from_frontmatter() {
        let meta = parse_meta(SAMPLE, &PathBuf::from("SKILL.md")).unwrap();
        assert_eq!(meta.name, "hello-skill");
        assert_eq!(meta.description, "问候示例");
    }

    #[test]
    fn parses_full_body_and_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("references")).unwrap();
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        std::fs::write(dir.path().join("references/foo.md"), "ref").unwrap();
        std::fs::write(dir.path().join("scripts/run.sh"), "#!/bin/sh").unwrap();
        std::fs::write(dir.path().join("SKILL.md"), SAMPLE).unwrap();
        let content = std::fs::read_to_string(dir.path().join("SKILL.md")).unwrap();
        let skill = parse_full(&content, dir.path().to_path_buf()).unwrap();
        assert_eq!(skill.body, "# Hello\n\n使用步骤：\n1. 打招呼");
        assert_eq!(skill.references, vec!["foo.md"]);
        assert_eq!(skill.scripts, vec!["run.sh"]);
    }

    #[test]
    fn missing_frontmatter_is_error() {
        assert!(parse_meta("no frontmatter", &PathBuf::from("x.md")).is_err());
        assert!(
            parse_meta(
                "---\ndescription: no name\n---\nbody",
                &PathBuf::from("x.md")
            )
            .is_err()
        );
    }
}
