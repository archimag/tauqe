//! Per-turn memoization of deterministic proposal inputs.
//!
//! The tracked-file list and the generated repo map are pure functions of the
//! repository HEAD and the active context. Consecutive rounds that leave both
//! unchanged (typical of patch retries) reuse the cached values instead of
//! re-running `git ls-files` and a full Tree-sitter pass over the repository on
//! every round. The retained extractor additionally reuses per-file symbols
//! across regenerations within the turn.

use std::path::Path;

use crate::repomap::TreeSitterExtractor;

/// Per-turn memoization of deterministic proposal inputs.
#[derive(Default)]
pub(super) struct TurnInputCache {
    files: Option<TrackedFiles>,
    repo_map: Option<RepoMapCacheEntry>,
    extractor: TreeSitterExtractor,
}

/// Tracked-file list memoized at the repository HEAD it was read from.
struct TrackedFiles {
    head: String,
    files: Vec<String>,
}

/// Repo map memoized against the inputs that determine it.
///
/// The map excludes every file in context, so the cached key stores the exact
/// set of targetable paths alongside the repository HEAD and the token budget.
struct RepoMapCacheEntry {
    head: String,
    targetable_paths: Vec<String>,
    token_budget: usize,
    map: Option<String>,
}

impl TurnInputCache {
    /// Returns the repository's tracked file list, re-reading it only when the
    /// repository HEAD has moved since the previous read.
    async fn tracked_files(&mut self, repo_root: &Path, head: &str) -> Vec<String> {
        let stale = self
            .files
            .as_ref()
            .map(|cached| cached.head != head)
            .unwrap_or(true);
        if stale {
            let root = repo_root.to_path_buf();
            let files = crate::blocking::run_blocking_async(move || {
                crate::git::list_repository_files(Some(&root)).unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            self.files = Some(TrackedFiles {
                head: head.to_string(),
                files,
            });
        }
        self.files
            .as_ref()
            .map(|cached| cached.files.clone())
            .unwrap_or_default()
    }

    /// Returns the repo map for the given inputs, regenerating it only when the
    /// HEAD, the targetable path set, or the token budget has changed.
    pub(super) async fn repo_map(
        &mut self,
        repo_root: &Path,
        head: &str,
        targetable_paths: &[String],
        token_budget: usize,
    ) -> Option<String> {
        let cache_hit = self
            .repo_map
            .as_ref()
            .map(|entry| {
                entry.head == head
                    && entry.token_budget == token_budget
                    && entry.targetable_paths == targetable_paths
            })
            .unwrap_or(false);
        if cache_hit {
            return self.repo_map.as_ref().and_then(|entry| entry.map.clone());
        }

        let available_files = self.tracked_files(repo_root, head).await;
        let root = repo_root.to_path_buf();
        let targetable = targetable_paths.to_vec();
        let extractor = self.extractor.clone();
        let map = crate::blocking::run_blocking_async(move || {
            crate::repomap::generate_repo_map_with_extractor(
                &root,
                &available_files,
                &targetable,
                token_budget,
                &extractor,
            )
            .ok()
            .filter(|map| !map.trim().is_empty())
        })
        .await
        .ok()
        .flatten();

        self.repo_map = Some(RepoMapCacheEntry {
            head: head.to_string(),
            targetable_paths: targetable_paths.to_vec(),
            token_budget,
            map: map.clone(),
        });
        map
    }
}
