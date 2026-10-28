use crate::prompt::estimate_tokens_from_bytes;
use anyhow::{bail, Result};
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
pub use tauqe_protocol::matches_glob_pattern;
use tauqe_protocol::{ContextAccess, ContextItem, ContextLayer, ContextState, GlobPattern};

pub mod path;
pub mod pinned;

pub use path::verify_path_within_repo;

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

/// One document of the Refs context layer.
///
/// `item` is the read-only snapshot the model reads; `lru` is the monotonic
/// watermark of the most recent request for it. Eviction compares those
/// watermarks rather than arrival order, so a document requested again stays
/// resident while the ones nobody asked for are dropped first.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RefsEntry {
    item: ContextItem,
    lru: u64,
}

#[derive(Debug, Clone)]
pub struct ContextManager {
    repo_root: PathBuf,
    revision: u64,
    /// Monotonic source of `ContextItem::insertion_seq`.
    ///
    /// Every path that enters the context consumes one value, so the order of
    /// the watermarks is the order in which the files actually arrived. The
    /// counter never moves backwards, not even when a layer is cleared or a
    /// file is pruned, so a watermark is never handed out twice.
    insertion_counter: u64,
    pinned_patterns: Vec<String>,
    pinned: IndexMap<String, ContextItem>,
    user: IndexMap<String, ContextItem>,
    auto: IndexMap<String, ContextItem>,
    /// Documents materialized from the external reference registry.
    ///
    /// The layer is persistent for the whole task: it is context rather than
    /// transcript, so history compaction never touches it and only its own
    /// token budget can evict a document.
    refs: IndexMap<String, RefsEntry>,
    /// Monotonic source of `RefsEntry::lru`.
    refs_lru_counter: u64,
}

