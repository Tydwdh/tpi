//! First-stage workspace mutation boundary: snapshot → execute → delta → Journal.
use camino::Utf8PathBuf;
use std::collections::{BTreeMap, BTreeSet};
use tpi_session::protocol::{MutationCommittedPayload, MutationFile};
const MAX_FILES: usize = 20_000;
const MAX_BYTES: usize = 64 * 1024 * 1024;
pub struct TrackedWorkspace {
    root: Utf8PathBuf,
    before: BTreeMap<String, Vec<u8>>,
    bytes: usize,
}
impl TrackedWorkspace {
    pub fn capture(root: Utf8PathBuf) -> Result<Self, String> {
        let mut s = Self {
            root,
            before: BTreeMap::new(),
            bytes: 0,
        };
        s.scan()?;
        Ok(s)
    }
    fn scan(&mut self) -> Result<(), String> {
        let mut w = ignore::WalkBuilder::new(self.root.as_std_path());
        // Runtime/build products (target, node_modules, local artifacts) can
        // be orders of magnitude larger than the source workspace and are
        // already excluded by repository policy. Respect repository ignore
        // rules so tracking remains available for normal source changes;
        // otherwise merely having a Rust `target/` makes every bash call fail
        // its capture budget before execution.
        w.hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true);
        // `filter_entry` prunes before descent; the later component check is
        // retained as a defence for paths reached through other walkers.
        w.filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            name != ".git"
                && name != "target"
                && name != "node_modules"
                && !name.starts_with(".tpi-")
        });
        for item in w.build() {
            let e = item.map_err(|e| format!("workspace scan: {e}"))?;
            if !e.file_type().is_some_and(|t| t.is_file())
                || e.path().components().any(|c| {
                    let name = c.as_os_str().to_string_lossy();
                    name == ".git"
                        || name == "target"
                        || name == "node_modules"
                        || name.starts_with(".tpi-")
                })
            {
                continue;
            }
            let b = std::fs::read(e.path()).map_err(|e| format!("workspace read: {e}"))?;
            self.bytes += b.len();
            if self.before.len() >= MAX_FILES || self.bytes > MAX_BYTES {
                return Err("workspace_tracking_budget_exceeded".into());
            }
            let k = e
                .path()
                .strip_prefix(self.root.as_std_path())
                .map_err(|_| "workspace path outside root".to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            self.before.insert(k, b);
        }
        Ok(())
    }
    /// Persist the delta since the last checkpoint and make the current state the
    /// next checkpoint. This allows long-lived terminals to commit each observed
    /// command batch without re-recording earlier mutations.
    pub fn commit(&mut self, artifacts: &std::path::Path, session: &str) -> Result<usize, String> {
        let mut after = Self {
            root: self.root.clone(),
            before: BTreeMap::new(),
            bytes: 0,
        };
        after.scan()?;
        let keys: BTreeSet<_> = self
            .before
            .keys()
            .chain(after.before.keys())
            .cloned()
            .collect();
        let mut files = Vec::new();
        for k in keys {
            let before_exists = self.before.contains_key(&k);
            let after_exists = after.before.contains_key(&k);
            let b = self.before.get(&k).cloned().unwrap_or_default();
            let a = after.before.get(&k).cloned().unwrap_or_default();
            if a != b || before_exists != after_exists {
                files.push(MutationFile {
                    path: self.root.join(&k).to_string(),
                    before_revision: crate::tool::edit::revision_of(&b),
                    after_revision: crate::tool::edit::revision_of(&a),
                    before_exists,
                    after_exists,
                    before_content: b,
                    after_content: a,
                });
            }
        }
        if files.is_empty() {
            self.before = after.before;
            self.bytes = after.bytes;
            return Ok(0);
        }
        let payload = MutationCommittedPayload {
            mutation_id: uuid::Uuid::now_v7().to_string(),
            files,
        };
        let count = payload.files.len();
        tpi_session::journal::append_mutation(artifacts, session, &payload)
            .map_err(|e| format!("workspace journal: {e}"))?;
        self.before = after.before;
        self.bytes = after.bytes;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::TrackedWorkspace;

    #[test]
    fn capture_respects_gitignore_without_losing_source_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target").join("huge.bin"), vec![0u8; 1024]).unwrap();
        std::fs::write(dir.path().join("source.rs"), "fn main() {}\n").unwrap();

        let snapshot = TrackedWorkspace::capture(
            camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap(),
        )
        .unwrap();
        assert!(snapshot.before.contains_key("source.rs"));
        assert!(!snapshot.before.contains_key("target/huge.bin"));
    }
}
