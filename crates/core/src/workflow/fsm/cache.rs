//! Per-turn memoization of deterministic proposal inputs.
//!
//! The repository map is the heaviest deterministic input of a proposal round:
//! a full Tree-sitter pass over every tracked file outside the active context.
//! It is generated once, on the first proposal round of a bounded turn, and
//! frozen for the rest of that turn. Patch retries, verification healing, and
//! research rounds that widened the context all receive the identical map
//! string, so the repository map block of the prompt stays byte-identical and a
//! provider's prefix cache keeps matching its tokens across rounds.

use std::path::{Path, PathBuf};
use tauqe_protocol::TurnIntent;

use crate::history::{HistoryManager, HistorySnapshot};
use crate::prompt::ContextBlockCache;

/// Per-turn memoization of deterministic proposal inputs.
#[derive(Default)]
pub(super) struct TurnInputCache {
    /// Repo map captured on the first proposal round of the turn.
    ///
    /// `Some` marks the map as generated, so an empty map is frozen as well
    /// instead of being recomputed on every round.
    repo_map: Option<FrozenRepoMap>,
    /// History records captured on the first proposal round of the turn.
    ///
    /// `Some` marks the history as resolved, so an absent or empty history is
    /// frozen as well instead of re-reading history.jsonl on each round. The
    /// snapshot keeps the records rather than the rendered block, so every
    /// layout reads the same structural material.
    history: Option<std::sync::Arc<HistorySnapshot>>,
    /// Body of every context file this turn has already read.
    ///
    /// A round renders the context it was handed, and within one turn that body
    /// only changes when the change set reaches the working tree. The memo
    /// therefore reads each path once and re-reads it only when the identity it
    /// recorded moved, so a file discovery just added costs one read while every
    /// later proposal, patch-retry, and healing round reuses the bytes already
    /// held.
    context_contents: std::collections::HashMap<String, MemoizedContextFile>,
    /// Directory outline captured on the first proposal round of the turn.
    ///
    /// `Some` marks the outline as rendered, so an empty tree is frozen as well
    /// instead of re-walking the tracked file list on every round.
    repo_dirs: Option<FrozenRepoDirs>,
    /// Project configuration frozen on the first proposal round of the turn.
    project: Option<tauqe_protocol::ProjectConfig>,
    /// Rendered context elements captured across the rounds of the turn.
    ///
    /// The memo keeps the rendering of every file that already entered the
    /// context block byte-identical from one round to the next, so the block
    /// only ever grows at its tail and the anchors of an unchanged file never
    /// shift under the provider's cached prompt prefix.
    context_block: ContextBlockCache,
}

/// Repository map frozen for the remaining rounds of a bounded turn.
struct FrozenRepoMap {
    map: Option<String>,
}

/// Compact directory outline frozen for the remaining rounds of a bounded turn.
struct FrozenRepoDirs {
    tree: Option<String>,
}
/// On-disk identity of a context file as the memo recorded it.
///
/// The pair tells an unchanged file from a rewritten one without reading the
/// body: a file keeps its identity until something writes it. A filesystem
/// that reports no modification time always answers `false`, so the file is
/// read again instead of serving bytes the memo cannot prove current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContextFileIdentity {
    size_bytes: u64,
    modified: Option<std::time::SystemTime>,
}

impl ContextFileIdentity {
    /// Returns the identity of a file whose metadata was just read.
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            size_bytes: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }

    /// Reports whether the file at `full_path` still carries this identity.
    fn is_current(&self, full_path: &Path) -> bool {
        let Ok(metadata) = std::fs::metadata(full_path) else {
            return false;
        };
        match (self.modified, metadata.modified().ok()) {
            (Some(memoized), Some(current)) => {
                self.size_bytes == metadata.len() && memoized == current
            }
            _ => false,
        }
    }
}

/// Content of one context file memoized for the rest of the turn.
#[derive(Debug)]
struct MemoizedContextFile {
    identity: ContextFileIdentity,
    content: String,
}

