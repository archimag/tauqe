use crate::prompt::estimate_tokens_from_bytes;
use anyhow::{bail, Result};
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
pub use tauqe_protocol::matches_glob_pattern;
use tauqe_protocol::{ContextAccess, ContextItem, ContextLayer, ContextState, GlobPattern};

pub mod path;
pub mod pinned;

pub use path::verify_path_within_repo;
pub use path::verify_path_within_repo_for_write;
#[derive(Debug, Clone)]
pub struct ContextFileContent {
    pub path: String,
    pub access: ContextAccess,
    pub content: String,
    /// Arrival watermark of the context entry the content came from.
    ///
    /// The value is the `insertion_seq` the path holds in the context layers,
    /// so a reader that places files chronologically uses the order in which
    /// they actually arrived instead of where their layer sits. `0` denotes an
    /// unrecorded arrival: a file created during the current turn, or a payload
    /// synthesized outside the context layers. Watermarks are allocated from
    /// `1`, so `0` is never a real one.
    pub insertion_seq: u64,
}

/// Outcome of adding files by glob pattern in a single batch.
#[derive(Debug, Clone)]
pub struct PatternAddOutcome {
    /// Files newly added to the context layer.
    pub added_items: Vec<ContextItem>,
    /// Total estimated tokens the added files consume, the wrapper their
    /// `<file>` elements render included.
    pub added_tokens: u64,
    /// Whether the file token budget dropped matched files.
    ///
    /// When `true`, some matching files were skipped because admitting them
    /// would have breached the file token budget. Callers should surface this
    /// so the omission is never silent.
    pub truncated: bool,
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
    /// Documents materialized from the external documentation registry.
    ///
    /// The layer is part of the transient context: it is context rather than
    /// transcript, so history compaction never touches it, and clearing the Auto
    /// or the User layer on its own leaves it intact. The transient context is
    /// released as a whole through [`Self::clear_transient`], and a full context
    /// clear drops the documents together with every other layer. It keeps no
    /// eviction order of its own: a snapshot is priced like every other file and
    /// stays resident until the transient context is released.
    outer_docs: IndexMap<String, ContextItem>,
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

/// Promotes an existing entry to `Editable` in place. An automatic addition
/// only ever upgrades access, so a read-only request is a no-op.
fn upgrade_access(item: &mut ContextItem, access: ContextAccess) -> bool {
    if access == ContextAccess::Editable && item.access != ContextAccess::Editable {
        item.access = ContextAccess::Editable;
        true
    } else {
        false
    }
}

/// Reduces a path a caller may hold to the logical name the OuterDocs layer
/// stores.
///
/// The layer addresses a document by its logical path, while a research round
/// that just materialized one still holds the cache path its snapshot was
/// written to. Both spellings name the same document, so a lookup resolves
/// either of them instead of reporting a second entry.
fn document_key(path: &str) -> &str {
    crate::outer_docs::logical_path(path).unwrap_or(path)
}

/// Diagnostic reported when a caller tries to fill a registry-managed layer.
///
/// The OuterDocs layer is filled exclusively by the external documentation
/// registry, so a manual admission is refused by contract wherever it is
/// attempted.
const OUTER_DOCS_MANUAL_ADD: &str =
    "The OuterDocs layer is registry-managed and cannot be filled manually";

/// Refreshes the recorded metadata of `item` from the file at `full_path`.
///
/// The helper reports whether the recorded size actually moved, so a caller
/// marks the context as changed only when a file really did. A path that cannot
/// be stat'ed keeps the metadata the entry already carries.
fn refresh_metadata(item: &mut ContextItem, full_path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(full_path) else {
        return false;
    };
    let size_bytes = metadata.len();
    let estimated_tokens = estimate_tokens_from_bytes(size_bytes);
    if item.size_bytes == size_bytes && item.estimated_tokens == estimated_tokens {
        return false;
    }
    item.size_bytes = size_bytes;
    item.estimated_tokens = estimated_tokens;
    true
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
            outer_docs: IndexMap::new(),
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
            outer_docs: IndexMap::new(),
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
        // An external document is read-only by construction, so it answers the
        // same way the read-only entries of the other three layers do. The
        // layer stores the logical path of a document while a research round
        // still holds the cache path its snapshot was written to, so both
        // spellings resolve to the same document.
        if found || self.outer_docs.contains_key(document_key(clean_path)) {
            Some(ContextAccess::ReadOnly)
        } else {
            None
        }
    }

