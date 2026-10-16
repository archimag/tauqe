use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::prompt::estimate_tokens_from_bytes;
use tauqe_protocol::{ContextAccess, ContextItem, ContextLayer};

use super::{verify_path_within_repo, ContextManager};

impl ContextManager {
    pub fn load_pinned_from_repo(&mut self) {
        let config = crate::config::load_context_config(&self.repo_root);
        self.load_pinned(&config.pinned);
    }

    pub fn load_pinned(&mut self, pinned_paths: &[String]) {
        self.pinned_patterns = pinned_paths.to_vec();
        self.recompute_pinned();
    }

    /// Recomputes the pinned context layer with hierarchical resolution.
    ///
    /// Root-level pinned files matching `pinned_patterns` from `repo_root` are always
    /// included. Additionally, for every file present in the `user` or `auto` context
    /// layers, all parent directories up to the project root are traversed: at each level,
    /// every pinned pattern is resolved as a relative path and any existing file is added
    /// to the pinned layer (read-only, deduplicated, never escaping repo root).
    pub(super) fn recompute_pinned(&mut self) -> bool {
        let mut new_pinned = BTreeMap::new();

        // 1. Root-level pinned files
        for pattern in &self.pinned_patterns {
            if let Ok(clean_path) = self.normalize_path(pattern) {
                self.insert_pinned_candidate(&mut new_pinned, clean_path);
            }
        }

        // 2. Hierarchical pinned files for all items in user and auto layers
        let mut checked_dirs = BTreeSet::new();
        for file_path in self.user.keys().chain(self.auto.keys()) {
            let mut curr = Path::new(file_path).parent();
            while let Some(parent) = curr {
                if parent.as_os_str().is_empty() {
                    break;
                }
                if !checked_dirs.insert(parent.to_path_buf()) {
                    // This directory and all its parent ancestors were already checked
                    break;
                }
                for pattern in &self.pinned_patterns {
                    let candidate = parent.join(pattern);
                    let candidate_str = candidate.to_string_lossy().to_string();
                    if let Ok(clean_candidate) = self.normalize_path(&candidate_str) {
                        self.insert_pinned_candidate(&mut new_pinned, clean_candidate);
                    }
                }
                curr = parent.parent();
            }
        }

        if self.pinned != new_pinned {
            self.pinned = new_pinned;
            true
        } else {
            false
        }
    }