fn context_item_disk_path(
    repo_root: &Path,
    path: &str,
    layer: tauqe_protocol::ContextLayer,
) -> PathBuf {
    if layer == tauqe_protocol::ContextLayer::OuterDocs
        && !crate::outer_docs::is_outer_docs_cache_path(path)
    {
        repo_root
            .join(crate::outer_docs::OUTER_DOCS_CACHE_DIR)
            .join(path)
    } else {
        repo_root.join(path)
    }
}

impl MemoizedContextFile {
    /// Reads `full_path` and records the identity the read started from.
    ///
    /// The metadata is taken before the body on purpose: a write that lands
    /// while the file is being read moves the identity, so the next round reads
    /// the file again instead of trusting content that may already be stale.
    fn read(full_path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(full_path).ok()?;
        let identity = ContextFileIdentity::of(&metadata);
        let content = std::fs::read_to_string(full_path).ok()?;
        Some(Self { identity, content })
    }
}

impl TurnInputCache {
    /// Returns the repository map frozen at the start of the turn.
    ///
    /// The first call generates the map from the tracked files, the paths
    /// already present in context, and the token budget, then freezes the
    /// result. Every later call returns that exact string, even when a research
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

    /// Returns the interaction records frozen at the start of the turn.
    ///
    /// A regime that suppresses session history never injects it and freezes an
    /// empty snapshot: an isolated step and an Architect synthesis both qualify.
    /// For regular turns the first call reads the session records off the
    /// executor thread and freezes them; every later round reuses that exact
    /// vector without hitting disk or re-parsing JSONL records. The freeze is
    /// structural: the semantic layout renders the `<history>` block from it
    /// byte for byte, and the chronological layout reads the recorded context
    /// watermarks from the same material. Records appended while the turn runs
    /// never reach the snapshot, so the prompt prefix stays byte-identical.
    pub(super) async fn history_snapshot(
        &mut self,
        history_manager: &HistoryManager,
        intent: TurnIntent,
    ) -> Option<std::sync::Arc<HistorySnapshot>> {
        if self.history.is_none() {
            let snapshot = Self::resolve_history(history_manager, intent).await;
            self.history = Some(std::sync::Arc::new(snapshot));
        }
        self.history
            .as_ref()
            .filter(|snapshot| !snapshot.is_empty())
            .cloned()
    }

