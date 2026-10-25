//! Per-turn memoization of deterministic proposal inputs.
//!
//! The repository map is the heaviest deterministic input of a proposal round:
//! a full Tree-sitter pass over every tracked file outside the active context.
//! It is generated once, on the first proposal round of a bounded turn, and
//! frozen for the rest of that turn. Patch retries, verification healing, and
//! discovery rounds that widened the context all receive the identical map
//! string, so the repository map block of the prompt stays byte-identical and a
//! provider's prefix cache keeps matching its tokens across rounds.

use std::path::Path;
use tauqe_protocol::TurnIntent;

use crate::history::HistoryManager;

/// Per-turn memoization of deterministic proposal inputs.
#[derive(Default)]
pub(super) struct TurnInputCache {
    /// Repo map captured on the first proposal round of the turn.
    ///
    /// `Some` marks the map as generated, so an empty map is frozen as well
    /// instead of being recomputed on every round.
    repo_map: Option<FrozenRepoMap>,
    /// History block captured on the first proposal round of the turn.
    ///
    /// `Some` marks the history as resolved, so an absent or empty history
    /// block is frozen as well instead of re-reading history.jsonl on each round.
    history_tag: Option<FrozenHistoryTag>,
    /// Directory outline captured on the first proposal round of the turn.
    ///
    /// `Some` marks the outline as rendered, so an empty tree is frozen as well
    /// instead of re-walking the tracked file list on every round.
    repo_dirs: Option<FrozenRepoDirs>,
    /// Project configuration frozen on the first proposal round of the turn.
    project: Option<tauqe_protocol::ProjectConfig>,
}

/// Repository map frozen for the remaining rounds of a bounded turn.
struct FrozenRepoMap {
    map: Option<String>,
}

/// Session history tag frozen for the remaining rounds of a bounded turn.
struct FrozenHistoryTag {
    tag: Option<String>,
}

/// Compact directory outline frozen for the remaining rounds of a bounded turn.
struct FrozenRepoDirs {
    tree: Option<String>,
}

