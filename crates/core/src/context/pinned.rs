use indexmap::IndexMap;
use std::collections::BTreeSet;
use std::path::Path;

use crate::prompt::estimate_tokens_from_bytes;
use crate::refs::REFS_MANIFEST;
use tauqe_protocol::{ContextAccess, ContextItem, ContextLayer};

use super::ContextManager;

impl ContextManager {
    pub fn load_pinned_from_repo(&mut self) {
        // Reading `tauqe.toml` and resolving the pinned ancestors is blocking
        // filesystem work, so it runs off the async executor thread.
        crate::blocking::run_blocking(|| {
            let config = crate::config::load_context_config(&self.repo_root);
            self.load_pinned(&config.pinned);
        });
    }

    pub fn load_pinned(&mut self, pinned_paths: &[String]) {
        self.pinned_patterns = pinned_paths.to_vec();
        self.recompute_pinned();
    }

    /// Recomputes the pinned context layer with hierarchical resolution.
    ///
    /// Root-level pinned files matching `pinned_patterns` from `repo_root` are
    /// always included, and so is the external reference registry manifest
    /// (`.tauqe/refs/refs.md`) when it exists on disk. The manifest is the
    /// showcase that lists the declared reference documents, so pinning it lets
    /// the model read the catalogue without requesting it first; a repository
    /// that declares no manifest contributes no entry.
    ///
    /// In addition, for every file present in the `user` or `auto` context
    /// layers, all parent directories up to the project root are traversed: at
    /// each level, every pinned pattern is resolved as a relative path and any
    /// existing file is added to the pinned layer (read-only, deduplicated,
    /// never escaping repo root).
    pub(super) fn recompute_pinned(&mut self) -> bool {
        // The ancestor walk, `verify_path_within_repo` symlink checks, and
        // `metadata()` inspections are all blocking filesystem work, so the
        // whole resolution runs off the async executor thread.
        crate::blocking::run_blocking(|| {
            let mut new_pinned = IndexMap::new();

            // 1. Root-level pinned files
            for pattern in &self.pinned_patterns {
                if let Ok(clean_path) = self.normalize_path(pattern) {
                    insert_pinned_candidate(
                        &self.repo_root,
                        &self.pinned,
                        &mut new_pinned,
                        &mut self.insertion_counter,
                        clean_path,
                    );
                }
            }

            // 2. The external reference registry showcase (`.tauqe/refs/refs.md`).
            //
            // The manifest is the catalogue of declared reference documents, so
            // pinning it renders the showcase into the `<context>` block without
            // the model having to request it. A missing manifest is not an
            // error: `insert_pinned_candidate` admits only files that exist. An
            // explicitly pinned manifest is already in the map, so it is neither
            // duplicated nor moved to a new insertion watermark.
            if let Ok(clean_manifest) = self.normalize_path(REFS_MANIFEST) {
                insert_pinned_candidate(
                    &self.repo_root,
                    &self.pinned,
                    &mut new_pinned,
                    &mut self.insertion_counter,
                    clean_manifest,
                );
            }

            // 3. Hierarchical pinned files for all items in user and auto layers
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
                        // A candidate that is not a file on disk can never enter
                        // the pinned layer, so probe its existence before paying
                        // for normalization and the component-wise symlink walk.
                        if !self.repo_root.join(&candidate).is_file() {
                            continue;
                        }
                        let candidate_str = candidate.to_string_lossy().to_string();
                        if let Ok(clean_candidate) = self.normalize_path(&candidate_str) {
                            insert_pinned_candidate(
                                &self.repo_root,
                                &self.pinned,
                                &mut new_pinned,
                                &mut self.insertion_counter,
                                clean_candidate,
                            );
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
        })
    }
}

/// Records one pinned candidate that has already been verified.
///
/// `clean_path` always arrives from `ContextManager::normalize_path`, which
/// rejects absolute and repository-escaping paths and walks every symlink
/// component. Verifying it again here would repeat the same walk for every
/// pattern of every ancestor directory of every context file.
///
/// A candidate that is not yet in the pinned layer is a genuinely new entry, so
/// it consumes the next insertion watermark. Reusing the watermark of an
/// existing candidate keeps a recomputation from moving every pinned file to
/// the tail of the chronological timeline.
fn insert_pinned_candidate(
    repo_root: &Path,
    pinned: &IndexMap<String, ContextItem>,
    target_map: &mut IndexMap<String, ContextItem>,
    insertion_counter: &mut u64,
    clean_path: String,
) {
    if target_map.contains_key(&clean_path) {
        return;
    }
    let full_path = repo_root.join(&clean_path);
    if !full_path.is_file() {
        return;
    }
    if let Some(existing) = pinned.get(&clean_path) {
        target_map.insert(clean_path, existing.clone());
    } else if let Ok(meta) = std::fs::metadata(&full_path) {
        let size = meta.len();
        let tokens = estimate_tokens_from_bytes(size);
        *insertion_counter += 1;
        target_map.insert(
            clean_path.clone(),
            ContextItem {
                path: clean_path,
                access: ContextAccess::ReadOnly,
                size_bytes: size,
                estimated_tokens: tokens,
                layer: ContextLayer::Pinned,
                insertion_seq: *insertion_counter,
            },
        );
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
        std::fs::write(
            root.join("crates/tui/src/views/develop.rs"),
            "pub fn view() {}",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("crates/core/src")).unwrap();
        std::fs::write(
            root.join("crates/core/Conventions.md"),
            "# Core Conventions",
        )
        .unwrap();
        std::fs::write(root.join("crates/core/src/lib.rs"), "pub fn core() {}").unwrap();

        let mut cm = ContextManager::new(root);
        cm.load_pinned(&["Conventions.md".to_string()]);

        // Initially only root Conventions.md is pinned
        assert!(cm.contains("Conventions.md").unwrap());
        assert!(!cm.contains("crates/tui/Conventions.md").unwrap());
        assert!(!cm.contains("crates/core/Conventions.md").unwrap());

        // Adding a file in crates/tui automatically includes crates/tui/Conventions.md in pinned
        cm.add_file("crates/tui/src/views/develop.rs", ContextAccess::Editable)
            .unwrap();
        assert!(cm.contains("crates/tui/Conventions.md").unwrap());
        assert!(!cm.is_editable("crates/tui/Conventions.md").unwrap());
        assert!(!cm.contains("crates/core/Conventions.md").unwrap());

        // Adding a file in crates/core to auto layer automatically includes crates/core/Conventions.md
        cm.add_auto_file("crates/core/src/lib.rs", ContextAccess::ReadOnly)
            .unwrap();
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

        cm.add_auto_file("pinned.rs", ContextAccess::Editable)
            .unwrap();
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
        cm.add_auto_file("pinned.rs", ContextAccess::Editable)
            .unwrap();

        // Access is upgraded without duplicating the pinned file into auto.
        assert!(cm.is_editable("pinned.rs").unwrap());
        assert!(cm.auto_files().is_empty());

        // A read-only re-request stays a no-op for pinned files.
        let rev_before = cm.get_state().revision;
        cm.add_auto_file("pinned.rs", ContextAccess::ReadOnly)
            .unwrap();
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
        turn_snapshot
            .add_auto_file("pinned.rs", ContextAccess::Editable)
            .unwrap();
        assert!(turn_snapshot.is_editable("pinned.rs").unwrap());

        primary.merge_turn_context(&turn_snapshot);
        assert!(primary.is_editable("pinned.rs").unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recompute_pinned_resolves_hierarchy_on_async_runtime() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("Conventions.md"), "# Root Conventions").unwrap();
        std::fs::create_dir_all(root.join("crates/core/src")).unwrap();
        std::fs::write(
            root.join("crates/core/Conventions.md"),
            "# Core Conventions",
        )
        .unwrap();
        std::fs::write(root.join("crates/core/src/lib.rs"), "pub fn core() {}").unwrap();

        let mut cm = ContextManager::with_pinned(root, &["Conventions.md".to_string()]);
        cm.add_file("crates/core/src/lib.rs", ContextAccess::Editable)
            .unwrap();
        assert!(cm.contains("crates/core/Conventions.md").unwrap());

        cm.clear_user();
        assert!(cm.contains("Conventions.md").unwrap());
        assert!(!cm.contains("crates/core/Conventions.md").unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn test_hierarchical_pinned_skips_symlink_escaping_repository() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        std::fs::create_dir_all(root.join("crates/core/src")).unwrap();
        std::fs::write(root.join("crates/core/src/lib.rs"), "pub fn core() {}").unwrap();
        std::fs::write(root.join("Conventions.md"), "# Root Conventions").unwrap();

        let outside_file = outside.path().join("Conventions.md");
        std::fs::write(&outside_file, "# Outside Conventions").unwrap();
        std::os::unix::fs::symlink(&outside_file, root.join("crates/core/Conventions.md")).unwrap();

        let mut cm = ContextManager::with_pinned(root, &["Conventions.md".to_string()]);
        cm.add_file("crates/core/src/lib.rs", ContextAccess::Editable)
            .unwrap();

        // The candidate exists on disk, so it reaches the symlink verification
        // inside normalization and is rejected there instead of being pinned.
        let state = cm.get_state();
        assert!(!state
            .items
            .iter()
            .any(|item| item.path == "crates/core/Conventions.md"));
        assert!(!cm
            .read_context_files()
            .iter()
            .any(|file| file.path == "crates/core/Conventions.md"));
    }
    #[test]
    fn test_the_registry_manifest_is_pinned_read_only_and_rendered() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let manifest = root.join(REFS_MANIFEST);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &manifest,
            "- [API](vendor/api.md) = https://example.com/llm.txt\n",
        )
        .unwrap();

        let cm = ContextManager::new(root);
        assert!(cm.contains(REFS_MANIFEST).unwrap());
        assert!(!cm.is_editable(REFS_MANIFEST).unwrap());

        let state = cm.get_state();
        let showcased: Vec<_> = state
            .items
            .iter()
            .filter(|item| item.path == REFS_MANIFEST)
            .collect();
        assert_eq!(
            showcased.len(),
            1,
            "the showcase must be pinned exactly once"
        );
        assert_eq!(showcased[0].layer, ContextLayer::Pinned);
        assert_eq!(showcased[0].access, ContextAccess::ReadOnly);

        let files = cm.read_context_files();
        assert!(files.iter().any(|file| file.path == REFS_MANIFEST));
    }

