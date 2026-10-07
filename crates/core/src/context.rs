use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::collections::BTreeSet;
pub use tauqe_protocol::matches_glob_pattern;
use tauqe_protocol::{ContextAccess, ContextItem, ContextLayer, ContextState};

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
    pinned: BTreeMap<String, ContextItem>,
    user: BTreeMap<String, ContextItem>,
    auto: BTreeMap<String, ContextItem>,
}

impl ContextManager {
    pub fn new(repo_root: PathBuf) -> Self {
        let mut cm = Self {
            repo_root,
            revision: 1,
            pinned: BTreeMap::new(),
            user: BTreeMap::new(),
            auto: BTreeMap::new(),
        };
        cm.load_pinned_from_repo();
        cm
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn set_repo_root(&mut self, repo_root: PathBuf) {
        self.repo_root = repo_root;
        self.load_pinned_from_repo();
    }

    pub fn load_pinned_from_repo(&mut self) {
        let config = crate::config::load_config(Some(&self.repo_root));
        self.load_pinned(&config.context.pinned);
    }

    pub fn load_pinned(&mut self, pinned_paths: &[String]) {
        self.pinned.clear();
        for path in pinned_paths {
            if let Ok(clean_path) = self.normalize_path(path) {
                let full_path = self.repo_root.join(&clean_path);
                if full_path.is_file() {
                    if let Ok(meta) = std::fs::metadata(&full_path) {
                        let size = meta.len();
                        let tokens = size.div_ceil(4);
                        self.pinned.insert(
                            clean_path.clone(),
                            ContextItem {
                                path: clean_path,
                                access: ContextAccess::ReadOnly,
                                size_bytes: size,
                                estimated_tokens: tokens,
                                layer: ContextLayer::Pinned,
                            },
                        );
                    }
                }
            }
        }
    }

    /// Resolves access across all layers.
    /// Invariant: `Editable` always takes absolute priority over `ReadOnly`.
    pub fn effective_access_for(&self, clean_path: &str) -> Option<ContextAccess> {
        let mut found = false;
        for layer in [&self.user, &self.auto, &self.pinned] {
            if let Some(it) = layer.get(clean_path) {
                if it.access == ContextAccess::Editable {
                    return Some(ContextAccess::Editable);
                }
                found = true;
            }
        }
        if found {
            Some(ContextAccess::ReadOnly)
        } else {
            None
        }
    }

    pub fn get_state(&self) -> ContextState {
        let mut items = Vec::new();
        for it in self.pinned.values() {
            let mut it = it.clone();
            if let Some(eff) = self.effective_access_for(&it.path) {
                it.access = eff;
            }
            items.push(it);
        }
        for it in self.user.values() {
            let mut it = it.clone();
            if let Some(eff) = self.effective_access_for(&it.path) {
                it.access = eff;
            }
            items.push(it);
        }
        for it in self.auto.values() {
            let mut it = it.clone();
            if let Some(eff) = self.effective_access_for(&it.path) {
                it.access = eff;
            }
            items.push(it);
        }

        let mut seen = std::collections::HashSet::new();
        let mut total_estimated_tokens = 0u64;
        for it in &items {
            if seen.insert(&it.path) {
                total_estimated_tokens += it.estimated_tokens;
            }
        }

        ContextState {
            revision: self.revision,
            items,
            total_estimated_tokens,
        }
    }

    pub fn read_context_files(&self) -> Vec<ContextFileContent> {
        let mut distinct_paths = BTreeSet::new();
        distinct_paths.extend(self.pinned.keys().cloned());
        distinct_paths.extend(self.user.keys().cloned());
        distinct_paths.extend(self.auto.keys().cloned());

        let mut files = Vec::new();
        for path in distinct_paths {
            let full_path = self.repo_root.join(&path);
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                let access = self.effective_access_for(&path).unwrap_or(ContextAccess::ReadOnly);
                files.push(ContextFileContent {
                    path,
                    access,
                    content,
                });
            }
        }
        files
    }

    pub fn contains(&self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        Ok(self.pinned.contains_key(&clean_path)
            || self.user.contains_key(&clean_path)
            || self.auto.contains_key(&clean_path))
    }

    pub fn is_editable(&self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        Ok(self.effective_access_for(&clean_path) == Some(ContextAccess::Editable))
    }

    pub fn add_file(&mut self, relative_path: &str, access: ContextAccess) -> Result<ContextItem> {
        self.add_file_to_layer(relative_path, access, ContextLayer::User)
    }

