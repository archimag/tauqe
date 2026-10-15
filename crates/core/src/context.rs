use anyhow::{bail, Result};
use crate::prompt::estimate_tokens_from_bytes;
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

/// Outcome of adding files by glob pattern in a single batch.
#[derive(Debug, Clone)]
pub struct PatternAddOutcome {
    /// Files newly added to the context layer.
    pub added_items: Vec<ContextItem>,
    /// Total estimated tokens contributed by the added files.
    pub added_tokens: u64,
    /// Whether the match set exceeded the batch limit and was capped.
    ///
    /// When `true`, some matching files were skipped because `max_batch` was
    /// reached. Callers should surface this so the cap is never silent.
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct ContextManager {
    repo_root: PathBuf,
    revision: u64,
    pinned_patterns: Vec<String>,
    pinned: BTreeMap<String, ContextItem>,
    user: BTreeMap<String, ContextItem>,
    auto: BTreeMap<String, ContextItem>,
}

/// Stores `item` in `layer`, returning `true` only when the layer contents
/// actually changed. Centralizing this check keeps every mutation path from
/// advancing the context revision on a no-op.
fn store_item(layer: &mut BTreeMap<String, ContextItem>, item: ContextItem) -> bool {
    if layer.get(&item.path) == Some(&item) {
        return false;
    }
    layer.insert(item.path.clone(), item);
    true
}

/// Promotes an existing entry to `Editable` in place. Auto-layer discovery
/// only ever upgrades access, so a read-only request is a no-op.
fn upgrade_access(item: &mut ContextItem, access: ContextAccess) -> bool {
    if access == ContextAccess::Editable && item.access != ContextAccess::Editable {
        item.access = ContextAccess::Editable;
        true
    } else {
        false
    }
}

/// Verifies that `relative_path` does not traverse or resolve to a symlink
/// outside `repo_root`.
///
/// If `relative_path` (or its deepest existing ancestor) points outside the
/// canonical repository root, returns an error.
pub fn verify_path_within_repo(repo_root: &Path, relative_path: &str) -> Result<()> {
    let Ok(canonical_root) = repo_root.canonicalize() else {
        // If repo_root cannot be canonicalized (e.g. does not exist in a synthetic test),
        // we cannot inspect symlinks on disk.
        return Ok(());
    };

    let full_path = repo_root.join(relative_path);

    // 1. If full_path exists (or is a symlink on disk):
    // symlink_metadata().is_ok() detects broken symlinks as well as existing files/dirs.
    if full_path.symlink_metadata().is_ok() {
        match full_path.canonicalize() {
            Ok(canonical_target) => {
                if !canonical_target.starts_with(&canonical_root) {
                    bail!(
                        "Path '{}' escapes repository root via symlink pointing to '{}'",
                        relative_path,
                        canonical_target.display()
                    );
                }
            }
            Err(err) => {
                bail!(
                    "Path '{}' cannot be canonicalized (broken symlink or inaccessible): {}",
                    relative_path,
                    err
                );
            }
        }
    } else {
        // 2. If full_path does not exist yet (e.g. a new file to create):
        // Walk up to find the deepest existing ancestor directory.
        let mut ancestor = full_path.as_path();
        while !ancestor.exists() && ancestor.symlink_metadata().is_err() {
            match ancestor.parent() {
                Some(p) if !p.as_os_str().is_empty() => ancestor = p,
                _ => break,
            }
        }

        if ancestor.exists() || ancestor.symlink_metadata().is_ok() {
            match ancestor.canonicalize() {
                Ok(canonical_ancestor) => {
                    if !canonical_ancestor.starts_with(&canonical_root) {
                        bail!(
                            "Path '{}' parent directory escapes repository root via symlink pointing to '{}'",
                            relative_path,
                            canonical_ancestor.display()
                        );
                    }
                }
                Err(err) => {
                    bail!(
                        "Path '{}' ancestor directory '{}' cannot be canonicalized: {}",
                        relative_path,
                        ancestor.display(),
                        err
                    );
                }
            }
        }
    }

    Ok(())
}

impl ContextManager {
    pub fn new(repo_root: PathBuf) -> Self {
        let mut cm = Self {
            repo_root,
            revision: 1,
            pinned_patterns: Vec::new(),
            pinned: BTreeMap::new(),
            user: BTreeMap::new(),
            auto: BTreeMap::new(),
        };
        cm.load_pinned_from_repo();
        cm
    }