    pub fn get_state(&self) -> ContextState {
        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut total_estimated_tokens = 0u64;

        // Emit every context path exactly once, in pinned -> outer_docs -> user
        // -> auto order. A file present in the pinned layer must never be
        // duplicated when it also lives in another layer (for example an
        // editable override), so the first owning layer wins and the merged
        // effective access is applied to the single emitted item.
        let ordered = self
            .pinned
            .iter()
            .chain(self.outer_docs.iter())
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
    /// Files are emitted in layer order (pinned -> outer_docs -> user -> auto)
    /// and in each layer's insertion order, and each file carries the watermark
    /// its context entry already holds.
    fn read_context_files_with_seq(&self) -> Vec<ContextFileContent> {
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
                .chain(self.outer_docs.keys())
                .chain(self.user.keys())
                .chain(self.auto.keys())
            {
                if !seen.insert(path.as_str()) {
                    continue;
                }
                // The OuterDocs layer names a logical document whose bytes live
                // in the isolated cache; every other layer names a repository
                // file, which passes the escape boundary before it is read.
                let full_path = if self.outer_docs.contains_key(path.as_str()) {
                    self.outer_docs_cache_path(path)
                } else {
                    if verify_path_within_repo(&self.repo_root, path).is_err() {
                        continue;
                    }
                    self.repo_root.join(path)
                };
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    let access = self
                        .effective_access_for(path)
                        .unwrap_or(ContextAccess::ReadOnly);
                    let insertion_seq = self.existing_insertion_seq(path).unwrap_or(0);
                    files.push(ContextFileContent {
                        path: path.clone(),
                        access,
                        content,
                        insertion_seq,
                    });
                }
            }
            files
        })
    }

    /// Reads the context files in semantic order: layer order (pinned ->
    /// outer_docs -> user -> auto) and insertion order within each layer.
    pub fn read_context_files(&self) -> Vec<ContextFileContent> {
        self.read_context_files_with_seq()
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
        files.sort_by_key(|file| file.insertion_seq);
        files
    }

    pub fn contains(&self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        Ok(self.pinned.contains_key(&clean_path)
            || self.outer_docs.contains_key(document_key(&clean_path))
            || self.user.contains_key(&clean_path)
            || self.auto.contains_key(&clean_path))
    }

    pub fn is_editable(&self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        Ok(self.effective_access_for(&clean_path) == Some(ContextAccess::Editable))
    }

    /// Returns all paths currently tracked in the OuterDocs context layer.
    pub fn outer_docs_files(&self) -> Vec<String> {
        self.outer_docs.keys().cloned().collect()
    }

    /// Filesystem location of one document of the OuterDocs layer.
    ///
    /// The layer keeps the logical path of a document — the declared identifier
    /// and the relative path below it — while its bytes live in the isolated
    /// documentation cache. Every disk access of the layer therefore resolves
    /// the document through this helper, so the cache prefix stays inside the
    /// harness and never reaches a rendered context element.
    fn outer_docs_cache_path(&self, clean_path: &str) -> PathBuf {
        self.repo_root
            .join(crate::outer_docs::OUTER_DOCS_CACHE_DIR)
            .join(clean_path)
    }

    /// Admits one external documentation snapshot into the OuterDocs layer.
    ///
    /// The layer stores the logical path of the document and never the cache
    /// path its snapshot is written to, so the `<file path="...">` element, the
    /// prompt manifest, and the interface all address one document by one name.
    /// A caller may pass either spelling: a cache path is reduced to the
    /// logical one the layer keeps.
    ///
    /// The document is stored read-only, because it is a snapshot the harness
    /// materialized and the model must never rewrite it. A repeated request is
    /// a no-op: the snapshot is immutable for the whole task, so no rendered
    /// byte changes and the context revision stays put.
    ///
    /// The layer applies no budget of its own: a snapshot is priced exactly like
    /// a repository file and admitted under the single `[context].token_budget`
    /// ceiling by the research round that requested it. A refusal therefore
    /// never deletes a materialized snapshot, and a later round can read it
    /// again.
    pub fn add_outer_docs_file(&mut self, relative_path: &str) -> Result<ContextItem> {
        crate::blocking::run_blocking(|| {
            let clean_path = self.normalize_path(document_key(relative_path))?;
            let full_path = self.outer_docs_cache_path(&clean_path);
            if !full_path.is_file() {
                bail!("External document not found: {}", clean_path);
            }

            if let Some(snapshot) = self.outer_docs.get(&clean_path) {
                return Ok(snapshot.clone());
            }

            let metadata = std::fs::metadata(&full_path)?;
            let size_bytes = metadata.len();
            let estimated_tokens = estimate_tokens_from_bytes(size_bytes);

            let item = ContextItem {
                path: clean_path.clone(),
                access: ContextAccess::ReadOnly,
                size_bytes,
                estimated_tokens,
                layer: ContextLayer::OuterDocs,
                insertion_seq: self.resolve_insertion_seq(&clean_path),
            };
            self.outer_docs.insert(clean_path, item.clone());
            self.revision += 1;
            Ok(item)
        })
    }

    /// Drops every document of the OuterDocs context layer.
    ///
    /// Returns `true` only when the layer actually held documents, so callers can
    /// rotate the dialogue session exactly when the stable prompt prefix shrank.
    pub fn clear_outer_docs(&mut self) -> bool {
        if self.outer_docs.is_empty() {
            return false;
        }
        self.outer_docs.clear();
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
        for layer in [&self.pinned, &self.user, &self.auto, &self.outer_docs] {
            if let Some(item) = layer.get(clean_path) {
                let seq = item.insertion_seq;
                watermark = Some(watermark.map_or(seq, |current: u64| current.max(seq)));
            }
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
        // The OuterDocs layer belongs to the external documentation registry: a
        // document enters it only through `add_outer_docs_file`, so a manual
        // admission is refused by contract. The check precedes the repository
        // file probe, which would otherwise report an unrelated cause: a
        // logical document lives in the cache and never in the working tree.
        if layer == ContextLayer::OuterDocs {
            bail!(OUTER_DOCS_MANUAL_ADD);
        }
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
            // Admitting a path as Editable is a write admission: an edit may
            // later write it. The strict check refuses the admission while the
            // escape boundary cannot be inspected, so a degraded repository
            // root never widens what the change set may touch.
            if access == ContextAccess::Editable {
                verify_path_within_repo_for_write(&self.repo_root, &clean_path)?;
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
                // The OuterDocs layer belongs to the external documentation
                // registry: a document enters it only through
                // `add_outer_docs_file`, so a manual add is refused rather than
                // stored.
                // The registry-managed layer is refused at the entry of this
                // function; the arm keeps the match exhaustive over the layers.
                ContextLayer::OuterDocs => bail!(OUTER_DOCS_MANUAL_ADD),
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
    /// Drops every file of the transient context: the auto layer and the
    /// external documents materialized by the registry.
    ///
    /// The transient context is whatever the harness gathered so the model
    /// could decide, so both of its layers are released at exactly the same
    /// boundaries. Returns `true` only when at least one of them actually held
    /// files, so a cleanup that removed nothing is distinguishable from one
    /// that shrank the stable prefix of the prompt.
    pub fn clear_transient(&mut self) -> bool {
        if self.auto.is_empty() && self.outer_docs.is_empty() {
            return false;
        }
        self.auto.clear();
        self.outer_docs.clear();
        self.recompute_pinned();
        self.revision += 1;
        true
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
    /// Skips files already present in the user layer. The aggregate token budget
    /// is the only measure of how much one call may add: a candidate is priced as
    /// its content plus the wrapper its `<file>` element renders, priced by the
    /// same builder that renders the `<context>` block. Files are admitted in
    /// sorted order until the budget is exhausted, and the outcome then reports
    /// `truncated = true` so the caller can surface the omission instead of
    /// silently dropping files.
    pub fn add_files_by_pattern(
        &mut self,
        pattern: &str,
        access: ContextAccess,
        available_files: &[String],
        token_budget: usize,
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

        // What the loaded files already spend is measured first with the same
        // wrapper-aware rule. A matched path is excluded here because the loop
        // below prices it, whether it arrives fresh or is promoted out of
        // another layer. The set keeps that exclusion linear instead of
        // rescanning every matched path once per loaded item.
        let mut spent_tokens: usize = {
            let matched_paths: std::collections::HashSet<&str> =
                matched_files.iter().map(String::as_str).collect();
            self.get_state()
                .items
                .iter()
                .filter(|item| !matched_paths.contains(item.path.as_str()))
                .map(|item| {
                    item.estimated_tokens as usize
                        + crate::prompt::context_element_overhead_tokens(&item.path, item.access)
                })
                .sum()
        };

        // Every candidate is priced from its own metadata before admission, so
        // the pass touches the filesystem. It runs off the async executor
        // thread, and the helper is reentrant: the offload each admission
        // performs internally runs inline here instead of stacking a second
        // hand-off.
        crate::blocking::run_blocking(|| {
            let mut added_items = Vec::new();
            let mut total_added_tokens = 0u64;
            let mut any_changed = false;
            let mut truncated = false;

            for path in matched_files {
                // The candidate is priced from its metadata before admission,
                // so the wrapper of its own path is charged to the budget it
                // spends.
                let cost = match std::fs::metadata(self.repo_root.join(&path)) {
                    Ok(metadata) => {
                        estimate_tokens_from_bytes(metadata.len()) as usize
                            + crate::prompt::context_element_overhead_tokens(&path, access)
                    }
                    // A path that vanished between the listing and the read
                    // adds nothing, and it is not a budget omission.
                    Err(_) => continue,
                };
                if spent_tokens.saturating_add(cost) > token_budget {
                    truncated = true;
                    break;
                }
                let rev_before = self.revision;
                if let Ok(item) =
                    self.add_file_to_layer_internal(&path, access, ContextLayer::User, false)
                {
                    spent_tokens += cost;
                    total_added_tokens += cost as u64;
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
        })
    }
    pub fn update_file_metadata(&mut self, relative_path: &str) -> Result<()> {
        // The metadata read is blocking filesystem work, so it runs off the
        // async executor thread.
        crate::blocking::run_blocking(|| {
            let clean_path = self.normalize_path(relative_path)?;
            let full_path = self.repo_root.join(&clean_path);
            // The OuterDocs layer holds the logical path of a document, so the
            // snapshot it renders is measured in the cache that keeps its bytes.
            let document_path = self.outer_docs_cache_path(&clean_path);
            let mut updated = false;
            for layer in [&mut self.pinned, &mut self.user, &mut self.auto] {
                if let Some(item) = layer.get_mut(&clean_path) {
                    updated |= refresh_metadata(item, &full_path);
                }
            }
            if let Some(item) = self.outer_docs.get_mut(&clean_path) {
                updated |= refresh_metadata(item, &document_path);
            }
            if updated {
                self.revision += 1;
            }
            Ok(())
        })
    }

    /// Removes a path from the mutable context layers.
    ///
    /// The `user`, `auto`, and `outer_docs` layers are emptied here. Pinned
    /// files are owned by project configuration and are deliberately left in
    /// place, so removing a pinned-only path reports `Ok(false)` (no change)
    /// instead of silently succeeding.
    pub fn remove_file(&mut self, relative_path: &str) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        let removed_user = self.user.shift_remove(&clean_path).is_some();
        let removed_auto = self.auto.shift_remove(&clean_path).is_some();
        // An external document leaves the layer on an explicit removal, because
        // dropping one document is a deliberate developer action; the registry
        // only withholds access and layer changes.
        let removed_outer_docs = self
            .outer_docs
            .shift_remove(document_key(&clean_path))
            .is_some();
        if removed_user || removed_auto || removed_outer_docs {
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
            } else if self.outer_docs.contains_key(&clean_from) {
                bail!(
                    "External document '{}' cannot be renamed or moved",
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
                    // Neither layer can reach this point: pinned and external
                    // documents are rejected before the rename begins.
                    ContextLayer::Pinned | ContextLayer::OuterDocs => {}
                }
                self.recompute_pinned();
                self.revision += 1;
            }

            Ok(())
        })
    }

    pub fn set_access(&mut self, relative_path: &str, access: ContextAccess) -> Result<bool> {
        let clean_path = self.normalize_path(relative_path)?;
        // Promoting a path to Editable is a write admission: the file may be
        // written afterwards, so the escape boundary must be inspectable.
        if access == ContextAccess::Editable {
            verify_path_within_repo_for_write(&self.repo_root, &clean_path)?;
        }
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

        if self.outer_docs.contains_key(&clean_path) {
            bail!(
                "External document '{}' is read-only and its access cannot be changed",
                clean_path
            );
        }
        if self.pinned.contains_key(&clean_path) {
            bail!("Pinned file '{}' access cannot be changed", clean_path);
        }

        Ok(false)
    }

    /// Drops every file of the mutable user, auto, and outer_docs context layers.
    ///
    /// Returns `true` only when at least one layer actually held files, so a
    /// clear that removed nothing is distinguishable from a clear that shrank
    /// the prompt prefix.
    pub fn clear(&mut self) -> bool {
        let had_items =
            !self.user.is_empty() || !self.auto.is_empty() || !self.outer_docs.is_empty();
        if had_items {
            self.user.clear();
            self.auto.clear();
            self.outer_docs.clear();
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
            // A document is stored under its logical path while its snapshot
            // lives in the isolated cache, so the existence check reads the
            // cache the layer keeps and never the working tree.
            let cache_root = self.repo_root.join(crate::outer_docs::OUTER_DOCS_CACHE_DIR);
            let stale_outer_docs: Vec<String> = self
                .outer_docs
                .keys()
                .filter(|path| !cache_root.join(path.as_str()).is_file())
                .cloned()
                .collect();
            for path in &stale_outer_docs {
                self.outer_docs.shift_remove(path);
            }
            removed.extend(stale_outer_docs);

            let pinned_changed = self.recompute_pinned();

            if !removed.is_empty() || pinned_changed {
                self.revision += 1;
            }

            removed
        })
    }

    /// Merges updates produced by a workflow run into the current context manager.
    /// Preserves user layer changes made concurrently. Updates the auto layer,
    /// merges the external documentation layer symmetrically, syncs file
    /// renames/removals, and refreshes metadata.
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

            // 1b. Merge the turn's external documents. The layer is part of the
            // transient context, so a document the turn materialized waits in
            // the live layer until that context is released. A snapshot is
            // immutable for the whole task, so a path present on both sides
            // keeps the item already held instead of being replaced.
            for (path, item) in &turn_context.outer_docs {
                if !self.outer_docs.contains_key(path) {
                    // The path may have entered another layer while the turn
                    // ran, so it keeps the higher of the two watermarks.
                    let mut merged = item.clone();
                    merged.insertion_seq = self
                        .existing_insertion_seq(path)
                        .map_or(item.insertion_seq, |seq| seq.max(item.insertion_seq));
                    self.outer_docs.insert(path.clone(), merged);
                    changed = true;
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

            // 4. Refresh metadata for all files in context, the external
            // documentation snapshots the turn materialized included. The
            // OuterDocs layer holds the logical path of a document, so its
            // snapshot is measured in the cache that keeps its bytes.
            for layer in [&mut self.pinned, &mut self.user, &mut self.auto] {
                for (path, item) in layer.iter_mut() {
                    changed |= refresh_metadata(item, &self.repo_root.join(path.as_str()));
                }
            }
            let cache_root = self.repo_root.join(crate::outer_docs::OUTER_DOCS_CACHE_DIR);
            for (path, item) in self.outer_docs.iter_mut() {
                changed |= refresh_metadata(item, &cache_root.join(path.as_str()));
            }

            // The turn may have consumed watermarks the primary never saw, so
            // the counter advances to the highest value either side reached and
            // a concurrent addition can never reuse a watermark the turn already
            // handed out. Advancing the counter alone changes no bytes, so it
            // does not mark the merge as a change.
            self.insertion_counter = self.insertion_counter.max(turn_context.insertion_counter);

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

    /// Writes one external document into the isolated cache a declaration
    /// materializes into.
    fn cache_document(root: &std::path::Path, logical: &str, contents: &str) {
        let path = root
            .join(crate::outer_docs::OUTER_DOCS_CACHE_DIR)
            .join(logical);
        std::fs::create_dir_all(path.parent().expect("the cache path has a directory"))
            .expect("the cache directory is created");
        std::fs::write(&path, contents).expect("the snapshot is written");
    }

    /// Repository-relative path the cache stores `logical` under.
    fn cache_path(logical: &str) -> String {
        format!("{}/{logical}", crate::outer_docs::OUTER_DOCS_CACHE_DIR)
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
            .add_files_by_pattern("src/*.rs", ContextAccess::ReadOnly, &repo_files, 300_000)
            .unwrap();

        assert_eq!(outcome.added_items.len(), 2);
        assert!(outcome.added_tokens > 0);
        assert!(!outcome.truncated);
        assert!(cm.contains("src/lib.rs").unwrap());
        assert!(cm.contains("src/main.rs").unwrap());
        assert!(!cm.contains("docs/Readme.md").unwrap());
    }

    #[test]
    fn test_add_files_by_pattern_reports_a_budget_truncation() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn a() {}").unwrap();
        std::fs::write(root.join("src/b.rs"), "pub fn b() {}").unwrap();

        let repo_files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];

        // The file token budget is the only measure: it pays for one file's
        // content together with the wrapper its `<file>` element renders, so
        // exactly one of the two matches is admitted.
        let budget = crate::prompt::estimate_tokens_from_bytes("pub fn a() {}".len() as u64)
            as usize
            + crate::prompt::context_element_overhead_tokens("src/a.rs", ContextAccess::ReadOnly);

        let mut cm = ContextManager::new(root);
        let outcome = cm
            .add_files_by_pattern("src/*.rs", ContextAccess::ReadOnly, &repo_files, budget)
            .unwrap();

        assert_eq!(outcome.added_items.len(), 1);
        assert!(outcome.truncated);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_add_files_by_pattern_prices_candidates_on_an_async_runtime() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn a() {}").unwrap();
        std::fs::write(root.join("src/b.rs"), "pub fn b() {}").unwrap();

        let repo_files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let mut cm = ContextManager::new(root);

        // The pricing walk reads metadata for every candidate, so the whole
        // pass is offloaded: it must complete from a multi-threaded executor
        // exactly as it does from synchronous code.
        let outcome = cm
            .add_files_by_pattern("src/*.rs", ContextAccess::Editable, &repo_files, 300_000)
            .unwrap();

        assert_eq!(outcome.added_items.len(), 2);
        assert!(!outcome.truncated);
        assert!(outcome.added_tokens > 0);
        assert!(cm.is_editable("src/a.rs").unwrap());
        assert!(cm.is_editable("src/b.rs").unwrap());
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
        // Every file carries the watermark of the entry it renders, so a
        // chronological reader never re-derives the arrival order.
        assert!(files.iter().all(|file| file.insertion_seq > 0));
        assert!(files
            .windows(2)
            .all(|pair| pair[0].insertion_seq < pair[1].insertion_seq));
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

        // Both readers carry the watermark of the entry they render, so the
        // chronology is data on the file instead of a second computation.
        let carried: Vec<(String, u64)> = cm
            .read_context_files_by_insertion()
            .into_iter()
            .map(|file| (file.path, file.insertion_seq))
            .collect();
        assert_eq!(
            carried,
            vec![
                ("pinned.rs".to_string(), watermark_of(&cm, "pinned.rs")),
                ("auto.rs".to_string(), watermark_of(&cm, "auto.rs")),
                ("user.rs".to_string(), watermark_of(&cm, "user.rs")),
            ]
        );
        // The semantic reader serves the very same values in layer order.
        let semantic_watermarks: Vec<u64> = cm
            .read_context_files()
            .into_iter()
            .map(|file| file.insertion_seq)
            .collect();
        assert_eq!(
            semantic_watermarks,
            vec![carried[0].1, carried[2].1, carried[1].1]
        );
    }

    #[test]
    fn test_outer_docs_layer_emits_read_only_documents_in_layer_order() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for name in ["pinned.rs", "user.rs", "auto.rs"] {
            std::fs::write(root.join(name), "fn f() {}\n").unwrap();
        }
        cache_document(&root, "docs.md", "external notes\n");

        let mut cm = ContextManager::new(root);
        cm.load_pinned(&["pinned.rs".to_string()]);
        let document = cm.add_outer_docs_file("docs.md").unwrap();
        cm.add_file("user.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();

        assert_eq!(document.layer, ContextLayer::OuterDocs);
        assert_eq!(document.access, ContextAccess::ReadOnly);
        // The layer addresses the document by its logical path, never by the
        // cache path its snapshot is stored under.
        assert_eq!(document.path, "docs.md");
        assert!(!cm.is_editable("docs.md").unwrap());
        assert_eq!(cm.outer_docs_files(), vec!["docs.md".to_string()]);

        let state = cm.get_state();
        let paths: Vec<&str> = state.items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["pinned.rs", "docs.md", "user.rs", "auto.rs"]);
        let emitted = state
            .items
            .iter()
            .find(|item| item.path == "docs.md")
            .unwrap();
        assert_eq!(emitted.layer, ContextLayer::OuterDocs);
        assert_eq!(emitted.access, ContextAccess::ReadOnly);

        // The bytes travel from the cache the layer keeps them in, while the
        // rendered element names the logical path.
        let files = cm.read_context_files();
        let file_paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            file_paths,
            vec!["pinned.rs", "docs.md", "user.rs", "auto.rs"]
        );
        let rendered = files.iter().find(|file| file.path == "docs.md").unwrap();
        assert_eq!(rendered.content, "external notes\n");
    }

    #[test]
    fn test_a_repeated_outer_docs_request_is_a_no_op() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        cache_document(&root, "docs.md", "external notes\n");

        let mut cm = ContextManager::new(root);
        cm.add_outer_docs_file("docs.md").unwrap();
        let revision = cm.get_state().revision;

        // A repeated request returns the same immutable snapshot: nothing
        // rendered changes and the revision stays put.
        let again = cm.add_outer_docs_file("docs.md").unwrap();
        assert_eq!(again.layer, ContextLayer::OuterDocs);
        assert_eq!(cm.get_state().revision, revision);
        assert_eq!(cm.outer_docs_files(), vec!["docs.md".to_string()]);

        // The cache path and the logical path name the same document, so a
        // caller still holding the cache spelling reaches the same snapshot.
        let cached = cache_path("docs.md");
        assert_eq!(cm.add_outer_docs_file(&cached).unwrap(), again);
        assert_eq!(cm.outer_docs_files(), vec!["docs.md".to_string()]);
        assert_eq!(cm.get_state().revision, revision);
    }

    #[test]
    fn test_the_outer_docs_layer_keeps_every_admitted_document() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for name in ["a.md", "b.md", "c.md"] {
            cache_document(&root, name, "external notes\n");
        }

        let mut cm = ContextManager::new(root);
        cm.add_outer_docs_file("a.md").unwrap();
        cm.add_outer_docs_file("b.md").unwrap();
        // A repeated request is a no-op and keeps the snapshot in place.
        cm.add_outer_docs_file("a.md").unwrap();

        // The layer keeps no eviction order: admitting the third document
        // leaves the first two resident.
        cm.add_outer_docs_file("c.md").unwrap();

        assert_eq!(
            cm.outer_docs_files(),
            vec!["a.md".to_string(), "b.md".to_string(), "c.md".to_string()]
        );
    }

    /// The layer itself applies no budget: a snapshot is priced by the research
    /// round that requested it, so a document the aggregate ceiling refuses is
    /// still admitted here and a later round can read it again.
    #[test]
    fn test_the_outer_docs_layer_applies_no_budget_of_its_own() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        cache_document(&root, "huge.md", &"x".repeat(4096));

        let mut cm = ContextManager::new(root);
        let document = cm
            .add_outer_docs_file("huge.md")
            .expect("the layer prices the document but never refuses it");

        assert_eq!(document.layer, ContextLayer::OuterDocs);
        assert!(document.estimated_tokens > 0);
        assert!(cm.outer_docs_files().contains(&"huge.md".to_string()));
    }

    #[test]
    fn test_the_outer_docs_layer_survives_clear_auto_and_a_turn_merge() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        cache_document(&root, "base.md", "base external notes\n");
        cache_document(&root, "turn.md", "turn external notes\n");
        std::fs::write(root.join("auto.rs"), "fn auto() {}\n").unwrap();

        let mut primary = ContextManager::new(root);
        primary.add_outer_docs_file("base.md").unwrap();
        primary
            .add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();
        let mut turn = primary.clone();
        turn.add_outer_docs_file("turn.md").unwrap();

        primary.merge_turn_context(&turn);
        assert_eq!(
            primary.outer_docs_files(),
            vec!["base.md".to_string(), "turn.md".to_string()]
        );

        // Both layers are transient. Clearing the Auto layer on its own is an
        // explicit per-layer action, so it keeps the external documents the same
        // task still needs; releasing the transient context as a whole is what
        // drops them.
        primary.clear_auto();
        assert_eq!(
            primary.outer_docs_files(),
            vec!["base.md".to_string(), "turn.md".to_string()]
        );

        assert!(primary.clear_outer_docs());
        assert!(primary.outer_docs_files().is_empty());
    }

    #[test]
    fn test_set_access_on_an_outer_docs_document_is_rejected() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        cache_document(&root, "docs.md", "external notes\n");

        let mut cm = ContextManager::new(root);
        cm.add_outer_docs_file("docs.md").unwrap();

        assert!(cm.set_access("docs.md", ContextAccess::Editable).is_err());
        assert!(!cm.is_editable("docs.md").unwrap());
    }

    #[test]
    fn test_the_outer_docs_layer_cannot_be_filled_manually() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        cache_document(&root, "docs.md", "external notes\n");

        let mut cm = ContextManager::new(root);
        let error = cm
            .add_file_to_layer("docs.md", ContextAccess::ReadOnly, ContextLayer::OuterDocs)
            .expect_err("the registry owns the OuterDocs layer");

        assert!(
            error.to_string().contains("registry"),
            "unexpected error: {error}"
        );
        assert!(cm.outer_docs_files().is_empty());
    }

    #[test]
    fn test_clear_outer_docs_reports_whether_documents_were_removed() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        cache_document(&root, "docs.md", "external notes\n");

        let mut cm = ContextManager::new(root);
        assert!(
            !cm.clear_outer_docs(),
            "an empty OuterDocs layer reports no change"
        );
        cm.add_outer_docs_file("docs.md").unwrap();
        assert!(
            cm.clear_outer_docs(),
            "removing a document reports a change"
        );
        assert!(!cm.clear_outer_docs(), "a repeated clear reports no change");
        assert!(cm.outer_docs_files().is_empty());
    }

    #[test]
    fn test_clear_transient_removes_auto_and_outer_docs_only() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for name in ["pinned.rs", "user.rs", "auto.rs"] {
            std::fs::write(root.join(name), "fn f() {}\n").unwrap();
        }
        cache_document(&root, "docs.md", "external notes\n");

        let mut cm = ContextManager::new(root);
        cm.load_pinned(&["pinned.rs".to_string()]);
        cm.add_file("user.rs", ContextAccess::Editable).unwrap();
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();
        cm.add_outer_docs_file("docs.md").unwrap();
        let rev_before = cm.get_state().revision;

        assert!(cm.clear_transient());

        assert!(cm.auto_files().is_empty());
        assert!(cm.outer_docs_files().is_empty());
        assert!(!cm.contains("auto.rs").unwrap());
        assert!(!cm.contains("docs.md").unwrap());
        // Pinned and user entries are not transient and stay in place.
        assert!(cm.contains("pinned.rs").unwrap());
        assert!(cm.contains("user.rs").unwrap());
        assert!(cm.is_editable("user.rs").unwrap());
        assert!(cm.get_state().revision > rev_before);
    }

    #[test]
    fn test_clear_transient_reports_whether_anything_was_removed() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("auto.rs"), "fn auto() {}\n").unwrap();
        cache_document(&root, "docs.md", "external notes\n");

        let mut cm = ContextManager::new(root);
        assert!(
            !cm.clear_transient(),
            "clearing an empty transient context must report no change"
        );

        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();
        assert!(
            cm.clear_transient(),
            "removing an auto file must report a change"
        );

        cm.add_outer_docs_file("docs.md").unwrap();
        assert!(
            cm.clear_transient(),
            "removing an external document must report a change"
        );
        assert!(
            !cm.clear_transient(),
            "a repeated clear on an empty transient context must report no change"
        );
    }

    #[test]
    fn test_clear_transient_recomputes_hierarchical_pinned_files() {
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
        cm.add_auto_file("crates/core/src/lib.rs", ContextAccess::ReadOnly)
            .unwrap();
        assert!(cm.contains("crates/core/Conventions.md").unwrap());

        cm.clear_transient();

        // The hierarchical pinned entry arrived with the transient file, so it
        // leaves together with it, while the root-level pinned file stays.
        assert!(cm.contains("Conventions.md").unwrap());
        assert!(!cm.contains("crates/core/Conventions.md").unwrap());
    }
    #[test]
    fn test_a_repository_file_is_not_an_external_document() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("docs.md"), "fn tracked() {}\n").unwrap();

        let mut cm = ContextManager::new(root);
        let error = cm
            .add_outer_docs_file("docs.md")
            .expect_err("only a cached snapshot is an external document");

        assert!(
            error.to_string().contains("not found"),
            "unexpected error: {error}"
        );
        assert!(cm.outer_docs_files().is_empty());
    }

    #[test]
    fn test_prune_missing_files_drops_a_document_whose_snapshot_disappeared() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        cache_document(&root, "docs.md", "external notes\n");
        // A repository file carrying the very same logical name must not keep a
        // document whose snapshot is gone: the layer reads the cache.
        std::fs::write(root.join("docs.md"), "fn tracked() {}\n").unwrap();

        let mut cm = ContextManager::new(root.clone());
        cm.add_file("docs.md", ContextAccess::ReadOnly).unwrap();
        cm.add_outer_docs_file("docs.md").unwrap();

        std::fs::remove_file(
            root.join(crate::outer_docs::OUTER_DOCS_CACHE_DIR)
                .join("docs.md"),
        )
        .unwrap();

        let pruned = cm.prune_missing_files();

        assert_eq!(pruned, vec!["docs.md".to_string()]);
        assert!(cm.outer_docs_files().is_empty());
        assert!(
            cm.contains("docs.md").unwrap(),
            "the repository file keeps its own entry"
        );
    }

    fn outer_docs_tokens(cm: &ContextManager) -> u64 {
        cm.get_state()
            .items
            .into_iter()
            .filter(|item| item.layer == ContextLayer::OuterDocs)
            .map(|item| item.estimated_tokens)
            .sum()
    }

    #[test]
    fn test_merge_turn_context_keeps_every_outer_docs_document() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for name in ["a.md", "b.md"] {
            cache_document(&root, name, "external notes\n");
        }

        let mut primary = ContextManager::new(root);
        primary.add_outer_docs_file("a.md").unwrap();

        // The turn materializes one more document beside the one the primary
        // already holds.
        let mut turn = primary.clone();
        turn.add_outer_docs_file("b.md").unwrap();

        primary.merge_turn_context(&turn);

        // The layer keeps no eviction order, so the merge takes the union of
        // both sides instead of dropping a document by recency.
        assert_eq!(
            primary.outer_docs_files(),
            vec!["a.md".to_string(), "b.md".to_string()]
        );
        assert!(outer_docs_tokens(&primary) > 0);
    }
}
