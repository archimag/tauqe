//! Per-turn memoization of deterministic proposal inputs.
//!
//! The repository map is the heaviest deterministic input of a proposal round:
//! a Tree-sitter pass over tracked files, independent of the active context.
//! It is generated once, on the first proposal round of a bounded turn, and
//! frozen for the rest of that turn. Patch retries, verification healing, and
//! research rounds that widened the context all receive the identical map
//! string, so the repository map block of the prompt stays byte-identical and a
//! provider's prefix cache keeps matching its tokens across rounds.

use std::path::{Path, PathBuf};

use crate::prompt::ContextBlockCache;

/// Per-turn memoization of deterministic proposal inputs.
#[derive(Default)]
pub struct TurnInputCache {
    /// Repo map captured on the first proposal round of the turn.
    ///
    /// `Some` marks the map as generated, so an empty map is frozen as well
    /// instead of being recomputed on every round.
    repo_map: Option<FrozenRepoMap>,
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
    project: Option<crate::config::project::ProjectConfig>,
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
    layer: crate::context::ContextLayer,
) -> PathBuf {
    if layer == crate::context::ContextLayer::OuterDocs
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
    /// The first call generates the map from tracked files within the token budget,
    /// independently of the active context. Later calls reuse the exact result,
    /// even if files change on disk during the turn.
    pub(crate) async fn repo_map(
        &mut self,
        repo_root: &Path,
        token_budget: usize,
    ) -> Option<String> {
        if let Some(frozen) = &self.repo_map {
            return frozen.map.clone();
        }

        let root = repo_root.to_path_buf();
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
                    &[],
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

    /// Returns the compact directory outline frozen at the start of the turn.
    ///
    /// The tree is derived only from the tracked file list, so it is rendered
    /// once and reused verbatim by every later round: the outline block stays
    /// byte-identical and the provider keeps matching its cached prefix. Files
    /// anchored by the pinned context layer are marked, so the model never
    /// searches for conventions that already travelled with the prompt.
    pub(crate) async fn repo_dirs(
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
    /// reads this snapshot through the executor, so a patch retry or
    /// a healing round never spawns the runner again.
    pub(crate) async fn project(
        &mut self,
        repo_root: &Path,
        cancel_rx: Option<&tokio::sync::watch::Receiver<bool>>,
    ) -> crate::config::project::ProjectConfig {
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
    pub(crate) async fn context_files(
        &mut self,
        repo_root: &Path,
        items: &[crate::context::ContextItem],
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
    pub(crate) fn context_cache(&mut self) -> &mut ContextBlockCache {
        &mut self.context_block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextAccess;

    #[tokio::test]
    async fn project_configuration_is_frozen_for_the_lifetime_of_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let config = root.join("tauqe.toml");
        let write_project = |name| {
            std::fs::write(
                &config,
                format!("[models]\nrepl = ['openrouter:mock/model']\n[project.commands.{name}]\ncommand = 'true'\n"),
            )
            .unwrap();
        };
        write_project("first");
        let mut cache = TurnInputCache::default();
        assert!(cache
            .project(root, None)
            .await
            .commands
            .contains_key("first"));

        write_project("replacement");
        let frozen = cache.project(root, None).await;
        assert!(frozen.commands.contains_key("first"));
        assert!(!frozen.commands.contains_key("replacement"));
        assert!(TurnInputCache::default()
            .project(root, None)
            .await
            .commands
            .contains_key("replacement"));
    }

    /// The context memo reads a file once per turn and re-reads it only when the
    /// bytes on disk moved: a discovery addition costs one read, and a change set
    /// the turn applied to the working tree is rendered from the bytes now on
    /// disk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn context_files_are_read_once_and_refreshed_when_the_bytes_move() {
        use crate::context::ContextManager;

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
    /// byte for byte and appends a discovered file to the transcript without
    /// moving a byte the provider already cached.
    #[test]
    fn context_cache_is_turn_scoped_and_extends_at_the_tail() {
        use crate::context::ContextFileContent;
        use crate::prompt::json::JsonPromptProtocol;
        use crate::prompt::PromptAssembly;
        use crate::workflow::EditProtocolKind;
        use crate::workflow::PatchStrategy;

        let file = |path: &str, content: &str| ContextFileContent {
            path: path.to_string(),
            access: ContextAccess::Editable,
            content: content.to_string(),
            // The fixture synthesizes files outside the context layers, so it
            // carries the unrecorded arrival.
            insertion_seq: 0,
        };

        let protocol = JsonPromptProtocol::new(crate::llm::CapabilityTree::leaf(
            crate::llm::LlmCapability::TextMessage,
        ));
        let mut cache = TurnInputCache::default();
        let mut assembly = PromptAssembly::new(
            None,
            1,
            vec![file("a.rs", "alpha\n")],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        )
        .with_patch_strategy(PatchStrategy::LineAnchor);

        let first = assembly.assemble_chat_messages_with_context_cache(
            "Task",
            &protocol,
            cache.context_cache(),
        );
        let second = assembly.assemble_chat_messages_with_context_cache(
            "Task",
            &protocol,
            cache.context_cache(),
        );
        assert_eq!(second, first);
        assert!(first
            .iter()
            .any(|message| message.content.contains(r#"path="a.rs""#)));

        // A research round appends a file: the head keeps its exact bytes and the
        // new element lands in the context message of the transcript.
        assembly.context_files.push(file("b.rs", "beta\n"));
        let third = assembly.assemble_chat_messages_with_context_cache(
            "Task",
            &protocol,
            cache.context_cache(),
        );
        assert_eq!(third[0], first[0]);
        assert!(third
            .iter()
            .any(|message| message.content.contains(r#"path="b.rs""#)));
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
    async fn repository_changes_only_reach_the_next_turns_map() {
        let (_dir, root) = repo_with_two_files();
        let mut first_turn = TurnInputCache::default();
        let before = first_turn
            .repo_map(&root, 1000)
            .await
            .expect("first turn map");
        assert!(before.contains("pub fn beta"));
        assert_eq!(
            TurnInputCache::default()
                .repo_map(&root, 1000)
                .await
                .as_deref(),
            Some(before.as_str())
        );

        std::fs::write(root.join("b.rs"), "pub fn rewritten_beta() {}\n").unwrap();
        assert_eq!(
            first_turn.repo_map(&root, 1000).await.as_deref(),
            Some(before.as_str())
        );

        let after = TurnInputCache::default()
            .repo_map(&root, 1000)
            .await
            .expect("next turn map");
        assert!(after.contains("pub fn rewritten_beta"));
        assert!(after.contains("a.rs"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_repo_map_is_frozen_instead_of_being_regenerated() {
        let (_dir, root) = repo_with_two_files();
        let mut cache = TurnInputCache::default();
        assert!(cache.repo_map(&root, 0).await.is_none());
        assert!(cache.repo_map(&root, 1000).await.is_none());
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
        assert!(cache.repo_map(&missing_root, 1000).await.is_none());

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
        assert!(cache.repo_map(&missing_root, 1000).await.is_none());
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