    pub fn add_file_to_layer(
        &mut self,
        relative_path: &str,
        access: ContextAccess,
        layer: ContextLayer,
    ) -> Result<ContextItem> {
        let clean_path = self.normalize_path(relative_path)?;
        let full_path = self.repo_root.join(&clean_path);

        if !full_path.is_file() {
            bail!("File not found or is a directory: {}", clean_path);
        }

        let metadata = std::fs::metadata(&full_path)?;
        let size_bytes = metadata.len();
        let estimated_tokens = size_bytes.div_ceil(4);

        let item = ContextItem {
            path: clean_path.clone(),
            access,
            size_bytes,
            estimated_tokens,
            layer,
        };

        match layer {
            ContextLayer::Pinned => {
                self.pinned.insert(clean_path, item.clone());
            }
            ContextLayer::User => {
                // If previously in auto, user adding it promotes it out of auto
                self.auto.remove(&clean_path);
                self.user.insert(clean_path, item.clone());
            }
            ContextLayer::Auto => {
                // If already in user, do not demote to auto
                if !self.user.contains_key(&clean_path) {
                    self.auto.insert(clean_path, item.clone());
                }
            }
        }

        self.revision += 1;
        Ok(item)
    }

    pub fn add_auto_file(
        &mut self,
        relative_path: &str,
        access: ContextAccess,
    ) -> Result<ContextItem> {
        self.add_file_to_layer(relative_path, access, ContextLayer::Auto)
    }

