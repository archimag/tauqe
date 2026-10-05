use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use anyhow::{bail, Result};
pub use workbench_protocol::matches_glob_pattern;
use workbench_protocol::{ContextAccess, ContextItem, ContextState};

#[derive(Debug, Clone)]
pub struct ContextFileContent {
    pub path: String,
    pub access: ContextAccess,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ContextManager {
    repo_root: PathBuf,
    revision: u64,
    items: BTreeMap<String, ContextItem>,
}

impl ContextManager {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            revision: 1,
            items: BTreeMap::new(),
        }
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn set_repo_root(&mut self, repo_root: PathBuf) {
        self.repo_root = repo_root;
    }

    pub fn get_state(&self) -> ContextState {
        let items: Vec<ContextItem> = self.items.values().cloned().collect();
        let total_estimated_tokens = items.iter().map(|it| it.estimated_tokens).sum();

        ContextState {
            revision: self.revision,
            items,
            total_estimated_tokens,
        }
    }

    pub fn read_context_files(&self) -> Vec<ContextFileContent> {
        let mut files = Vec::new();
        for (path, item) in &self.items {
            let full_path = self.repo_root.join(path);
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                files.push(ContextFileContent {
                    path: path.clone(),
                    access: item.access,
                    content,
                });
            }
        }
        files
    }

    pub fn contains(&self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        Ok(self.items.contains_key(&clean_path))
    }

    pub fn is_editable(&self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        match self.items.get(&clean_path) {
            Some(item) => Ok(item.access == ContextAccess::Editable),
            None => Ok(false),
        }
    }

    pub fn add_file(&mut self, relative_path: &str, access: ContextAccess) -> Result<ContextItem> {
        let clean_path = self.normalize_path(relative_path)?;
        let full_path = self.repo_root.join(&clean_path);

        if !full_path.is_file() {
            bail!("File not found or is a directory: {}", clean_path);
        }

        let metadata = std::fs::metadata(&full_path)?;
        let size_bytes = metadata.len();
        let estimated_tokens = (size_bytes + 3) / 4;

        let item = ContextItem {
            path: clean_path.clone(),
            access,
            size_bytes,
            estimated_tokens,
        };

        self.items.insert(clean_path, item.clone());
        self.revision += 1;

        Ok(item)
    }

    /// Adds all repository files matching a glob pattern or directory prefix.
    ///
    /// Skips files already present in context.
    /// Returns (list of newly added items, total added estimated tokens).
    pub fn add_files_by_pattern(
        &mut self,
        pattern: &str,
        access: ContextAccess,
        available_files: &[String],
    ) -> Result<(Vec<ContextItem>, u64)> {
        let trimmed_pat = pattern.trim();
        if trimmed_pat.is_empty() {
            bail!("Empty pattern is not allowed");
        }

        let mut matched_files: Vec<String> = available_files
            .iter()
            .filter(|path| matches_glob_pattern(trimmed_pat, path))
            .filter(|path| !self.items.contains_key(*path))
            .cloned()
            .collect();

        // Sort for deterministic addition order
        matched_files.sort();

        // Safety cap: max 150 files per batch
        const MAX_BATCH_LIMIT: usize = 150;
        if matched_files.len() > MAX_BATCH_LIMIT {
            matched_files.truncate(MAX_BATCH_LIMIT);
        }

        let mut added_items = Vec::new();
        let mut total_added_tokens = 0u64;

        for path in matched_files {
            if let Ok(item) = self.add_file(&path, access) {
                total_added_tokens += item.estimated_tokens;
                added_items.push(item);
            }
        }

        Ok((added_items, total_added_tokens))
    }

    pub fn update_file_metadata(&mut self, relative_path: &str) -> Result<()> {
        let clean_path = self.normalize_path(relative_path)?;
        let full_path = self.repo_root.join(&clean_path);
        if let Some(item) = self.items.get_mut(&clean_path) {
            if let Ok(metadata) = std::fs::metadata(&full_path) {
                item.size_bytes = metadata.len();
                item.estimated_tokens = (item.size_bytes + 3) / 4;
                self.revision += 1;
            }
        }
        Ok(())
    }

    pub fn remove_file(&mut self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        if self.items.remove(&clean_path).is_some() {
            self.revision += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn set_access(&mut self, relative_path: &str, access: ContextAccess) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        if let Some(item) = self.items.get_mut(&clean_path) {
            if item.access != access {
                item.access = access;
                self.revision += 1;
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn clear(&mut self) {
        if !self.items.is_empty() {
            self.items.clear();
            self.revision += 1;
        }
    }

    fn normalize_path(&self, p: &str) -> Result<String> {
        let path = Path::new(p);
        let mut components = Vec::new();

        for component in path.components() {
            match component {
                std::path::Component::Normal(c) => components.push(c.to_string_lossy().to_string()),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    if components.pop().is_none() {
                        bail!("Path escapes repository root: {}", p);
                    }
                }
                std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                    bail!("Absolute paths are not allowed: {}", p);
                }
            }
        }

        if components.is_empty() {
            bail!("Empty path is not allowed");
        }

        Ok(components.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_normalize_path() {
        let cm = ContextManager::new(PathBuf::from("/tmp"));
        assert_eq!(
            cm.normalize_path("crates/core/src/lib.rs").unwrap(),
            "crates/core/src/lib.rs"
        );
        assert_eq!(
            cm.normalize_path("./crates/core/../core/src/lib.rs").unwrap(),
            "crates/core/src/lib.rs"
        );
        assert!(cm.normalize_path("../outside.rs").is_err());
        assert!(cm.normalize_path("/abs/path.rs").is_err());
    }

    #[test]
    fn test_add_files_by_pattern() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn test() {}").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("docs/Readme.md"), "# Docs").unwrap();

        let repo_files = vec![
            "src/lib.rs".to_string(),
            "src/main.rs".to_string(),
            "docs/Readme.md".to_string(),
        ];

        let mut cm = ContextManager::new(root);
        let (added, tokens) = cm
            .add_files_by_pattern("src/*.rs", ContextAccess::ReadOnly, &repo_files)
            .unwrap();

        assert_eq!(added.len(), 2);
        assert!(tokens > 0);
        assert!(cm.contains("src/lib.rs").unwrap());
        assert!(cm.contains("src/main.rs").unwrap());
        assert!(!cm.contains("docs/Readme.md").unwrap());
    }
}