impl TurnInputCache {
    /// Returns the repository map frozen at the start of the turn.
    ///
    /// The first call generates the map from the tracked files, the paths
    /// already present in context, and the token budget, then freezes the
    /// result. Every later call returns that exact string, even when a discovery
    /// round added files to the context and thereby narrowed the set of paths the
    /// map describes: dropping those nodes mid-turn would shorten the map block
    /// and invalidate the prompt prefix the provider has already cached.
    pub(super) async fn repo_map(
        &mut self,
        repo_root: &Path,
        targetable_paths: &[String],
        token_budget: usize,
    ) -> Option<String> {
        if let Some(frozen) = &self.repo_map {
            return frozen.map.clone();
        }

        let root = repo_root.to_path_buf();
        let targetable = targetable_paths.to_vec();
        // The tracked-file listing and the Tree-sitter pass are blocking file
        // and subprocess work, so they run off the async executor thread. The
        // pass runs through the process-wide shared extractor, so files whose
        // modification state is unchanged since an earlier turn are served
        // from the persistent symbol cache instead of being re-parsed.
        let map = match crate::blocking::run_blocking_async(move || {
            let available_files = crate::git::list_repository_files(Some(&root));
            available_files.map(|files| {
                crate::repomap::generate_repo_map_with_extractor(
                    &root,
                    &files,
                    &targetable,
                    token_budget,
                    crate::repomap::treesitter::shared_extractor(),
                )
                .ok()
                .filter(|map| !map.trim().is_empty())
            })
        })
        .await
        {
            Ok(Ok(map)) => map,
            // A repository whose listing failed is not an empty repository: the
            // degradation is recorded before the map is treated as absent.
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    "failed to list repository files; the repository map is unavailable"
                );
                None
            }
            Err(join_error) => {
                crate::blocking::log_join_error(&join_error, "repository map generation");
                None
            }
        };

        self.repo_map = Some(FrozenRepoMap { map: map.clone() });
        map
    }

    /// Returns the session history block frozen at the start of the turn.
    ///
    /// An isolated step never injects history and freezes `None`. For regular
    /// turns, the first call reads and formats session history off the executor
    /// thread and freezes the result. Every subsequent round reuses that exact
    /// string without hitting disk or re-parsing JSONL records, ensuring that
    /// the session history block in the prompt prefix remains byte-identical.
    pub(super) async fn history_tag(
        &mut self,
        history_manager: &HistoryManager,
        intent: TurnIntent,
    ) -> Option<String> {
        if let Some(frozen) = &self.history_tag {
            return frozen.tag.clone();
        }

        if intent.is_isolated_step() {
            self.history_tag = Some(FrozenHistoryTag { tag: None });
            return None;
        }

        let hm = history_manager.clone();
        let tag = match crate::blocking::run_blocking_async(move || {
            hm.format_history_tag().map(|tag| {
                let trimmed = tag.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(tag)
                }
            })
        })
        .await
        {
            Ok(Ok(tag)) => tag,
            // A history that could not be read is not an empty history: the
            // degradation is recorded before the block is treated as absent.
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    "failed to format the session history; the turn proceeds without it"
                );
                None
            }
            Err(join_error) => {
                crate::blocking::log_join_error(&join_error, "session history formatting");
                None
            }
        };

        self.history_tag = Some(FrozenHistoryTag { tag: tag.clone() });
        tag
    }

    /// Returns the compact directory outline frozen at the start of the turn.
    ///
    /// The tree is derived only from the tracked file list, so it is rendered
    /// once and reused verbatim by every later round: the outline block stays
    /// byte-identical and the provider keeps matching its cached prefix. Files
    /// anchored by the pinned context layer are marked, so the model never
    /// searches for conventions that already travelled with the prompt.
    pub(super) async fn repo_dirs(
        &mut self,
        repo_root: &Path,
        pinned_patterns: &[String],
    ) -> Option<String> {
        if let Some(frozen) = &self.repo_dirs {
            return frozen.tree.clone();
        }

        let root = repo_root.to_path_buf();
        let pinned = pinned_patterns.to_vec();
        // Listing the tracked files is blocking subprocess work, so it runs off
        // the async executor thread.
        let tree = match crate::blocking::run_blocking_async(move || {
            crate::git::list_repository_files(Some(&root)).map(|available_files| {
                let tree =
                    crate::repomap::dirs::generate_repo_dirs_with_pinned(&available_files, &pinned);
                if tree.trim().is_empty() {
                    None
                } else {
                    Some(tree)
                }
            })
        })
        .await
        {
            Ok(Ok(tree)) => tree,
            // A repository whose listing failed is not an empty repository: the
            // degradation is recorded before the outline is treated as absent.
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    "failed to list repository files; the directory outline is unavailable"
                );
                None
            }
            Err(join_error) => {
                crate::blocking::log_join_error(&join_error, "directory outline rendering");
                None
            }
        };

        self.repo_dirs = Some(FrozenRepoDirs { tree: tree.clone() });
        tree
    }

    /// Returns the effective project configuration frozen at the start of the turn.
    pub(super) async fn project(
        &mut self,
        repo_root: &Path,
        cancel_rx: Option<&tokio::sync::watch::Receiver<bool>>,
    ) -> tauqe_protocol::ProjectConfig {
        if let Some(project) = &self.project {
            return project.clone();
        }

        let project = crate::toolchain::effective_project_config(repo_root, cancel_rx).await;
        self.project = Some(project.clone());
        project
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a git repository with two tracked Rust files so the tracked-file
    /// listing resolves them.
    fn repo_with_two_files() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&root), true).unwrap();
        std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(root.join("b.rs"), "pub fn beta() {}\n").unwrap();
        let files = vec!["a.rs".to_string(), "b.rs".to_string()];
        crate::git::cmd::stage_files(&root, &files).unwrap();
        crate::git::cmd::commit_with_developer_author(&root, "Add fixtures", false).unwrap();
        (dir, root)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repo_map_stays_identical_when_discovery_widens_the_context() {
        let (_dir, root) = repo_with_two_files();
        let mut cache = TurnInputCache::default();

        let opening_round: Vec<String> = Vec::new();
        let first = cache
            .repo_map(&root, &opening_round, 1000)
            .await
            .expect("the opening round generates the map");
        assert!(first.contains("b.rs"));
        assert!(first.contains("pub fn beta"));

        // A discovery round pulls `b.rs` into the context, which would exclude
        // it from a regenerated map. The frozen map keeps the node and stays
        // byte-identical, so the prompt prefix of the previous round survives.
        let later_round = vec!["b.rs".to_string()];
        let second = cache
            .repo_map(&root, &later_round, 1000)
            .await
            .expect("the frozen map is reused");
        assert_eq!(second, first);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_repo_map_is_frozen_instead_of_being_regenerated() {
        let (_dir, root) = repo_with_two_files();
        let mut cache = TurnInputCache::default();

        let everything = crate::git::list_repository_files(Some(&root)).unwrap();
        assert!(cache.repo_map(&root, &everything, 1000).await.is_none());

        // A later round that would produce a non-empty map still observes the
        // frozen empty result.
        let narrowed = vec!["a.rs".to_string()];
        assert!(cache.repo_map(&root, &narrowed, 1000).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_turn_generates_its_own_map() {
        let (_dir, root) = repo_with_two_files();

        let mut first_turn = TurnInputCache::default();
        let opening_round: Vec<String> = Vec::new();
        let before = first_turn
            .repo_map(&root, &opening_round, 1000)
            .await
            .expect("first turn map");
        assert!(before.contains("b.rs"));

        // A new turn starts with `b.rs` already in context and therefore gets a
        // map without that node: the freeze is scoped to a single turn.
        let mut second_turn = TurnInputCache::default();
        let with_b = vec!["b.rs".to_string()];
        let after = second_turn
            .repo_map(&root, &with_b, 1000)
            .await
            .expect("second turn map");
        assert!(!after.contains("b.rs"));
        assert!(after.contains("a.rs"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_tag_is_frozen_across_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut history_manager = HistoryManager::new(root);
        history_manager
            .record_turn("Original prompt", vec!["a.rs".to_string()])
            .unwrap();

        let mut cache = TurnInputCache::default();
        let first = cache
            .history_tag(&history_manager, TurnIntent::InteractiveRepl)
            .await
            .expect("first round resolves history");
        assert!(first.contains("Original prompt"));

        // Simulate an external mutation or subsequent event added during the turn
        history_manager
            .record_turn("Subsequent prompt", vec!["b.rs".to_string()])
            .unwrap();

        let second = cache
            .history_tag(&history_manager, TurnIntent::InteractiveRepl)
            .await
            .expect("second round reuses frozen history");
        assert_eq!(second, first);
        assert!(!second.contains("Subsequent prompt"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn directory_outline_is_frozen_across_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&root), true).unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("src/main.rs"), "pub fn main() {}\n").unwrap();
        std::fs::write(root.join("src/nested/deep.rs"), "pub fn deep() {}\n").unwrap();
        let files = vec!["src/main.rs".to_string(), "src/nested/deep.rs".to_string()];
        crate::git::cmd::stage_files(&root, &files).unwrap();
        crate::git::cmd::commit_with_developer_author(&root, "Add fixtures", false).unwrap();

        let mut cache = TurnInputCache::default();
        let first = cache.repo_dirs(&root, &[]).await.expect("outline rendered");
        assert_eq!(first, "src\n  nested\n");

        // A folder tracked later would appear in a regenerated outline; the
        // frozen one keeps the prompt prefix byte-identical.
        std::fs::create_dir_all(root.join("extra")).unwrap();
        std::fs::write(root.join("extra/mod.rs"), "pub fn extra() {}\n").unwrap();
        crate::git::cmd::stage_files(&root, &["extra/mod.rs".to_string()]).unwrap();
        crate::git::cmd::commit_with_developer_author(&root, "Add extra", false).unwrap();

        assert_eq!(
            cache.repo_dirs(&root, &[]).await.as_deref(),
            Some(first.as_str())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn isolated_step_freezes_none_history() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut history_manager = HistoryManager::new(root);
        history_manager.record_turn("Prompt", vec![]).unwrap();

        let mut cache = TurnInputCache::default();
        assert!(cache
            .history_tag(&history_manager, TurnIntent::IsolatedStep)
            .await
            .is_none());
        assert!(cache
            .history_tag(&history_manager, TurnIntent::IsolatedStep)
            .await
            .is_none());
    }

    /// A repository whose tracked file list cannot be resolved must degrade
    /// observably: the outline is frozen as absent for the rest of the turn
    /// and the failed listing leaves a warn record, so the degradation never
    /// looks like an empty repository.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unresolvable_repository_freezes_an_absent_outline_with_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let missing_root = dir.path().join("no-such-repository");
        let mut cache = TurnInputCache::default();

        let (buffer, _guard) = crate::blocking::log_capture::install();
        assert!(cache.repo_dirs(&missing_root, &[]).await.is_none());

        let logs = crate::blocking::log_capture::contents(&buffer);
        assert!(
            logs.contains("failed to list repository files"),
            "the degradation must be observable: {logs}"
        );
        assert!(
            logs.contains("the directory outline is unavailable"),
            "the degradation must be observable: {logs}"
        );

        // The degraded outline is frozen: a later round reuses it without
        // re-running the listing or emitting a second record.
        assert!(cache.repo_dirs(&missing_root, &[]).await.is_none());
        assert_eq!(
            crate::blocking::log_capture::contents(&buffer),
            logs,
            "the frozen degradation must not be re-reported"
        );
    }

    /// A repository that cannot be inspected must degrade observably as well:
    /// the map is frozen as absent and the failed listing leaves a warn
    /// record, so a failed offload never looks like an empty repository.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unresolvable_repository_freezes_an_absent_repo_map_with_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let missing_root = dir.path().join("no-such-repository");
        let mut cache = TurnInputCache::default();

        let (buffer, _guard) = crate::blocking::log_capture::install();
        assert!(cache.repo_map(&missing_root, &[], 1000).await.is_none());

        let logs = crate::blocking::log_capture::contents(&buffer);
        assert!(
            logs.contains("failed to list repository files"),
            "the degradation must be observable: {logs}"
        );
        assert!(
            logs.contains("the repository map is unavailable"),
            "the degradation must be observable: {logs}"
        );

        // The degraded map is frozen: a later round reuses it without
        // re-running the listing or emitting a second record.
        assert!(cache.repo_map(&missing_root, &[], 1000).await.is_none());
        assert_eq!(
            crate::blocking::log_capture::contents(&buffer),
            logs,
            "the frozen degradation must not be re-reported"
        );
    }
}