    pub fn promote_auto_to_user(&mut self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        if let Some(mut item) = self.auto.remove(&clean_path) {
            item.layer = ContextLayer::User;
            self.user.insert(clean_path, item);
            self.revision += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn clear_auto(&mut self) {
        if !self.auto.is_empty() {
            self.auto.clear();
            self.revision += 1;
        }
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
            .filter(|path| !self.user.contains_key(*path))
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
        let mut updated = false;
        if let Ok(metadata) = std::fs::metadata(&full_path) {
            let size = metadata.len();
            let tokens = size.div_ceil(4);
            for layer in [&mut self.pinned, &mut self.user, &mut self.auto] {
                if let Some(item) = layer.get_mut(&clean_path) {
                    item.size_bytes = size;
                    item.estimated_tokens = tokens;
                    updated = true;
                }
            }
        }
        if updated {
            self.revision += 1;
        }
        Ok(())
    }

    pub fn remove_file(&mut self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        let removed_user = self.user.remove(&clean_path).is_some();
        let removed_auto = self.auto.remove(&clean_path).is_some();
        if removed_user || removed_auto {
            self.revision += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Renames a file in context, preserving its layer, access level, and metadata.
    pub fn rename_file(&mut self, from_relative: &str, to_relative: &str) -> Result<()> {
        let clean_from = self.normalize_path(from_relative)?;
        let clean_to = self.normalize_path(to_relative)?;
        if clean_from == clean_to {
            bail!("Source and destination paths are identical: {}", clean_from);
        }

        let mut found_item = None;
        let mut found_layer = None;

        if let Some(item) = self.user.remove(&clean_from) {
            found_item = Some(item);
            found_layer = Some(ContextLayer::User);
        } else if let Some(item) = self.auto.remove(&clean_from) {
            found_item = Some(item);
            found_layer = Some(ContextLayer::Auto);
        } else if self.pinned.contains_key(&clean_from) {
            bail!("Pinned file '{}' cannot be renamed or moved", clean_from);
        }

        if let (Some(mut item), Some(layer)) = (found_item, found_layer) {
            item.path = clean_to.clone();
            let full_to = self.repo_root.join(&clean_to);
            if let Ok(meta) = std::fs::metadata(&full_to) {
                item.size_bytes = meta.len();
                item.estimated_tokens = item.size_bytes.div_ceil(4);
            }
            match layer {
                ContextLayer::User => {
                    self.user.insert(clean_to, item);
                }
                ContextLayer::Auto => {
                    self.auto.insert(clean_to, item);
                }
                ContextLayer::Pinned => {}
            }
            self.revision += 1;
        }

        Ok(())
    }

    pub fn set_access(&mut self, relative_path: &str, access: ContextAccess) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        let mut changed = false;
        if let Some(item) = self.user.get_mut(&clean_path) {
            if item.access != access {
                item.access = access;
                changed = true;
            }
        } else if let Some(item) = self.auto.get_mut(&clean_path) {
            if item.access != access {
                item.access = access;
                changed = true;
            }
        }
        // Pinned files are protected: read-only access cannot be changed via set_access
        if changed {
            self.revision += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn clear(&mut self) {
        let had_items = !self.user.is_empty() || !self.auto.is_empty();
        if had_items {
            self.user.clear();
            self.auto.clear();
            self.revision += 1;
        }
    }

    /// Prunes files from context that no longer exist on disk (e.g. after undoing AI creation).
    pub fn prune_missing_files(&mut self) -> Vec<String> {
        let mut removed = Vec::new();
        let prune_layer = |layer: &mut BTreeMap<String, ContextItem>, root: &Path| -> Vec<String> {
            let to_remove: Vec<String> = layer
                .keys()
                .filter(|p| !root.join(p).is_file())
                .cloned()
                .collect();
            for p in &to_remove {
                layer.remove(p);
            }
            to_remove
        };

        removed.extend(prune_layer(&mut self.pinned, &self.repo_root));
        removed.extend(prune_layer(&mut self.user, &self.repo_root));
        removed.extend(prune_layer(&mut self.auto, &self.repo_root));

        if !removed.is_empty() {
            self.revision += 1;
        }

        removed
    }

    /// Merges updates produced by a workflow run into the current context manager.
    /// Preserves user layer changes made concurrently. Updates the auto layer,
    /// syncs file renames/removals, and refreshes metadata.
    pub fn merge_turn_context(&mut self, turn_context: &ContextManager) {
        // 1. Sync auto layer: replace with turn_context.auto (excluding any path already in user layer)
        self.auto.clear();
        for (path, item) in &turn_context.auto {
            if !self.user.contains_key(path) {
                self.auto.insert(path.clone(), item.clone());
            }
        }

        // 2. Remove files that no longer exist on disk (prunes deleted files)
        self.prune_missing_files();

        // 3. Mirror any file renamed/moved into user layer by the turn
        for (path, item) in &turn_context.user {
            if !self.user.contains_key(path) && self.repo_root.join(path).is_file() {
                self.user.insert(path.clone(), item.clone());
            }
        }

        // 4. Refresh metadata for all files in context
        for layer in [&mut self.pinned, &mut self.user, &mut self.auto] {
            for (path, item) in layer.iter_mut() {
                let full_path = self.repo_root.join(path);
                if let Ok(meta) = std::fs::metadata(&full_path) {
                    let size = meta.len();
                    item.size_bytes = size;
                    item.estimated_tokens = size.div_ceil(4);
                }
            }
        }

        self.revision += 1;
    }

    pub fn normalize_path(&self, p: &str) -> Result<String> {
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
            cm.normalize_path("./crates/core/../core/src/lib.rs")
                .unwrap(),
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

    #[test]
    fn test_three_tier_layers_and_permission_merging() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("pinned.rs"), "pub fn pinned() {}").unwrap();
        std::fs::write(root.join("auto.rs"), "pub fn auto() {}").unwrap();

        let mut cm = ContextManager::new(root.clone());
        cm.load_pinned(&["pinned.rs".to_string()]);
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly).unwrap();

        // Pinned is read-only
        assert!(!cm.is_editable("pinned.rs").unwrap());
        // Protected from removal
        assert!(!cm.remove_file("pinned.rs").unwrap());
        assert!(cm.contains("pinned.rs").unwrap());

        // When developer adds the pinned file as Editable to user layer, Editable takes absolute priority
        cm.add_file("pinned.rs", ContextAccess::Editable).unwrap();
        assert!(cm.is_editable("pinned.rs").unwrap());

        // Promoting auto file to user
        assert!(cm.promote_auto_to_user("auto.rs").unwrap());
        let state = cm.get_state();
        let auto_item = state.items.iter().find(|i| i.path == "auto.rs").unwrap();
        assert_eq!(auto_item.layer, ContextLayer::User);
    }

    #[test]
    fn test_merge_turn_context_preserves_concurrent_user_additions() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("base.rs"), "fn base() {}").unwrap();
        std::fs::write(root.join("auto.rs"), "fn auto() {}").unwrap();
        std::fs::write(root.join("user_added.rs"), "fn user_added() {}").unwrap();

        let mut primary = ContextManager::new(root.clone());
        primary.add_file("base.rs", ContextAccess::Editable).unwrap();

        // Turn starts with a snapshot
        let mut turn_snapshot = primary.clone();
        turn_snapshot.add_auto_file("auto.rs", ContextAccess::ReadOnly).unwrap();

        // While turn is running, user adds a new file to primary
        primary.add_file("user_added.rs", ContextAccess::Editable).unwrap();

        // Turn completes and merges back
        primary.merge_turn_context(&turn_snapshot);

        // Verify user_added is still present
        assert!(primary.contains("user_added.rs").unwrap());
        assert!(primary.is_editable("user_added.rs").unwrap());
        // Verify auto file from turn was merged
        assert!(primary.contains("auto.rs").unwrap());
        assert!(!primary.is_editable("auto.rs").unwrap());
    }

    #[test]
    fn test_prune_missing_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let file_path = root.join("test.rs");
        std::fs::write(&file_path, "fn hello() {}").unwrap();

        let mut cm = ContextManager::new(root);
        cm.add_file("test.rs", ContextAccess::Editable).unwrap();
        assert!(cm.contains("test.rs").unwrap());

        // Delete file from disk
        std::fs::remove_file(&file_path).unwrap();

        let pruned = cm.prune_missing_files();
        assert_eq!(pruned, vec!["test.rs".to_string()]);
        assert!(!cm.contains("test.rs").unwrap());
    }
}