    pub fn with_pinned(repo_root: PathBuf, pinned_patterns: &[String]) -> Self {
        let mut cm = Self {
            repo_root,
            revision: 1,
            pinned_patterns: Vec::new(),
            pinned: BTreeMap::new(),
            user: BTreeMap::new(),
            auto: BTreeMap::new(),
        };
        cm.load_pinned(pinned_patterns);
        cm
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn pinned_patterns(&self) -> &[String] {
        &self.pinned_patterns
    }

    pub fn set_repo_root(&mut self, repo_root: PathBuf) {
        self.repo_root = repo_root;
        self.load_pinned_from_repo();
    }

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
    fn recompute_pinned(&mut self) -> bool {
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
        // Reading every context file is blocking file I/O. Run it through the
        // blocking helper so a large context never stalls an async worker.
        crate::blocking::run_blocking(|| {
            let mut distinct_paths = BTreeSet::new();
            distinct_paths.extend(self.pinned.keys().cloned());
            distinct_paths.extend(self.user.keys().cloned());
            distinct_paths.extend(self.auto.keys().cloned());

            let mut files = Vec::new();
            for path in distinct_paths {
                if verify_path_within_repo(&self.repo_root, &path).is_err() {
                    continue;
                }
                let full_path = self.repo_root.join(&path);
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    let access = self
                        .effective_access_for(&path)
                        .unwrap_or(ContextAccess::ReadOnly);
                    files.push(ContextFileContent {
                        path,
                        access,
                        content,
                    });
                }
            }
            files
        })
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
        self.add_file_to_layer_internal(relative_path, access, layer, true)
    }

    fn add_file_to_layer_internal(
        &mut self,
        relative_path: &str,
        access: ContextAccess,
        layer: ContextLayer,
        recompute: bool,
    ) -> Result<ContextItem> {
        let clean_path = self.normalize_path(relative_path)?;
        verify_path_within_repo(&self.repo_root, &clean_path)?;
        let full_path = self.repo_root.join(&clean_path);

        if !full_path.is_file() {
            bail!("File not found or is a directory: {}", clean_path);
        }

        let metadata = std::fs::metadata(&full_path)?;
        let size_bytes = metadata.len();
        let estimated_tokens = estimate_tokens_from_bytes(size_bytes);

        let item = ContextItem {
            path: clean_path.clone(),
            access,
            size_bytes,
            estimated_tokens,
            layer,
        };

        let changed = match layer {
            ContextLayer::Pinned => store_item(&mut self.pinned, item.clone()),
            ContextLayer::User => {
                // Adding to the user layer promotes the path out of auto, so
                // the eviction is itself a change.
                let evicted = self.auto.remove(&clean_path).is_some();
                store_item(&mut self.user, item.clone()) || evicted
            }
            ContextLayer::Auto => {
                if let Some(user_item) = self.user.get_mut(&clean_path) {
                    upgrade_access(user_item, access)
                } else if self.pinned.contains_key(&clean_path) {
                    // Pinned entries are owned by configuration and are rebuilt
                    // on reload, so an editable override is tracked in the auto
                    // layer instead of mutating the pinned entry in place. A
                    // read-only request is a no-op because pinned is already
                    // read-only.
                    access == ContextAccess::Editable && store_item(&mut self.auto, item.clone())
                } else {
                    store_item(&mut self.auto, item.clone())
                }
            }
        };

        let pinned_changed = if recompute {
            self.recompute_pinned()
        } else {
            false
        };

        if changed || pinned_changed {
            self.revision += 1;
        }
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
            self.recompute_pinned();
            self.revision += 1;
        }
    }