    #[test]
    fn test_the_showcase_keeps_its_watermark_across_recomputations() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let manifest = root.join(REFS_MANIFEST);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, "# References\n").unwrap();
        std::fs::write(root.join("auto.rs"), "fn auto() {}\n").unwrap();

        let mut cm = ContextManager::new(root);
        let watermark = cm
            .get_state()
            .items
            .into_iter()
            .find(|item| item.path == REFS_MANIFEST)
            .expect("the showcase is pinned on construction")
            .insertion_seq;

        // Any later recomputation must reuse the watermark the manifest was
        // given when it first entered the context, so the showcase never jumps
        // to the tail of the chronological timeline.
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();
        assert!(!cm.recompute_pinned(), "the pinned layer must not change");

        let again: Vec<_> = cm
            .get_state()
            .items
            .into_iter()
            .filter(|item| item.path == REFS_MANIFEST)
            .collect();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].insertion_seq, watermark);
    }

    #[test]
    fn test_an_explicitly_pinned_manifest_is_not_duplicated_by_the_showcase() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let manifest = root.join(REFS_MANIFEST);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, "# References\n").unwrap();

        let cm = ContextManager::with_pinned(root, &[REFS_MANIFEST.to_string()]);
        let state = cm.get_state();
        assert_eq!(
            state
                .items
                .iter()
                .filter(|item| item.path == REFS_MANIFEST)
                .count(),
            1,
            "an explicitly pinned manifest must be admitted exactly once"
        );
        assert!(!cm.is_editable(REFS_MANIFEST).unwrap());
    }

    #[test]
    fn test_a_repository_without_a_manifest_pins_no_showcase() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("file.rs"), "fn file() {}\n").unwrap();

        let mut cm = ContextManager::new(root);
        cm.add_file("file.rs", ContextAccess::ReadOnly).unwrap();

        assert!(!cm.contains(REFS_MANIFEST).unwrap());
        assert!(!cm
            .get_state()
            .items
            .iter()
            .any(|item| item.path == REFS_MANIFEST));
    }

    #[test]
    fn test_the_showcase_leaves_the_hierarchical_pinned_walk_untouched() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let manifest = root.join(REFS_MANIFEST);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::create_dir_all(root.join("crates/core/src")).unwrap();
        std::fs::write(&manifest, "# References\n").unwrap();
        std::fs::write(root.join("Conventions.md"), "# Root Conventions").unwrap();
        std::fs::write(
            root.join("crates/core/Conventions.md"),
            "# Core Conventions",
        )
        .unwrap();
        std::fs::write(root.join("crates/core/src/lib.rs"), "pub fn core() {}").unwrap();

        let mut cm = ContextManager::with_pinned(root, &["Conventions.md".to_string()]);
        cm.add_file("crates/core/src/lib.rs", ContextAccess::Editable)
            .unwrap();
        assert!(cm.contains(REFS_MANIFEST).unwrap());
        assert!(cm.contains("Conventions.md").unwrap());
        assert!(cm.contains("crates/core/Conventions.md").unwrap());

        // The hierarchical entry follows the user layer, while the root pattern
        // and the showcase stay pinned.
        cm.clear_user();
        assert!(cm.contains(REFS_MANIFEST).unwrap());
        assert!(cm.contains("Conventions.md").unwrap());
        assert!(!cm.contains("crates/core/Conventions.md").unwrap());
    }
}