    /// Reads the session records a turn freezes for its whole duration.
    async fn resolve_history(
        history_manager: &HistoryManager,
        intent: TurnIntent,
    ) -> HistorySnapshot {
        if intent.suppresses_session_history() {
            return HistorySnapshot::default();
        }

        let hm = history_manager.clone();
        match crate::blocking::run_blocking_async(move || hm.snapshot()).await {
            Ok(Ok(snapshot)) => snapshot,
            // A history that could not be read is not an empty history: the
            // degradation is recorded before it is treated as absent.
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    "failed to read the session history; the turn proceeds without it"
                );
                HistorySnapshot::default()
            }
            Err(join_error) => {
                crate::blocking::log_join_error(&join_error, "session history reading");
                HistorySnapshot::default()
            }
        }
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
    ///
    /// The first call reads `tauqe.toml` and, when the project declares a task
    /// runner, runs the runner's dump once. Every later phase of the turn —
    /// staging, the declared post-hook, verification, and project commands —
    /// reads this snapshot through `TurnKernel::project`, so a patch retry or
    /// a healing round never spawns the runner again.
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

    /// Returns the context files of a round, reading only the paths this turn
    /// has not read yet.
    ///
    /// The list follows the context state's item order, so the memo never
    /// reorders a file an earlier round already rendered. A path that entered
    /// the context through research is read once, on first appearance, and
    /// every later proposal, patch-retry, and healing round reuses the bytes
    /// already held. A path whose recorded identity moved — the change set a
    /// turn applied to the working tree — is read again, so a healing round
    /// reasons about the bytes it is about to patch rather than the bytes the
    /// turn opened with.
    ///
    /// A path that cannot be read leaves the memo and is omitted from the round,
    /// which is how an unreadable context file has always been rendered. A
    /// failed offload is reported to the caller instead of silently shrinking
    /// the context. Every returned file carries the insertion watermark of its
    /// context entry, so a layout places it where it actually arrived.
    pub(super) async fn context_files(
        &mut self,
        repo_root: &Path,
        items: &[tauqe_protocol::ContextItem],
    ) -> anyhow::Result<Vec<crate::context::ContextFileContent>> {
        let mut memoized: Vec<(String, PathBuf)> = Vec::new();
        let mut to_read: Vec<(String, PathBuf)> = Vec::new();
        for item in items {
            let disk_path = context_item_disk_path(repo_root, &item.path, item.layer);
            if self.context_contents.contains_key(&item.path) {
                memoized.push((item.path.clone(), disk_path));
            } else {
                to_read.push((item.path.clone(), disk_path));
            }
        }

        // Serving a memoized file costs one metadata query, never a content
        // read: the identity it recorded decides whether the bytes the memo
        // holds are still the bytes on disk.
        if !memoized.is_empty() {
            let identities: Vec<(String, PathBuf, ContextFileIdentity)> = memoized
                .into_iter()
                .filter_map(|(path, disk_path)| {
                    self.context_contents
                        .get(&path)
                        .map(|entry| (path, disk_path, entry.identity))
                })
                .collect();
            if !identities.is_empty() {
                let moved = crate::blocking::run_blocking_async(move || {
                    identities
                        .into_iter()
                        .filter(|(_, disk_path, identity)| !identity.is_current(disk_path))
                        .map(|(path, disk_path, _)| (path, disk_path))
                        .collect::<Vec<(String, PathBuf)>>()
                })
                .await?;
                to_read.extend(moved);
            }
        }

        if !to_read.is_empty() {
            let read = crate::blocking::run_blocking_async(move || {
                to_read
                    .into_iter()
                    .map(|(path, disk_path)| {
                        let entry = MemoizedContextFile::read(&disk_path);
                        (path, entry)
                    })
                    .collect::<Vec<(String, Option<MemoizedContextFile>)>>()
            })
            .await?;
            for (path, entry) in read {
                match entry {
                    Some(entry) => {
                        self.context_contents.insert(path, entry);
                    }
                    None => {
                        self.context_contents.remove(&path);
                    }
                }
            }
        }

        let files: Vec<crate::context::ContextFileContent> = items
            .iter()
            .filter_map(|item| {
                self.context_contents.get(&item.path).map(|entry| {
                    crate::context::ContextFileContent {
                        path: item.path.clone(),
                        access: item.access,
                        content: entry.content.clone(),
                        insertion_seq: item.insertion_seq,
                    }
                })
            })
            .collect();
        Ok(files)
    }

    /// Returns the turn-scoped memo of rendered context elements.
    ///
    /// The memo is owned by the turn rather than by a single prompt assembly, so
    /// a file rendered in an earlier round is reproduced verbatim while the
    /// rounds that widened the context render only the files they added. An
    /// unchanged file therefore keeps the exact bytes — and the exact line
    /// anchors — the previous round already sent to the provider.
    pub(super) fn context_cache(&mut self) -> &mut ContextBlockCache {
        &mut self.context_block
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The context memo reads a file once per turn and re-reads it only when the
    /// bytes on disk moved: a discovery addition costs one read, and a change set
    /// the turn applied to the working tree is rendered from the bytes now on
    /// disk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn context_files_are_read_once_and_refreshed_when_the_bytes_move() {
        use crate::context::ContextManager;
        use tauqe_protocol::ContextAccess;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "alpha\n").unwrap();
        std::fs::write(root.join("b.rs"), "beta\n").unwrap();

        let mut context = ContextManager::new(root.clone());
        context.add_file("a.rs", ContextAccess::Editable).unwrap();
        let mut cache = TurnInputCache::default();

        let opening = context.get_state();
        let files = cache
            .context_files(&root, &opening.items)
            .await
            .expect("the opening round reads the context");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "alpha\n");

        // A research addition is read on first appearance, while the file the
        // opening round already read keeps the bytes it was rendered with.
        context
            .add_auto_file("b.rs", ContextAccess::ReadOnly)
            .unwrap();
        let widened = context.get_state();
        let files = cache
            .context_files(&root, &widened.items)
            .await
            .expect("the widened round reads only the new path");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].content, "alpha\n");
        assert_eq!(files[1].content, "beta\n");

        // The turn applied a change set, so the memoized bytes are stale and the
        // round serves what is on disk now.
        std::fs::write(root.join("a.rs"), "alpha, revised\n").unwrap();
        let healed = context.get_state();
        let files = cache
            .context_files(&root, &healed.items)
            .await
            .expect("the healing round reads the rewritten file again");
        assert_eq!(files[0].content, "alpha, revised\n");
        assert_eq!(files[1].content, "beta\n");
    }

    /// The context memo is scoped to the turn: it reproduces an unchanged file
    /// byte for byte and extends the block only at its tail when a discovery
    /// round appends a file.
    #[test]
    fn context_cache_is_turn_scoped_and_extends_at_the_tail() {
        use crate::context::ContextFileContent;
        use crate::prompt::PromptAssembly;
        use tauqe_protocol::{ContextAccess, EditProtocolKind, PatchStrategy};

        let file = |path: &str, content: &str| ContextFileContent {
            path: path.to_string(),
            access: ContextAccess::Editable,
            content: content.to_string(),
            // The fixture synthesizes files outside the context layers, so it
            // carries the unrecorded arrival.
            insertion_seq: 0,
        };

        let mut cache = TurnInputCache::default();
        let mut assembly = PromptAssembly::new(
            None,
            1,
            vec![file("a.rs", "alpha\n")],
            "git",
            EditProtocolKind::Xml,
            None,
            None,
        )
        .with_patch_strategy(PatchStrategy::LineAnchor);

        let first = assembly
            .format_context_block_with(cache.context_cache())
            .expect("the first round renders the block");
        let second = assembly
            .format_context_block_with(cache.context_cache())
            .expect("the memo reproduces the block");
        assert_eq!(second, first);

        // A research round appends a file: the previous element keeps its exact
        // bytes and the new one lands at the tail.
        assembly.context_files.push(file("b.rs", "beta\n"));
        let third = assembly
            .format_context_block_with(cache.context_cache())
            .expect("the widened round renders the block");
        assert!(third.starts_with(&first[..first.len() - "</context>".len()]));
        assert!(third.contains("b.rs"));
    }

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
    async fn repo_map_stays_identical_when_research_widens_the_context() {
        let (_dir, root) = repo_with_two_files();
        let mut cache = TurnInputCache::default();

        let opening_round: Vec<String> = Vec::new();
        let first = cache
            .repo_map(&root, &opening_round, 1000)
            .await
            .expect("the opening round generates the map");
        assert!(first.contains("b.rs"));
        assert!(first.contains("pub fn beta"));

        // A research round pulls `b.rs` into the context, which would exclude
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
    async fn history_snapshot_is_frozen_across_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut history_manager = HistoryManager::new(root);
        history_manager
            .record_turn("Original prompt", vec!["a.rs".to_string()])
            .unwrap();

        let mut cache = TurnInputCache::default();
        let first = cache
            .history_snapshot(&history_manager, TurnIntent::InteractiveRepl)
            .await
            .expect("first round resolves history");
        // The frozen records render exactly the block the live history would
        // have produced at that moment.
        let block = first.render_block().expect("the frozen records render");
        assert_eq!(block, history_manager.format_history_tag().unwrap());
        assert!(block.contains("Original prompt"));

        // A record appended while the turn runs never reaches the snapshot.
        history_manager
            .record_turn("Subsequent prompt", vec!["b.rs".to_string()])
            .unwrap();

        let second = cache
            .history_snapshot(&history_manager, TurnIntent::InteractiveRepl)
            .await
            .expect("second round reuses frozen history");
        assert_eq!(second, first);
        assert!(!second
            .render_block()
            .expect("the frozen records render")
            .contains("Subsequent prompt"));
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
            .history_snapshot(&history_manager, TurnIntent::IsolatedStep)
            .await
            .is_none());
        assert!(cache
            .history_snapshot(&history_manager, TurnIntent::IsolatedStep)
            .await
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn architect_synthesis_freezes_none_history() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut history_manager = HistoryManager::new(root);
        history_manager.record_turn("Prompt", vec![]).unwrap();

        let mut cache = TurnInputCache::default();
        assert!(cache
            .history_snapshot(&history_manager, TurnIntent::ArchitectSynthesis)
            .await
            .is_none());
    }
    /// The freeze is structural: the context watermarks the frozen turn
    /// records observed stay readable for the whole turn, even after the live
    /// history moved on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn frozen_snapshot_keeps_the_recorded_turn_watermarks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let storage = crate::history::SessionStorage::new(root.clone());
        storage
            .append_entry(&crate::history::HistoryEntry::Turn {
                id: Some(1),
                prompt: "Original prompt".to_string(),
                context: vec![],
                context_watermark: Some(12),
            })
            .unwrap();
        let history_manager = HistoryManager::new(root);

        let mut cache = TurnInputCache::default();
        let frozen = cache
            .history_snapshot(&history_manager, TurnIntent::InteractiveRepl)
            .await
            .expect("the turn freezes its history");
        assert_eq!(frozen.watermark(), Some(12));
        assert!(matches!(
            frozen.entries(),
            [crate::history::HistoryEntry::Turn { .. }]
        ));

        // The live history keeps growing while the frozen structure stays the
        // material this turn renders and interleaves from.
        storage
            .append_entry(&crate::history::HistoryEntry::Turn {
                id: Some(2),
                prompt: "Later prompt".to_string(),
                context: vec![],
                context_watermark: Some(20),
            })
            .unwrap();
        let again = cache
            .history_snapshot(&history_manager, TurnIntent::InteractiveRepl)
            .await
            .expect("the frozen snapshot is reused");
        assert_eq!(again.watermark(), Some(12));
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

    /// External documentation snapshots in the OuterDocs layer are stored under
    /// the isolated cache directory, while their context item records the
    /// logical path. The turn cache must resolve them to their real on-disk
    /// cache location rather than treating them as repository-relative files.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn outer_docs_context_files_are_read_from_the_isolated_cache() {
        use crate::context::ContextManager;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let cache_dir = root.join(crate::outer_docs::OUTER_DOCS_CACHE_DIR);
        std::fs::create_dir_all(cache_dir.join("openrouter")).unwrap();
        std::fs::write(
            cache_dir.join("openrouter/models.md"),
            "# OpenRouter Models\n",
        )
        .unwrap();

        let mut context = ContextManager::new(root.clone());
        let cached_rel = format!(
            "{}/openrouter/models.md",
            crate::outer_docs::OUTER_DOCS_CACHE_DIR
        );
        context
            .add_outer_docs_file(&cached_rel)
            .expect("the external doc is added");
        assert_eq!(
            context.outer_docs_files(),
            vec!["openrouter/models.md".to_string()]
        );

        let mut cache = TurnInputCache::default();
        let state = context.get_state();
        let files = cache
            .context_files(&root, &state.items)
            .await
            .expect("read context files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "openrouter/models.md");
        assert_eq!(files[0].content, "# OpenRouter Models\n");
    }
}