    /// Drops all files strictly from the user context layer.
    /// Root pinned files are never affected; hierarchical pinned files are recomputed.
    pub fn clear_user(&mut self) {
        if !self.user.is_empty() {
            self.user.clear();
            self.recompute_pinned();
            self.revision += 1;
        }
    }

    /// Drops a file strictly from the auto context layer.
    /// Files present in `user` or `pinned` layers are never removed and return `Ok(false)`.
    pub fn drop_auto_file(&mut self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        if self.auto.remove(&clean_path).is_some() {
            self.recompute_pinned();
            self.revision += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Returns all paths currently tracked in the auto context layer.
    pub fn auto_files(&self) -> Vec<String> {
        self.auto.keys().cloned().collect()
    }

    /// Adds all repository files matching a glob pattern or directory prefix.
    ///
    /// Skips files already present in context. The number of files added in a
    /// single call is bounded by `max_batch`; when the match set exceeds it the
    /// batch is capped and the outcome reports `truncated = true` so the caller
    /// can surface the cap instead of silently dropping files.
    pub fn add_files_by_pattern(
        &mut self,
        pattern: &str,
        access: ContextAccess,
        available_files: &[String],
        max_batch: usize,
    ) -> Result<PatternAddOutcome> {
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

        // Cap the batch at the configured limit and remember whether files were
        // left out so the truncation is never silent.
        let truncated = matched_files.len() > max_batch;
        if truncated {
            matched_files.truncate(max_batch);
        }

        let mut added_items = Vec::new();
        let mut total_added_tokens = 0u64;
        let mut any_changed = false;

        for path in matched_files {
            let rev_before = self.revision;
            if let Ok(item) = self.add_file_to_layer_internal(&path, access, ContextLayer::User, false) {
                total_added_tokens += item.estimated_tokens;
                added_items.push(item);
                if self.revision > rev_before {
                    any_changed = true;
                }
            }
        }

        let pinned_changed = self.recompute_pinned();
        if pinned_changed && !any_changed {
            self.revision += 1;
        }

        Ok(PatternAddOutcome {
            added_items,
            added_tokens: total_added_tokens,
            truncated,
        })
    }

    pub fn update_file_metadata(&mut self, relative_path: &str) -> Result<()> {
        let clean_path = self.normalize_path(relative_path)?;
        let full_path = self.repo_root.join(&clean_path);
        let mut updated = false;
        if let Ok(metadata) = std::fs::metadata(&full_path) {
            let size = metadata.len();
            let tokens = estimate_tokens_from_bytes(size);
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

    /// Removes a path from the mutable context layers.
    ///
    /// Only the `user` and `auto` layers are emptied here. Pinned files are
    /// owned by project configuration and are deliberately left in place, so
    /// removing a pinned-only path reports `Ok(false)` (no change) instead of
    /// silently succeeding.
    pub fn remove_file(&mut self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        let removed_user = self.user.remove(&clean_path).is_some();
        let removed_auto = self.auto.remove(&clean_path).is_some();
        if removed_user || removed_auto {
            self.recompute_pinned();
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
                item.estimated_tokens = estimate_tokens_from_bytes(item.size_bytes);
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
            self.recompute_pinned();
            self.revision += 1;
        }

        Ok(())
    }

    pub fn set_access(&mut self, relative_path: &str, access: ContextAccess) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;

        // User and auto entries own their access directly.
        if let Some(item) = self.user.get_mut(&clean_path) {
            if item.access == access {
                return Ok(false);
            }
            item.access = access;
            self.revision += 1;
            return Ok(true);
        }
        if let Some(item) = self.auto.get_mut(&clean_path) {
            if item.access == access {
                return Ok(false);
            }
            item.access = access;
            self.revision += 1;
            return Ok(true);
        }

        // Pinned entries are owned by project configuration and are rebuilt
        // from `tauqe.toml` on reload, so writing the override into the pinned
        // layer would be silently discarded. Persist a user-layer override
        // instead; a read-only request matches the pinned base access and is
        // reported as no change.
        if let Some(pinned_item) = self.pinned.get(&clean_path).cloned() {
            if pinned_item.access == access {
                return Ok(false);
            }
            let user_item = ContextItem {
                access,
                layer: ContextLayer::User,
                ..pinned_item
            };
            if store_item(&mut self.user, user_item) {
                self.revision += 1;
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub fn clear(&mut self) {
        let had_items = !self.user.is_empty() || !self.auto.is_empty();
        if had_items {
            self.user.clear();
            self.auto.clear();
            self.recompute_pinned();
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

        let pinned_changed = self.recompute_pinned();

        if !removed.is_empty() || pinned_changed {
            self.revision += 1;
        }

        removed
    }

    /// Merges updates produced by a workflow run into the current context manager.
    /// Preserves user layer changes made concurrently. Updates the auto layer,
    /// syncs file renames/removals, and refreshes metadata.
    pub fn merge_turn_context(&mut self, turn_context: &ContextManager) {
        let mut changed = false;

        // 1. Sync auto layer: replace with turn_context.auto (excluding any path
        // already in the user layer).
        let previous_auto = std::mem::take(&mut self.auto);
        for (path, item) in &turn_context.auto {
            if !self.user.contains_key(path) {
                self.auto.insert(path.clone(), item.clone());
            }
        }
        changed |= previous_auto != self.auto;

        // 2. Remove files that no longer exist on disk (prunes deleted files).
        // The prune advances the revision itself when it drops anything, so its
        // outcome is not counted again here.
        self.prune_missing_files();

        // 3. Mirror any file renamed/moved into the user layer by the turn and
        // sync access upgrades from the turn. The pinned layer is configuration
        // owned and is never upgraded at runtime, so it needs no sync here.
        for (path, item) in &turn_context.user {
            if let Some(existing) = self.user.get_mut(path) {
                if existing.access == ContextAccess::ReadOnly
                    && item.access == ContextAccess::Editable
                {
                    existing.access = ContextAccess::Editable;
                    changed = true;
                }
            } else if self.repo_root.join(path).is_file() {
                self.user.insert(path.clone(), item.clone());
                changed = true;
            }
        }

        // Recompute hierarchical pinned files after merging user and auto layers.
        changed |= self.recompute_pinned();

        // 4. Refresh metadata for all files in context.
        for layer in [&mut self.pinned, &mut self.user, &mut self.auto] {
            for (path, item) in layer.iter_mut() {
                let full_path = self.repo_root.join(path);
                if let Ok(meta) = std::fs::metadata(&full_path) {
                    let size = meta.len();
                    let tokens = estimate_tokens_from_bytes(size);
                    if item.size_bytes != size || item.estimated_tokens != tokens {
                        item.size_bytes = size;
                        item.estimated_tokens = tokens;
                        changed = true;
                    }
                }
            }
        }

        if changed {
            self.revision += 1;
        }
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

        let clean = components.join("/");
        verify_path_within_repo(&self.repo_root, &clean)?;
        Ok(clean)
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
        let outcome = cm
            .add_files_by_pattern("src/*.rs", ContextAccess::ReadOnly, &repo_files, 150)
            .unwrap();

        assert_eq!(outcome.added_items.len(), 2);
        assert!(outcome.added_tokens > 0);
        assert!(!outcome.truncated);
        assert!(cm.contains("src/lib.rs").unwrap());
        assert!(cm.contains("src/main.rs").unwrap());
        assert!(!cm.contains("docs/Readme.md").unwrap());
    }

    #[test]
    fn test_add_files_by_pattern_reports_truncation() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn a() {}").unwrap();
        std::fs::write(root.join("src/b.rs"), "pub fn b() {}").unwrap();

        let repo_files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];

        let mut cm = ContextManager::new(root);
        let outcome = cm
            .add_files_by_pattern("src/*.rs", ContextAccess::ReadOnly, &repo_files, 1)
            .unwrap();

        assert_eq!(outcome.added_items.len(), 1);
        assert!(outcome.truncated);
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

    #[test]
    fn test_add_auto_file_upgrades_user_layer_to_editable() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("file.rs"), "fn test() {}").unwrap();

        let mut cm = ContextManager::new(root);
        cm.add_file("file.rs", ContextAccess::ReadOnly).unwrap();
        assert!(!cm.is_editable("file.rs").unwrap());

        cm.add_auto_file("file.rs", ContextAccess::Editable).unwrap();
        assert!(cm.is_editable("file.rs").unwrap());
    }

    #[test]
    fn test_merge_turn_context_syncs_upgraded_user_access() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("file.rs"), "fn test() {}").unwrap();

        let mut primary = ContextManager::new(root);
        primary.add_file("file.rs", ContextAccess::ReadOnly).unwrap();

        let mut turn_snapshot = primary.clone();
        turn_snapshot.add_auto_file("file.rs", ContextAccess::Editable).unwrap();
        assert!(turn_snapshot.is_editable("file.rs").unwrap());

        primary.merge_turn_context(&turn_snapshot);
        assert!(primary.is_editable("file.rs").unwrap());
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

    #[test]
    fn test_drop_auto_file_removes_only_from_auto_and_preserves_other_layers() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("pinned.rs"), "fn pinned() {}").unwrap();
        std::fs::write(root.join("user.rs"), "fn user() {}").unwrap();
        std::fs::write(root.join("auto.rs"), "fn auto() {}").unwrap();

        let mut cm = ContextManager::new(root);
        cm.load_pinned(&["pinned.rs".to_string()]);
        cm.add_file("user.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly).unwrap();

        assert_eq!(cm.auto_files(), vec!["auto.rs".to_string()]);

        // Dropping auto file succeeds
        assert!(cm.drop_auto_file("auto.rs").unwrap());
        assert!(!cm.contains("auto.rs").unwrap());
        assert!(cm.auto_files().is_empty());

        // Attempting to drop pinned or user files fails safely
        assert!(!cm.drop_auto_file("pinned.rs").unwrap());
        assert!(cm.contains("pinned.rs").unwrap());

        assert!(!cm.drop_auto_file("user.rs").unwrap());
        assert!(cm.contains("user.rs").unwrap());
    }

    #[test]
    fn test_drop_auto_file_normalization_and_revision() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("auto.rs"), "fn auto() {}").unwrap();

        let mut cm = ContextManager::new(root);
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly).unwrap();
        let rev_before = cm.get_state().revision;