    fn insert_pinned_candidate(
        &self,
        target_map: &mut BTreeMap<String, ContextItem>,
        clean_path: String,
    ) {
        if target_map.contains_key(&clean_path) {
            return;
        }
        if verify_path_within_repo(&self.repo_root, &clean_path).is_err() {
            return;
        }
        let full_path = self.repo_root.join(&clean_path);
        if full_path.is_file() {
            if let Some(existing) = self.pinned.get(&clean_path) {
                target_map.insert(clean_path, existing.clone());
            } else if let Ok(meta) = std::fs::metadata(&full_path) {
                let size = meta.len();
                let tokens = estimate_tokens_from_bytes(size);
                target_map.insert(
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_hierarchical_pinned_resolution() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        std::fs::write(root.join("Conventions.md"), "# Root Conventions").unwrap();
        std::fs::create_dir_all(root.join("crates/tui/src/views")).unwrap();
        std::fs::write(root.join("crates/tui/Conventions.md"), "# TUI Conventions").unwrap();
        std::fs::write(root.join("crates/tui/src/views/develop.rs"), "pub fn view() {}").unwrap();
        std::fs::create_dir_all(root.join("crates/core/src")).unwrap();
        std::fs::write(root.join("crates/core/Conventions.md"), "# Core Conventions").unwrap();
        std::fs::write(root.join("crates/core/src/lib.rs"), "pub fn core() {}").unwrap();

        let mut cm = ContextManager::new(root);
        cm.load_pinned(&["Conventions.md".to_string()]);

        // Initially only root Conventions.md is pinned
        assert!(cm.contains("Conventions.md").unwrap());
        assert!(!cm.contains("crates/tui/Conventions.md").unwrap());
        assert!(!cm.contains("crates/core/Conventions.md").unwrap());

        // Adding a file in crates/tui automatically includes crates/tui/Conventions.md in pinned
        cm.add_file("crates/tui/src/views/develop.rs", ContextAccess::Editable).unwrap();
        assert!(cm.contains("crates/tui/Conventions.md").unwrap());
        assert!(!cm.is_editable("crates/tui/Conventions.md").unwrap());
        assert!(!cm.contains("crates/core/Conventions.md").unwrap());

        // Adding a file in crates/core to auto layer automatically includes crates/core/Conventions.md
        cm.add_auto_file("crates/core/src/lib.rs", ContextAccess::ReadOnly).unwrap();
        assert!(cm.contains("crates/core/Conventions.md").unwrap());

        // Clearing auto removes crates/core/Conventions.md, but retains crates/tui/Conventions.md
        cm.clear_auto();
        assert!(!cm.contains("crates/core/Conventions.md").unwrap());
        assert!(cm.contains("crates/tui/Conventions.md").unwrap());

        // Removing the user file removes crates/tui/Conventions.md
        cm.remove_file("crates/tui/src/views/develop.rs").unwrap();
        assert!(!cm.contains("crates/tui/Conventions.md").unwrap());
        assert!(cm.contains("Conventions.md").unwrap());
    }

    #[test]
    fn test_context_manager_with_pinned_and_scoped_config_loader() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("pinned.md"), "# Pinned").unwrap();
        std::fs::write(
            root.join("tauqe.toml"),
            "[context]\npinned = [\"pinned.md\"]\n",
        )
        .unwrap();

        // ContextManager::new reads exclusively [context] via load_context_config
        let cm_auto = ContextManager::new(root.clone());
        assert_eq!(cm_auto.pinned_patterns(), &["pinned.md".to_string()]);
        assert!(cm_auto.contains("pinned.md").unwrap());

        // ContextManager::with_pinned sets patterns explicitly without disk config lookup
        let cm_explicit = ContextManager::with_pinned(root, &["other.md".to_string()]);
        assert_eq!(cm_explicit.pinned_patterns(), &["other.md".to_string()]);
    }

    #[test]
    fn test_add_auto_file_upgrades_pinned_layer_to_editable() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("pinned.rs"), "fn pinned() {}").unwrap();

        let mut cm = ContextManager::new(root);
        cm.load_pinned(&["pinned.rs".to_string()]);
        assert!(!cm.is_editable("pinned.rs").unwrap());

        cm.add_auto_file("pinned.rs", ContextAccess::Editable).unwrap();
        assert!(cm.is_editable("pinned.rs").unwrap());
    }

    #[test]
    fn test_auto_request_on_pinned_file_never_duplicates_into_auto_layer() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("pinned.rs"), "fn pinned() {}").unwrap();

        let mut cm = ContextManager::new(root);
        cm.load_pinned(&["pinned.rs".to_string()]);
        assert!(!cm.is_editable("pinned.rs").unwrap());

        // The model requests the already-pinned file as editable.
        cm.add_auto_file("pinned.rs", ContextAccess::Editable).unwrap();

        // Access is upgraded without duplicating the pinned file into auto.
        assert!(cm.is_editable("pinned.rs").unwrap());
        assert!(cm.auto_files().is_empty());

        // A read-only re-request stays a no-op for pinned files.
        let rev_before = cm.get_state().revision;
        cm.add_auto_file("pinned.rs", ContextAccess::ReadOnly).unwrap();
        assert_eq!(cm.get_state().revision, rev_before);
        assert!(cm.auto_files().is_empty());

        // The emitted context state reports the path exactly once.
        let state = cm.get_state();
        let rows: Vec<_> = state
            .items
            .iter()
            .filter(|i| i.path == "pinned.rs")
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].access, ContextAccess::Editable);
        assert_eq!(rows[0].layer, ContextLayer::Pinned);
    }

    #[test]
    fn test_merge_turn_context_syncs_upgraded_pinned_access() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("pinned.rs"), "fn pinned() {}").unwrap();

        let mut primary = ContextManager::new(root);
        primary.load_pinned(&["pinned.rs".to_string()]);
        assert!(!primary.is_editable("pinned.rs").unwrap());

        let mut turn_snapshot = primary.clone();
        turn_snapshot.add_auto_file("pinned.rs", ContextAccess::Editable).unwrap();
        assert!(turn_snapshot.is_editable("pinned.rs").unwrap());

        primary.merge_turn_context(&turn_snapshot);
        assert!(primary.is_editable("pinned.rs").unwrap());
    }
}