/// Stores `item` in `layer`, returning `true` only when the layer contents
/// actually changed. Centralizing this check keeps every mutation path from
/// advancing the context revision on a no-op.
fn store_item(layer: &mut IndexMap<String, ContextItem>, item: ContextItem) -> bool {
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

impl ContextManager {
    pub fn new(repo_root: PathBuf) -> Self {
        let mut cm = Self {
            repo_root,
            revision: 1,
            insertion_counter: 0,
            pinned_patterns: Vec::new(),
            pinned: IndexMap::new(),
            user: IndexMap::new(),
            auto: IndexMap::new(),
            refs: IndexMap::new(),
            refs_lru_counter: 0,
        };
        cm.load_pinned_from_repo();
        cm
    }

    pub fn with_pinned(repo_root: PathBuf, pinned_patterns: &[String]) -> Self {
        let mut cm = Self {
            repo_root,
            revision: 1,
            insertion_counter: 0,
            pinned_patterns: Vec::new(),
            pinned: IndexMap::new(),
            user: IndexMap::new(),
            auto: IndexMap::new(),
            refs: IndexMap::new(),
            refs_lru_counter: 0,
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
        // A reference document is read-only by construction, so it answers the
        // same way the read-only entries of the other three layers do.
        if found || self.refs.contains_key(clean_path) {
            Some(ContextAccess::ReadOnly)
        } else {
            None
        }
    }

    pub fn get_state(&self) -> ContextState {
        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut total_estimated_tokens = 0u64;

        // Emit every context path exactly once, in pinned -> refs -> user ->
        // auto order. A file present in the pinned layer must never be
        // duplicated when it also lives in another layer (for example an
        // editable override), so the first owning layer wins and the merged
        // effective access is applied to the single emitted item.
        let refs_items = self.refs.iter().map(|(path, entry)| (path, &entry.item));
        let ordered = self
            .pinned
            .iter()
            .chain(refs_items)
            .chain(self.user.iter())
            .chain(self.auto.iter());
        for (_, it) in ordered {
            if !seen.insert(it.path.clone()) {
                continue;
            }
            let mut item = it.clone();
            if let Some(eff) = self.effective_access_for(&item.path) {
                item.access = eff;
            }
            total_estimated_tokens += item.estimated_tokens;
            items.push(item);
        }

        ContextState {
            revision: self.revision,
            items,
            total_estimated_tokens,
        }
    }

    /// Reads every context file together with its insertion watermark.
    ///
    /// Files are emitted in layer order (pinned -> refs -> user -> auto) and in
    /// each layer's insertion order.
    fn read_context_files_with_seq(&self) -> Vec<(u64, ContextFileContent)> {
        // Reading every context file is blocking file I/O. Run it through the
        // blocking helper so a large context never stalls an async worker.
        crate::blocking::run_blocking(|| {
            // The context block is the stable prefix of every later round
            // within a turn, so an alphabetical re-sort here would shift tokens
            // and invalidate the provider prefix cache.
            let mut seen = std::collections::HashSet::new();
            let mut files = Vec::new();
            for path in self
                .pinned
                .keys()
                .chain(self.refs.keys())
                .chain(self.user.keys())
                .chain(self.auto.keys())
            {
                if !seen.insert(path.as_str()) {
                    continue;
                }
                if verify_path_within_repo(&self.repo_root, path).is_err() {
                    continue;
                }
                let full_path = self.repo_root.join(path);
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    let access = self
                        .effective_access_for(path)
                        .unwrap_or(ContextAccess::ReadOnly);
                    let insertion_seq = self.existing_insertion_seq(path).unwrap_or(0);
                    files.push((
                        insertion_seq,
                        ContextFileContent {
                            path: path.clone(),
                            access,
                            content,
                        },
                    ));
                }
            }
            files
        })
    }

    /// Reads the context files in semantic order: layer order (pinned -> refs
    /// -> user -> auto) and insertion order within each layer.
    pub fn read_context_files(&self) -> Vec<ContextFileContent> {
        self.read_context_files_with_seq()
            .into_iter()
            .map(|(_, file)| file)
            .collect()
    }

    /// Reads the context files ordered by their insertion watermark.
    ///
    /// The chronological layout renders its transcript by this order, so a file
    /// appears where it actually entered the context instead of where its layer
    /// places it. The sort is stable, so equal watermarks — concurrent additions
    /// folded in by `merge_turn_context` — keep the semantic layer order and the
    /// result stays deterministic.
    pub fn read_context_files_by_insertion(&self) -> Vec<ContextFileContent> {
        let mut files = self.read_context_files_with_seq();
        files.sort_by_key(|(insertion_seq, _)| *insertion_seq);
        files.into_iter().map(|(_, file)| file).collect()
    }

    pub fn contains(&self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        Ok(self.pinned.contains_key(&clean_path)
            || self.refs.contains_key(&clean_path)
            || self.user.contains_key(&clean_path)
            || self.auto.contains_key(&clean_path))
    }

    pub fn is_editable(&self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        Ok(self.effective_access_for(&clean_path) == Some(ContextAccess::Editable))
    }

    /// Returns all paths currently tracked in the Refs context layer.
    pub fn refs_files(&self) -> Vec<String> {
        self.refs.keys().cloned().collect()
    }

    /// Admits one external reference document into the Refs layer.
    ///
    /// The document is stored read-only, because it is a snapshot the harness
    /// materialized and the model must never rewrite it. A repeated request
    /// refreshes the LRU watermark only: the snapshot is immutable for the whole
    /// task, so no rendered byte changes and the context revision stays put.
    ///
    /// Insertion keeps the layer inside `refs_token_budget` by evicting the least
    /// recently requested documents. A document that alone exceeds the budget is
    /// rejected with a diagnostic instead of evicting every other document and
    /// failing anyway.
    pub fn add_refs_file(
        &mut self,
        relative_path: &str,
        refs_token_budget: usize,
    ) -> Result<ContextItem> {
        crate::blocking::run_blocking(|| {
            let clean_path = self.normalize_path(relative_path)?;
            let full_path = self.repo_root.join(&clean_path);
            if !full_path.is_file() {
                bail!("Reference document not found: {}", clean_path);
            }

            let existing = self.refs.get(&clean_path).map(|entry| entry.item.clone());
            if let Some(snapshot) = existing {
                self.bump_refs_lru(&clean_path);
                return Ok(snapshot);
            }

            let metadata = std::fs::metadata(&full_path)?;
            let size_bytes = metadata.len();
            let estimated_tokens = estimate_tokens_from_bytes(size_bytes);
            if estimated_tokens > refs_token_budget as u64 {
                bail!(
                    "Reference document '{}' needs ~{} tokens, more than the configured \
                     refs budget of {}",
                    clean_path,
                    estimated_tokens,
                    refs_token_budget
                );
            }

            self.refs_lru_counter += 1;
            let item = ContextItem {
                path: clean_path.clone(),
                access: ContextAccess::ReadOnly,
                size_bytes,
                estimated_tokens,
                layer: ContextLayer::Refs,
                insertion_seq: self.resolve_insertion_seq(&clean_path),
            };
            self.refs.insert(
                clean_path,
                RefsEntry {
                    item: item.clone(),
                    lru: self.refs_lru_counter,
                },
            );
            self.evict_refs_to_budget(refs_token_budget);
            self.revision += 1;
            Ok(item)
        })
    }

    /// Marks `relative_path` as the most recently requested reference document.
    ///
    /// The watermark only decides which document is evicted first, so refreshing
    /// it changes no rendered byte and does not advance the context revision.
    /// Returns `false` when the document is not in the Refs layer.
    pub fn touch_refs_lru(&mut self, relative_path: &str) -> bool {
        let Ok(clean_path) = self.normalize_path(relative_path) else {
            return false;
        };
        self.bump_refs_lru(&clean_path)
    }

    /// Refreshes the LRU watermark of an already-normalized Refs path.
    fn bump_refs_lru(&mut self, clean_path: &str) -> bool {
        if !self.refs.contains_key(clean_path) {
            return false;
        }
        self.refs_lru_counter += 1;
        if let Some(entry) = self.refs.get_mut(clean_path) {
            entry.lru = self.refs_lru_counter;
        }
        true
    }

    /// Evicts the least recently requested documents until the layer fits
    /// `refs_token_budget`, reporting whether anything was dropped.
    fn evict_refs_to_budget(&mut self, refs_token_budget: usize) -> bool {
        let mut evicted = false;
        loop {
            let total: u64 = self
                .refs
                .values()
                .map(|entry| entry.item.estimated_tokens)
                .sum();
            if total <= refs_token_budget as u64 {
                return evicted;
            }
            let victim = self
                .refs
                .iter()
                .min_by_key(|(_, entry)| entry.lru)
                .map(|(path, _)| path.clone());
            let Some(victim) = victim else {
                return evicted;
            };
            self.refs.shift_remove(&victim);
            evicted = true;
        }
    }

    /// Drops every document of the Refs context layer.
    ///
    /// Returns `true` only when the layer actually held documents, so callers can
    /// rotate the dialogue session exactly when the stable prompt prefix shrank.
    pub fn clear_refs(&mut self) -> bool {
        if self.refs.is_empty() {
            return false;
        }
        self.refs.clear();
        self.revision += 1;
        true
    }

    /// Returns the insertion watermark `clean_path` already carries.
    ///
    /// A path that lives in several layers reports the highest watermark it
    /// ever reached, which is the max-watermark rule `merge_turn_context`
    /// applies when it folds a turn back into the primary state.
    fn existing_insertion_seq(&self, clean_path: &str) -> Option<u64> {
        let mut watermark = None;
        for layer in [&self.pinned, &self.user, &self.auto] {
            if let Some(item) = layer.get(clean_path) {
                let seq = item.insertion_seq;
                watermark = Some(watermark.map_or(seq, |current: u64| current.max(seq)));
            }
        }
        if let Some(entry) = self.refs.get(clean_path) {
            let seq = entry.item.insertion_seq;
            watermark = Some(watermark.map_or(seq, |current: u64| current.max(seq)));
        }
        watermark
    }

    /// Returns the watermark an incoming item must carry.
    ///
    /// A path already in context keeps the watermark it was given when it first
    /// arrived. Allocating a fresh one on every add would move the file to the
    /// tail of the chronological timeline, and it would make an otherwise
    /// identical re-add look like a change that bumps the revision on its own.
    fn resolve_insertion_seq(&mut self, clean_path: &str) -> u64 {
        if let Some(seq) = self.existing_insertion_seq(clean_path) {
            return seq;
        }
        self.insertion_counter += 1;
        self.insertion_counter
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
        // Symlink verification, existence checks, and metadata reads are
        // blocking filesystem work, so the whole operation runs off the async
        // executor thread. The nested pinned recomputation stays inline.
        crate::blocking::run_blocking(|| {
            let clean_path = self.normalize_path(relative_path)?;
            verify_path_within_repo(&self.repo_root, &clean_path)?;
            let full_path = self.repo_root.join(&clean_path);

            if !full_path.is_file() {
                bail!("File not found or is a directory: {}", clean_path);
            }

            let metadata = std::fs::metadata(&full_path)?;
            let size_bytes = metadata.len();
            let estimated_tokens = estimate_tokens_from_bytes(size_bytes);

            let insertion_seq = self.resolve_insertion_seq(&clean_path);
            let item = ContextItem {
                path: clean_path.clone(),
                access,
                size_bytes,
                estimated_tokens,
                layer,
                insertion_seq,
            };

            let changed = match layer {
                ContextLayer::Pinned => store_item(&mut self.pinned, item.clone()),
                ContextLayer::User => {
                    // Adding to the user layer promotes the path out of auto, so
                    // the eviction is itself a change.
                    let evicted = self.auto.shift_remove(&clean_path).is_some();
                    store_item(&mut self.user, item.clone()) || evicted
                }
                ContextLayer::Auto => {
                    if let Some(user_item) = self.user.get_mut(&clean_path) {
                        upgrade_access(user_item, access)
                    } else if let Some(pinned_item) = self.pinned.get(&clean_path).cloned() {
                        // A pinned entry is owned by configuration and is always
                        // present in context, so it must never be duplicated into
                        // the auto layer. An editable request is persisted as a
                        // user-layer override (mirroring set_access); a read-only
                        // request matches the pinned base access and is a no-op.
                        if access == ContextAccess::Editable
                            && pinned_item.access != ContextAccess::Editable
                        {
                            let override_item = ContextItem {
                                access: ContextAccess::Editable,
                                layer: ContextLayer::User,
                                ..pinned_item
                            };
                            store_item(&mut self.user, override_item)
                        } else {
                            false
                        }
                    } else {
                        store_item(&mut self.auto, item.clone())
                    }
                }
                // The Refs layer belongs to the external-reference registry: a
                // document enters it only through `add_refs_file`, which applies
                // the LRU budget, so a manual add is refused rather than stored.
                ContextLayer::Refs => {
                    bail!("The Refs layer is registry-managed and cannot be filled manually")
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
        })
    }

    pub fn add_auto_file(
        &mut self,
        relative_path: &str,
        access: ContextAccess,
    ) -> Result<ContextItem> {
        self.add_file_to_layer(relative_path, access, ContextLayer::Auto)
    }

    /// Drops every file of the auto context layer.
    ///
    /// Returns `true` only when the layer actually held files, so callers can
    /// treat the removal as a deterministic signal that the stable prefix of
    /// the prompt shrank. A repeated clear on an already empty layer reports
    /// `false` instead of a change that did not happen.
    pub fn clear_auto(&mut self) -> bool {
        if !self.auto.is_empty() {
            self.auto.clear();
            self.recompute_pinned();
            self.revision += 1;
            true
        } else {
            false
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
        if self.auto.shift_remove(&clean_path).is_some() {
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

        // Compile the pattern once and match every candidate against it: the
        // repository file list can be large, and re-normalizing the pattern per
        // path is pure overhead.
        let compiled = GlobPattern::new(trimmed_pat);

        let mut matched_files: Vec<String> = available_files
            .iter()
            .filter(|path| compiled.matches(path))
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
            if let Ok(item) =
                self.add_file_to_layer_internal(&path, access, ContextLayer::User, false)
            {
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
        // The metadata read is blocking filesystem work, so it runs off the
        // async executor thread.
        crate::blocking::run_blocking(|| {
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
        })
    }

    /// Removes a path from the mutable context layers.
    ///
    /// The `user`, `auto`, and `refs` layers are emptied here. Pinned files are
    /// owned by project configuration and are deliberately left in place, so
    /// removing a pinned-only path reports `Ok(false)` (no change) instead of
    /// silently succeeding.
    pub fn remove_file(&mut self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        let removed_user = self.user.shift_remove(&clean_path).is_some();
        let removed_auto = self.auto.shift_remove(&clean_path).is_some();
        // A reference document leaves the layer on an explicit removal, because
        // dropping one document is a deliberate developer action; the registry
        // only withholds access and layer changes.
        let removed_refs = self.refs.shift_remove(&clean_path).is_some();
        if removed_user || removed_auto || removed_refs {
            self.recompute_pinned();
            self.revision += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Renames a file in context, preserving its layer, access level, and metadata.
    pub fn rename_file(&mut self, from_relative: &str, to_relative: &str) -> Result<()> {
        // The destination metadata read is blocking filesystem work, so it runs
        // off the async executor thread.
        crate::blocking::run_blocking(|| {
            let clean_from = self.normalize_path(from_relative)?;
            let clean_to = self.normalize_path(to_relative)?;
            if clean_from == clean_to {
                bail!("Source and destination paths are identical: {}", clean_from);
            }

            let mut found_item = None;
            let mut found_layer = None;

            if let Some(item) = self.user.shift_remove(&clean_from) {
                found_item = Some(item);
                found_layer = Some(ContextLayer::User);
            } else if let Some(item) = self.auto.shift_remove(&clean_from) {
                found_item = Some(item);
                found_layer = Some(ContextLayer::Auto);
            } else if self.pinned.contains_key(&clean_from) {
                bail!("Pinned file '{}' cannot be renamed or moved", clean_from);
            } else if self.refs.contains_key(&clean_from) {
                bail!(
                    "Reference document '{}' cannot be renamed or moved",
                    clean_from
                );
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
                    // Neither layer can reach this point: pinned and reference
                    // documents are rejected before the rename begins.
                    ContextLayer::Pinned | ContextLayer::Refs => {}
                }
                self.recompute_pinned();
                self.revision += 1;
            }

            Ok(())
        })
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

        if self.refs.contains_key(&clean_path) {
            bail!(
                "Reference document '{}' is read-only and its access cannot be changed",
                clean_path
            );
        }
        if self.pinned.contains_key(&clean_path) {
            bail!("Pinned file '{}' access cannot be changed", clean_path);
        }

        Ok(false)
    }

    /// Drops every file of the mutable user, auto, and refs context layers.
    ///
    /// Returns `true` only when at least one layer actually held files, so a
    /// clear that removed nothing is distinguishable from a clear that shrank
    /// the prompt prefix.
    pub fn clear(&mut self) -> bool {
        let had_items = !self.user.is_empty() || !self.auto.is_empty() || !self.refs.is_empty();
        if had_items {
            self.user.clear();
            self.auto.clear();
            self.refs.clear();
            self.recompute_pinned();
            self.revision += 1;
            true
        } else {
            false
        }
    }

    /// Prunes files from context that no longer exist on disk (e.g. after undoing AI creation).
    pub fn prune_missing_files(&mut self) -> Vec<String> {
        // Existence checks against every context path are blocking filesystem
        // work, so the prune runs off the async executor thread.
        crate::blocking::run_blocking(|| {
            let mut removed = Vec::new();
            let prune_layer =
                |layer: &mut IndexMap<String, ContextItem>, root: &Path| -> Vec<String> {
                    let to_remove: Vec<String> = layer
                        .keys()
                        .filter(|p| !root.join(p).is_file())
                        .cloned()
                        .collect();
                    for p in &to_remove {
                        layer.shift_remove(p);
                    }
                    to_remove
                };

            removed.extend(prune_layer(&mut self.pinned, &self.repo_root));
            removed.extend(prune_layer(&mut self.user, &self.repo_root));
            removed.extend(prune_layer(&mut self.auto, &self.repo_root));
            // A reference snapshot lives in the isolated cache, so a document
            // whose cached file disappeared is dropped like any other missing
            // context path instead of lingering as a dangling entry.
            let stale_refs: Vec<String> = self
                .refs
                .keys()
                .filter(|path| !self.repo_root.join(path).is_file())
                .cloned()
                .collect();
            for path in &stale_refs {
                self.refs.shift_remove(path);
            }
            removed.extend(stale_refs);

            let pinned_changed = self.recompute_pinned();

            if !removed.is_empty() || pinned_changed {
                self.revision += 1;
            }

            removed
        })
    }

    /// Merges updates produced by a workflow run into the current context manager.
    /// Preserves user layer changes made concurrently. Updates the auto layer,
    /// syncs file renames/removals, and refreshes metadata.
    pub fn merge_turn_context(&mut self, turn_context: &ContextManager) {
        // The prune and the metadata refresh both touch the filesystem, so the
        // whole merge runs off the async executor thread.
        crate::blocking::run_blocking(|| {
            let mut changed = false;

            // 1. Sync auto layer: replace with turn_context.auto (excluding any path
            // already in the user layer).
            let previous_auto = std::mem::take(&mut self.auto);
            for (path, item) in &turn_context.auto {
                if !self.user.contains_key(path) {
                    // Max-watermark rule: a path present on both sides keeps the
                    // highest watermark it ever reached, so a file the turn
                    // re-added holds its chronological position instead of
                    // jumping to the tail of the timeline.
                    let mut merged = item.clone();
                    if let Some(previous) = previous_auto.get(path) {
                        merged.insertion_seq = previous.insertion_seq.max(item.insertion_seq);
                    }
                    self.auto.insert(path.clone(), merged);
                }
            }
            changed |= previous_auto != self.auto;

            // 1b. Merge the turn's reference documents. The layer is persistent
            // for the whole task, so a document the turn materialized survives
            // the merge; a path present on both sides keeps the more recent LRU
            // watermark, so a document requested again inside the turn is not
            // evicted ahead of the ones it now outranks.
            for (path, entry) in &turn_context.refs {
                match self.refs.entry(path.clone()) {
                    indexmap::map::Entry::Occupied(occupied) => {
                        let slot = occupied.into_mut();
                        slot.lru = slot.lru.max(entry.lru);
                    }
                    indexmap::map::Entry::Vacant(vacant) => {
                        vacant.insert(entry.clone());
                        changed = true;
                    }
                }
            }

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
                    // The path may have entered another layer while the turn ran,
                    // so it keeps the higher of the two watermarks.
                    let mut merged = item.clone();
                    merged.insertion_seq = self
                        .existing_insertion_seq(path)
                        .map_or(item.insertion_seq, |seq| seq.max(item.insertion_seq));
                    self.user.insert(path.clone(), merged);
                    changed = true;
                }
            }

            // Recompute hierarchical pinned files after merging user and auto layers.
            changed |= self.recompute_pinned();

            // 4. Refresh metadata for all files in context, the reference
            // snapshots the turn materialized included.
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
            for (path, entry) in self.refs.iter_mut() {
                let full_path = self.repo_root.join(path);
                if let Ok(meta) = std::fs::metadata(&full_path) {
                    let size = meta.len();
                    let tokens = estimate_tokens_from_bytes(size);
                    if entry.item.size_bytes != size || entry.item.estimated_tokens != tokens {
                        entry.item.size_bytes = size;
                        entry.item.estimated_tokens = tokens;
                        changed = true;
                    }
                }
            }

            // The turn may have consumed watermarks the primary never saw, so
            // the counter advances to the highest value either side reached and
            // a concurrent addition can never reuse a watermark the turn already
            // handed out. Advancing the counter alone changes no bytes, so it
            // does not mark the merge as a change.
            self.insertion_counter = self.insertion_counter.max(turn_context.insertion_counter);
            // The LRU counter follows the same rule, so a document merged from
            // the turn keeps its eviction rank against later requests.
            self.refs_lru_counter = self.refs_lru_counter.max(turn_context.refs_lru_counter);

            if changed {
                self.revision += 1;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
    fn test_set_access_on_pinned_file_is_rejected() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("pinned.rs"), "pub fn pinned() {}").unwrap();

        let mut cm = ContextManager::new(root);
        cm.load_pinned(&["pinned.rs".to_string()]);
        assert!(!cm.is_editable("pinned.rs").unwrap());

        let res = cm.set_access("pinned.rs", ContextAccess::Editable);
        assert!(res.is_err());
        assert!(!cm.is_editable("pinned.rs").unwrap());
    }

    #[test]
    fn test_context_layers_and_permission_merging() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("pinned.rs"), "pub fn pinned() {}").unwrap();
        std::fs::write(root.join("auto.rs"), "pub fn auto() {}").unwrap();

        let mut cm = ContextManager::new(root.clone());
        cm.load_pinned(&["pinned.rs".to_string()]);
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();

        // Pinned is read-only
        assert!(!cm.is_editable("pinned.rs").unwrap());
        // Protected from removal
        assert!(!cm.remove_file("pinned.rs").unwrap());
        assert!(cm.contains("pinned.rs").unwrap());

        // When developer adds the pinned file as Editable to user layer, Editable takes absolute priority
        cm.add_file("pinned.rs", ContextAccess::Editable).unwrap();
        assert!(cm.is_editable("pinned.rs").unwrap());

        // Only an explicit manual addition moves a file into the user layer: it
        // evicts the auto entry so the path is never owned by two layers at once.
        cm.add_file("auto.rs", ContextAccess::ReadOnly).unwrap();
        assert!(cm.auto_files().is_empty());
        let state = cm.get_state();
        let promoted = state.items.iter().find(|i| i.path == "auto.rs").unwrap();
        assert_eq!(promoted.layer, ContextLayer::User);
    }

    #[test]
    fn test_merge_turn_context_preserves_concurrent_user_additions() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("base.rs"), "fn base() {}").unwrap();
        std::fs::write(root.join("auto.rs"), "fn auto() {}").unwrap();
        std::fs::write(root.join("user_added.rs"), "fn user_added() {}").unwrap();

        let mut primary = ContextManager::new(root.clone());
        primary
            .add_file("base.rs", ContextAccess::Editable)
            .unwrap();

        // Turn starts with a snapshot
        let mut turn_snapshot = primary.clone();
        turn_snapshot
            .add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();

        // While turn is running, user adds a new file to primary
        primary
            .add_file("user_added.rs", ContextAccess::Editable)
            .unwrap();

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

        cm.add_auto_file("file.rs", ContextAccess::Editable)
            .unwrap();
        assert!(cm.is_editable("file.rs").unwrap());
    }

    #[test]
    fn test_merge_turn_context_syncs_upgraded_user_access() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("file.rs"), "fn test() {}").unwrap();

        let mut primary = ContextManager::new(root);
        primary
            .add_file("file.rs", ContextAccess::ReadOnly)
            .unwrap();

        let mut turn_snapshot = primary.clone();
        turn_snapshot
            .add_auto_file("file.rs", ContextAccess::Editable)
            .unwrap();
        assert!(turn_snapshot.is_editable("file.rs").unwrap());

        primary.merge_turn_context(&turn_snapshot);
        assert!(primary.is_editable("file.rs").unwrap());
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
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();

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
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();
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
        primary
            .add_file("user.rs", ContextAccess::ReadOnly)
            .unwrap();
        primary
            .add_auto_file("auto1.rs", ContextAccess::ReadOnly)
            .unwrap();

        let mut turn = primary.clone();
        // During turn, auto1 is dropped/evicted, auto2 is added
        turn.drop_auto_file("auto1.rs").unwrap();
        turn.add_auto_file("auto2.rs", ContextAccess::ReadOnly)
            .unwrap();

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
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();

        let rev_before = cm.get_state().revision;
        cm.clear_user();

        assert!(!cm.contains("user.rs").unwrap());
        assert!(cm.contains("pinned.rs").unwrap());
        assert!(cm.contains("auto.rs").unwrap());
        assert!(cm.get_state().revision > rev_before);
    }

    #[test]
    fn test_clear_auto_reports_whether_files_were_removed() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("auto.rs"), "fn auto() {}").unwrap();

        let mut cm = ContextManager::new(root);
        assert!(
            !cm.clear_auto(),
            "clearing an empty auto layer must report no change"
        );

        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();
        assert!(
            cm.clear_auto(),
            "removing an auto file must report a change"
        );
        assert!(
            !cm.clear_auto(),
            "a repeated clear on an empty layer must report no change"
        );
    }

    #[test]
    fn test_clear_reports_whether_files_were_removed() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("user.rs"), "fn user() {}").unwrap();
        std::fs::write(root.join("auto.rs"), "fn auto() {}").unwrap();

        let mut cm = ContextManager::new(root);
        assert!(
            !cm.clear(),
            "clearing an empty context must report no change"
        );

        cm.add_file("user.rs", ContextAccess::Editable).unwrap();
        assert!(cm.clear(), "removing a user file must report a change");
        assert!(
            !cm.clear(),
            "a repeated clear on an empty context must report no change"
        );

        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();
        assert!(
            cm.clear(),
            "removing an auto-only file must report a change"
        );
    }

    #[test]
    fn test_clear_user_on_empty_layer_is_noop() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("auto.rs"), "fn auto() {}").unwrap();

        let mut cm = ContextManager::new(root);
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();
        let rev_before = cm.get_state().revision;

        cm.clear_user();

        assert_eq!(cm.get_state().revision, rev_before);
        assert!(cm.contains("auto.rs").unwrap());
    }

    #[test]
    fn test_layers_preserve_insertion_order_not_alphabetical() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for name in ["pinned.rs", "zeta.rs", "alpha.rs", "beta.rs", "gamma.rs"] {
            std::fs::write(root.join(name), "fn f() {}\n").unwrap();
        }

        let mut cm = ContextManager::new(root);
        cm.load_pinned(&["pinned.rs".to_string()]);
        // Insertion order deliberately contradicts alphabetical order.
        cm.add_file("zeta.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_file("alpha.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_auto_file("beta.rs", ContextAccess::ReadOnly)
            .unwrap();
        cm.add_auto_file("gamma.rs", ContextAccess::ReadOnly)
            .unwrap();

        let state = cm.get_state();
        let paths: Vec<&str> = state.items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["pinned.rs", "zeta.rs", "alpha.rs", "beta.rs", "gamma.rs"]
        );
    }

    #[test]
    fn test_re_adding_existing_file_keeps_its_position() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("zeta.rs"), "fn zeta() {}\n").unwrap();
        std::fs::write(root.join("alpha.rs"), "fn alpha() {}\n").unwrap();

        let mut cm = ContextManager::new(root);
        cm.add_file("zeta.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_file("alpha.rs", ContextAccess::ReadOnly).unwrap();
        // Re-adding an existing path upgrades access without moving the entry.
        cm.add_file("zeta.rs", ContextAccess::Editable).unwrap();

        let state = cm.get_state();
        let paths: Vec<&str> = state.items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, vec!["zeta.rs", "alpha.rs"]);
        let zeta = state.items.iter().find(|i| i.path == "zeta.rs").unwrap();
        assert_eq!(zeta.access, ContextAccess::Editable);
    }

    #[test]
    fn test_read_context_files_preserves_layer_and_insertion_order() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for name in ["pinned.rs", "zeta.rs", "alpha.rs"] {
            std::fs::write(root.join(name), "fn f() {}\n").unwrap();
        }

        let mut cm = ContextManager::new(root);
        cm.load_pinned(&["pinned.rs".to_string()]);
        cm.add_file("zeta.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_file("alpha.rs", ContextAccess::Editable).unwrap();

        let files = cm.read_context_files();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["pinned.rs", "zeta.rs", "alpha.rs"]);
        let alpha = files.iter().find(|f| f.path == "alpha.rs").unwrap();
        assert_eq!(alpha.access, ContextAccess::Editable);
    }

    fn watermark_of(cm: &ContextManager, path: &str) -> u64 {
        cm.get_state()
            .items
            .into_iter()
            .find(|item| item.path == path)
            .unwrap()
            .insertion_seq
    }

    #[test]
    fn test_insertion_watermarks_are_assigned_and_monotonic() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for name in ["pinned.rs", "user.rs", "auto.rs", "later.rs"] {
            std::fs::write(root.join(name), "fn f() {}\n").unwrap();
        }

        let mut cm = ContextManager::new(root.clone());
        cm.load_pinned(&["pinned.rs".to_string()]);
        cm.add_file("user.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();

        let pinned_seq = watermark_of(&cm, "pinned.rs");
        let user_seq = watermark_of(&cm, "user.rs");
        let auto_seq = watermark_of(&cm, "auto.rs");
        assert!(
            pinned_seq > 0,
            "a file that enters the context must carry a watermark"
        );
        assert!(user_seq > pinned_seq);
        assert!(auto_seq > user_seq);

        // A re-add of an existing path upgrades its access in place and keeps
        // its watermark, so the file never jumps to the tail of the timeline.
        cm.add_file("user.rs", ContextAccess::Editable).unwrap();
        assert_eq!(watermark_of(&cm, "user.rs"), user_seq);
        let readded = cm
            .get_state()
            .items
            .into_iter()
            .find(|item| item.path == "user.rs")
            .unwrap();
        assert_eq!(readded.access, ContextAccess::Editable);

        // A file that arrives after the previous ones continues the timeline.
        cm.add_auto_file("later.rs", ContextAccess::ReadOnly)
            .unwrap();
        assert!(watermark_of(&cm, "later.rs") > auto_seq);
    }

    #[test]
    fn test_merge_turn_context_keeps_turn_watermarks_and_advances_counter() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for name in ["base.rs", "turn_added.rs", "after_merge.rs"] {
            std::fs::write(root.join(name), "fn f() {}\n").unwrap();
        }

        let mut primary = ContextManager::new(root.clone());
        primary
            .add_file("base.rs", ContextAccess::ReadOnly)
            .unwrap();
        let base_seq = watermark_of(&primary, "base.rs");

        // The turn allocates watermarks of its own while the primary is idle.
        let mut turn = primary.clone();
        turn.add_auto_file("turn_added.rs", ContextAccess::ReadOnly)
            .unwrap();
        let turn_seq = watermark_of(&turn, "turn_added.rs");
        assert!(turn_seq > base_seq);

        primary.merge_turn_context(&turn);
        assert_eq!(
            watermark_of(&primary, "turn_added.rs"),
            turn_seq,
            "the chronology collected by the turn must survive the merge"
        );
        assert_eq!(watermark_of(&primary, "base.rs"), base_seq);

        // The merged counter advances past every watermark the turn handed out,
        // so the next addition can never reuse one of them.
        primary
            .add_auto_file("after_merge.rs", ContextAccess::ReadOnly)
            .unwrap();
        assert!(watermark_of(&primary, "after_merge.rs") > turn_seq);
    }

    #[test]
    fn test_insertion_order_follows_watermarks_while_semantic_order_is_unchanged() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for name in ["pinned.rs", "auto.rs", "user.rs"] {
            std::fs::write(root.join(name), "fn f() {}\n").unwrap();
        }

        let mut cm = ContextManager::new(root.clone());
        cm.load_pinned(&["pinned.rs".to_string()]);
        // The auto file arrives first and the user file afterwards, so the
        // chronological order deliberately contradicts the layer order.
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();
        cm.add_file("user.rs", ContextAccess::ReadOnly).unwrap();

        let semantic: Vec<String> = cm
            .read_context_files()
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert_eq!(semantic, vec!["pinned.rs", "user.rs", "auto.rs"]);

        let chronological: Vec<String> = cm
            .read_context_files_by_insertion()
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert_eq!(chronological, vec!["pinned.rs", "auto.rs", "user.rs"]);
    }

    #[test]
    fn test_refs_layer_emits_read_only_documents_in_layer_order() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for name in ["pinned.rs", "refs.md", "user.rs", "auto.rs"] {
            std::fs::write(root.join(name), "fn f() {}\n").unwrap();
        }

        let mut cm = ContextManager::new(root);
        cm.load_pinned(&["pinned.rs".to_string()]);
        let document = cm.add_refs_file("refs.md", 10_000).unwrap();
        cm.add_file("user.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();

        assert_eq!(document.layer, ContextLayer::Refs);
        assert_eq!(document.access, ContextAccess::ReadOnly);
        assert!(!cm.is_editable("refs.md").unwrap());
        assert_eq!(cm.refs_files(), vec!["refs.md".to_string()]);

        let state = cm.get_state();
        let paths: Vec<&str> = state.items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["pinned.rs", "refs.md", "user.rs", "auto.rs"]);
        let emitted = state
            .items
            .iter()
            .find(|item| item.path == "refs.md")
            .unwrap();
        assert_eq!(emitted.layer, ContextLayer::Refs);
        assert_eq!(emitted.access, ContextAccess::ReadOnly);

        let files = cm.read_context_files();
        let file_paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            file_paths,
            vec!["pinned.rs", "refs.md", "user.rs", "auto.rs"]
        );
    }

    #[test]
    fn test_a_repeated_refs_request_touches_the_lru_without_a_revision_change() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("refs.md"), "reference notes\n").unwrap();

        let mut cm = ContextManager::new(root);
        cm.add_refs_file("refs.md", 10_000).unwrap();
        let revision = cm.get_state().revision;

        // A repeated request refreshes the watermark only: the snapshot is
        // immutable for the whole task, so nothing rendered changes.
        let again = cm.add_refs_file("refs.md", 10_000).unwrap();
        assert_eq!(again.layer, ContextLayer::Refs);
        assert_eq!(cm.get_state().revision, revision);
        assert_eq!(cm.refs_files(), vec!["refs.md".to_string()]);

        assert!(cm.touch_refs_lru("refs.md"));
        assert!(!cm.touch_refs_lru("missing.md"));
        assert_eq!(cm.get_state().revision, revision);
    }

    #[test]
    fn test_refs_layer_evicts_the_least_recently_requested_document() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.md"), "reference notes\n").unwrap();
        std::fs::write(root.join("b.md"), "reference notes\n").unwrap();
        std::fs::write(root.join("c.md"), "reference notes\n").unwrap();

        let mut cm = ContextManager::new(root);
        let first = cm.add_refs_file("a.md", 10_000).unwrap();
        let second = cm.add_refs_file("b.md", 10_000).unwrap();
        // The repeated request pushes `a` ahead of `b`, so `b` becomes the least
        // recently requested document even though it arrived last.
        cm.add_refs_file("a.md", 10_000).unwrap();

        // The budget fits exactly two equal documents, so admitting the third
        // drops the least recently requested one.
        let budget = (first.estimated_tokens + second.estimated_tokens) as usize;
        cm.add_refs_file("c.md", budget).unwrap();

        assert_eq!(
            cm.refs_files(),
            vec!["a.md".to_string(), "c.md".to_string()]
        );
    }

    #[test]
    fn test_a_refs_document_larger_than_the_budget_is_rejected() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("huge.md"), "x".repeat(4096)).unwrap();

        let mut cm = ContextManager::new(root);
        let error = cm
            .add_refs_file("huge.md", 1)
            .expect_err("a document larger than the budget is refused");

        assert!(
            error.to_string().contains("refs budget"),
            "unexpected error: {error}"
        );
        assert!(cm.refs_files().is_empty());
        assert!(!cm.contains("huge.md").unwrap());
    }

    #[test]
    fn test_the_refs_layer_survives_clear_auto_and_a_turn_merge() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("base.md"), "base reference\n").unwrap();
        std::fs::write(root.join("turn.md"), "turn reference\n").unwrap();
        std::fs::write(root.join("auto.rs"), "fn auto() {}\n").unwrap();

        let mut primary = ContextManager::new(root);
        primary.add_refs_file("base.md", 10_000).unwrap();
        primary
            .add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();
        let mut turn = primary.clone();
        turn.add_refs_file("turn.md", 10_000).unwrap();

        primary.merge_turn_context(&turn);
        assert_eq!(
            primary.refs_files(),
            vec!["base.md".to_string(), "turn.md".to_string()]
        );

        // The Auto layer is transient, while reference documents persist for the
        // whole task and survive its removal.
        primary.clear_auto();
        assert_eq!(
            primary.refs_files(),
            vec!["base.md".to_string(), "turn.md".to_string()]
        );

        assert!(primary.clear());
        assert!(primary.refs_files().is_empty());
    }

    #[test]
    fn test_set_access_on_a_refs_document_is_rejected() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("refs.md"), "reference notes\n").unwrap();

        let mut cm = ContextManager::new(root);
        cm.add_refs_file("refs.md", 10_000).unwrap();

        assert!(cm.set_access("refs.md", ContextAccess::Editable).is_err());
        assert!(!cm.is_editable("refs.md").unwrap());
    }

    #[test]
    fn test_the_refs_layer_cannot_be_filled_manually() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("refs.md"), "reference notes\n").unwrap();

        let mut cm = ContextManager::new(root);
        let error = cm
            .add_file_to_layer("refs.md", ContextAccess::ReadOnly, ContextLayer::Refs)
            .expect_err("the registry owns the Refs layer");

        assert!(
            error.to_string().contains("registry"),
            "unexpected error: {error}"
        );
        assert!(cm.refs_files().is_empty());
    }

    #[test]
    fn test_clear_refs_reports_whether_documents_were_removed() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("refs.md"), "reference notes\n").unwrap();

        let mut cm = ContextManager::new(root);
        assert!(!cm.clear_refs(), "an empty Refs layer reports no change");
        cm.add_refs_file("refs.md", 10_000).unwrap();
        assert!(cm.clear_refs(), "removing a document reports a change");
        assert!(!cm.clear_refs(), "a repeated clear reports no change");
        assert!(cm.refs_files().is_empty());
    }
}