        // Dropping non-existent path doesn't change revision
        assert!(!cm.drop_auto_file("nonexistent.rs").unwrap());
        assert_eq!(cm.get_state().revision, rev_before);

        // Dropping with relative/un-normalized path succeeds and bumps revision
        assert!(cm.drop_auto_file("./dir/../auto.rs").unwrap());
        assert!(!cm.contains("auto.rs").unwrap());
        assert!(cm.get_state().revision > rev_before);
    }

    #[test]
    fn test_merge_turn_context_reflects_auto_eviction() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("pinned.rs"), "fn pinned() {}").unwrap();
        std::fs::write(root.join("user.rs"), "fn user() {}").unwrap();
        std::fs::write(root.join("auto1.rs"), "fn auto1() {}").unwrap();
        std::fs::write(root.join("auto2.rs"), "fn auto2() {}").unwrap();

        let mut primary = ContextManager::new(root.clone());
        primary.load_pinned(&["pinned.rs".to_string()]);
        primary.add_file("user.rs", ContextAccess::ReadOnly).unwrap();
        primary.add_auto_file("auto1.rs", ContextAccess::ReadOnly).unwrap();

        let mut turn = primary.clone();
        // During turn, auto1 is dropped/evicted, auto2 is added
        turn.drop_auto_file("auto1.rs").unwrap();
        turn.add_auto_file("auto2.rs", ContextAccess::ReadOnly).unwrap();

        primary.merge_turn_context(&turn);

        // auto1 evicted from primary
        assert!(!primary.contains("auto1.rs").unwrap());
        // auto2 added to primary
        assert!(primary.contains("auto2.rs").unwrap());
        // pinned and user preserved
        assert!(primary.contains("pinned.rs").unwrap());
        assert!(primary.contains("user.rs").unwrap());
    }

    #[test]
    fn test_clear_user_removes_only_user_layer_and_preserves_others() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("pinned.rs"), "fn pinned() {}").unwrap();
        std::fs::write(root.join("user.rs"), "fn user() {}").unwrap();
        std::fs::write(root.join("auto.rs"), "fn auto() {}").unwrap();

        let mut cm = ContextManager::new(root);
        cm.load_pinned(&["pinned.rs".to_string()]);
        cm.add_file("user.rs", ContextAccess::Editable).unwrap();
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly).unwrap();

        let rev_before = cm.get_state().revision;
        cm.clear_user();

        assert!(!cm.contains("user.rs").unwrap());
        assert!(cm.contains("pinned.rs").unwrap());
        assert!(cm.contains("auto.rs").unwrap());
        assert!(cm.get_state().revision > rev_before);
    }

    #[test]
    fn test_clear_user_on_empty_layer_is_noop() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("auto.rs"), "fn auto() {}").unwrap();

        let mut cm = ContextManager::new(root);
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly).unwrap();
        let rev_before = cm.get_state().revision;

        cm.clear_user();

        assert_eq!(cm.get_state().revision, rev_before);
        assert!(cm.contains("auto.rs").unwrap());
    }

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
    #[cfg(unix)]
    fn test_symlink_escaping_repo_is_rejected() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let outside_root = outside.path().to_path_buf();

        let secret_file = outside_root.join("secret.txt");
        std::fs::write(&secret_file, "super secret").unwrap();

        // Symlink pointing to a file outside repo
        let sym_file = repo_root.join("sym_file.txt");
        std::os::unix::fs::symlink(&secret_file, &sym_file).unwrap();

        // Symlink pointing to a directory outside repo
        let sym_dir = repo_root.join("sym_dir");
        std::os::unix::fs::symlink(&outside_root, &sym_dir).unwrap();

        let mut cm = ContextManager::new(repo_root.clone());

        // File symlink escapes repo
        assert!(cm.normalize_path("sym_file.txt").is_err());
        assert!(cm.add_file("sym_file.txt", ContextAccess::ReadOnly).is_err());

        // Directory symlink escapes repo (both existing and non-existing files under it)
        assert!(cm.normalize_path("sym_dir/secret.txt").is_err());
        assert!(cm.normalize_path("sym_dir/new_file.txt").is_err());
        assert!(cm.add_file("sym_dir/secret.txt", ContextAccess::ReadOnly).is_err());

        // Symlink pointing inside repo is allowed
        let internal_dir = repo_root.join("src");
        std::fs::create_dir_all(&internal_dir).unwrap();
        let internal_file = internal_dir.join("lib.rs");
        std::fs::write(&internal_file, "pub fn ok() {}").unwrap();

        let internal_sym = repo_root.join("sym_lib.rs");
        std::os::unix::fs::symlink(&internal_file, &internal_sym).unwrap();

        assert!(cm.normalize_path("sym_lib.rs").is_ok());
        assert!(cm.add_file("sym_lib.rs", ContextAccess::ReadOnly).is_ok());

        // Reading context files ignores any file that escapes repo
        let files = cm.read_context_files();
        assert!(files.iter().any(|f| f.path == "sym_lib.rs"));
        assert!(!files.iter().any(|f| f.path == "sym_file.txt"));
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
}
