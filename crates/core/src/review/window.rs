//! Deterministic projection of the audit window a review launch reads.
//!
//! The window is the complete union of admitted context and matched paths.
//! Only admission of the final model request can reject its size.
//!
//! The plain launch, the grouped preset launch, and the read-only preview all
//! consume this expansion, so the window a developer previews is the window the
//! launch reads. Unreadable matches are counted separately.

use crate::context::ContextAccess;
use std::collections::HashSet;
use std::path::Path;

use crate::context::ContextFileContent;
use crate::context::ContextManager;
use tauqe_tools::path_patterns::GlobPattern;

/// Domain counters for one audit window, independent of the RPC preview DTO.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewWindow {
    pub context_files: usize,
    pub context_tokens: usize,
    pub matched_files: usize,
    pub selected_files: usize,
    pub selected_tokens: usize,
    /// Legacy preview field; expansion does not drop readable matches.
    pub omitted_by_budget: usize,
    pub unreadable_files: usize,
    /// Legacy preview field; only the complete request has a size limit.
    pub files_token_budget: usize,
}

impl ReviewWindow {
    /// Group windows are independent; their material estimates sum.
    pub fn accumulate(&mut self, window: &Self) {
        self.context_files = self.context_files.saturating_add(window.context_files);
        self.context_tokens = self.context_tokens.saturating_add(window.context_tokens);
        self.matched_files = self.matched_files.saturating_add(window.matched_files);
        self.selected_files = self.selected_files.saturating_add(window.selected_files);
        self.selected_tokens = self.selected_tokens.saturating_add(window.selected_tokens);
        self.omitted_by_budget = self
            .omitted_by_budget
            .saturating_add(window.omitted_by_budget);
        self.unreadable_files = self
            .unreadable_files
            .saturating_add(window.unreadable_files);
        self.files_token_budget = self
            .files_token_budget
            .saturating_add(window.files_token_budget);
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AuditWindowError(String);

/// Expanded audit window: the files a launch reads and its typed projection.
pub struct ExpandedWindow {
    /// Active context followed by newly matched files, without duplicates.
    pub files: Vec<ContextFileContent>,
    /// Estimated tokens the whole window occupies, the context included.
    pub estimated_tokens: usize,
    /// Projection of the same window, as `review/preview` publishes it.
    pub window: ReviewWindow,
}

impl ExpandedWindow {
    /// Whether the declared paths selected no repository file at all.
    ///
    /// A pattern that selects nothing is a mistyped or unsupported path, never
    /// a narrower audit, so a launch refuses it instead of reviewing a
    /// different file set than the developer named.
    pub fn selected_nothing(&self) -> bool {
        self.window.matched_files == 0
    }
}

/// Expands the declared audit paths into the window a launch reads.
///
/// A declared path is a glob pattern, so a literal directory (`crates/core/`),
/// a wildcard (`crates/*/src/`), and an exact file all resolve through the one
/// matcher the workspace uses. Context, Git and file reads run on the blocking
/// pool. A repository Git cannot inspect selects nothing.
pub async fn expand_window(
    repo_root: &Path,
    context: Option<ContextManager>,
    paths: &[String],
) -> Result<ExpandedWindow, AuditWindowError> {
    let (context_files, context_tokens) = crate::execution::cache::audit_context_files(context)
        .await
        .map_err(AuditWindowError)?;
    let patterns: Vec<String> = paths
        .iter()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
        .collect();
    if patterns.is_empty() {
        return Ok(ExpandedWindow {
            files: context_files.clone(),
            estimated_tokens: context_tokens,
            window: ReviewWindow {
                context_files: context_files.len(),
                context_tokens,
                ..ReviewWindow::default()
            },
        });
    }

    let root = repo_root.to_path_buf();
    let existing: HashSet<String> = context_files.iter().map(|file| file.path.clone()).collect();
    let context_count = context_files.len();
    let (matched_content, estimated_tokens, matched_files, unreadable_files) =
        tokio::task::spawn_blocking(move || {
            // A repository Git cannot inspect selects nothing; the launch then
            // reports that it found no file to audit instead of failing the run.
            let available = crate::git::list_repository_files(Some(&root)).unwrap_or_default();
            let compiled: Vec<GlobPattern> = patterns
                .iter()
                .map(|pattern| GlobPattern::new(pattern))
                .collect();
            let mut matched: Vec<String> = available
                .into_iter()
                .filter(|path| compiled.iter().any(|pattern| pattern.matches(path)))
                .collect();
            matched.sort();
            matched.dedup();
            // Matches are counted before the context filter: a file the launch
            // already holds is found by the patterns and is not read twice, so
            // the count reports how much the patterns found.
            let held = matched
                .iter()
                .filter(|path| existing.contains(*path))
                .count();
            matched.retain(|path| !existing.contains(path));
            let matched_files = held.saturating_add(matched.len());

            // Price every readable match, without a separate file allowance.
            let mut files = Vec::new();
            let mut total_tokens = context_tokens;
            let mut unreadable = 0usize;
            for path in matched {
                // A fresh match the window cannot read is not a review
                // candidate: a binary blob, a symlink escaping the repository,
                // and a file deleted from the working tree but still tracked
                // all fail here. Charging such a path to the token budget would
                // report a truncation no limit caused.
                if crate::context::verify_path_within_repo(&root, &path).is_err() {
                    unreadable += 1;
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(root.join(&path)) else {
                    unreadable += 1;
                    continue;
                };
                // A file costs its content plus the wrapper its own `<file>`
                // element renders, so the window is priced by the same builder
                // that renders the `<context>` block and never measures less
                // than the audit reads.
                let wrapper =
                    crate::prompt::context_element_overhead_tokens(&path, ContextAccess::ReadOnly);
                let content_tokens =
                    crate::prompt::estimate_tokens_from_bytes(content.len() as u64);
                let tokens = content_tokens as usize + wrapper;
                total_tokens = total_tokens.saturating_add(tokens);
                files.push(ContextFileContent {
                    path,
                    access: ContextAccess::ReadOnly,
                    content,
                    // A window is materialized outside the context layers, so
                    // the matched file never entered one and carries no
                    // arrival watermark.
                    insertion_seq: 0,
                });
            }

            (files, total_tokens, matched_files, unreadable)
        })
        .await
        .map_err(|error| {
            AuditWindowError(format!(
                "Failed to list the repository for the audit window: blocking task failed: {error}"
            ))
        })?;

    let selected_files = matched_content.len();
    let selected_tokens = estimated_tokens.saturating_sub(context_tokens);
    let mut files = context_files;
    files.extend(matched_content);
    Ok(ExpandedWindow {
        files,
        estimated_tokens,
        window: ReviewWindow {
            context_files: context_count,
            context_tokens,
            matched_files,
            selected_files,
            selected_tokens,
            unreadable_files,
            ..ReviewWindow::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_tools::git::{state::init_repository, GitHooks};

    fn init(root: &Path) {
        init_repository(Some(root), true, GitHooks::Bypass).unwrap();
    }

    #[tokio::test]
    async fn blank_patterns_keep_context_and_do_not_inspect_the_repository() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("context-only.rs"), "already read").unwrap();
        let mut context = ContextManager::new(root.path().to_path_buf());
        context
            .add_file("context-only.rs", ContextAccess::ReadOnly)
            .unwrap();
        let result = expand_window(root.path(), Some(context), &[" ".into()])
            .await
            .unwrap();
        assert!(result.estimated_tokens > 0);
        assert_eq!(result.window.context_files, 1);
        assert_eq!(result.window.context_tokens, result.estimated_tokens);
        assert_eq!(result.window.files_token_budget, 0);
        assert_eq!(result.window.matched_files, 0);
        assert_eq!(result.files[0].path, "context-only.rs");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn expansion_deduplicates_context_and_selects_every_readable_match() {
        let root = tempfile::tempdir().unwrap();
        init(root.path());
        std::fs::write(root.path().join("a.bin"), [0, 255]).unwrap();
        std::fs::write(root.path().join("b.rs"), "b").unwrap();
        std::fs::write(root.path().join("c.rs"), "c").unwrap();
        std::fs::write(root.path().join("held.rs"), "held").unwrap();
        tauqe_tools::git::cmd::run_git(Some(root.path()), &["add", "-A"]).unwrap();
        let mut context = ContextManager::new(root.path().to_path_buf());
        context
            .add_file("held.rs", ContextAccess::ReadOnly)
            .unwrap();
        let context_tokens = context.get_state().total_estimated_tokens as usize;
        let price = crate::prompt::context_element_overhead_tokens("b.rs", ContextAccess::ReadOnly)
            + crate::prompt::estimate_tokens_from_bytes(1) as usize;
        let result = expand_window(
            root.path(),
            Some(context),
            &[" * ".into(), "b.rs".into(), "*".into()],
        )
        .await
        .unwrap();
        assert_eq!(
            result
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["held.rs", "b.rs", "c.rs"]
        );
        assert_eq!(result.files[0].access, ContextAccess::ReadOnly);
        assert!(result.files[0].insertion_seq > 0);
        assert_eq!(result.files[1].insertion_seq, 0);
        assert_eq!(result.window.context_files, 1);
        assert_eq!(result.window.matched_files, 4);
        assert_eq!(result.window.unreadable_files, 1);
        assert_eq!(result.window.omitted_by_budget, 0);
        assert_eq!(result.window.selected_tokens, price * 2);
        assert_eq!(result.estimated_tokens, context_tokens + price * 2);
        assert!(!result.selected_nothing());
        assert_eq!(std::fs::read(root.path().join("a.bin")).unwrap(), [0, 255]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binary_and_escaping_symlink_matches_are_unreadable() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        init(root.path());
        std::fs::write(root.path().join("a.bin"), [255]).unwrap();
        let secret = outside.path().join("outside.rs");
        std::fs::write(&secret, "outside content").unwrap();
        std::os::unix::fs::symlink(&secret, root.path().join("escape.rs")).unwrap();
        tauqe_tools::git::cmd::run_git(Some(root.path()), &["add", "-A"]).unwrap();
        let result = expand_window(root.path(), None, &["*".into()])
            .await
            .unwrap();
        assert_eq!(result.window.matched_files, 2);
        assert_eq!(result.window.unreadable_files, 2);
        assert_eq!(result.window.selected_files, 0);
        assert_eq!(result.window.omitted_by_budget, 0);
        assert_eq!(result.estimated_tokens, 0);
        assert!(result.files.is_empty());
        assert!(!result.selected_nothing());
    }

    #[test]
    fn independent_group_materials_and_counters_accumulate_with_saturation() {
        let group = ReviewWindow {
            matched_files: 2,
            selected_files: 1,
            selected_tokens: 100,
            ..Default::default()
        };
        let mut total = ReviewWindow::default();
        total.accumulate(&group);
        total.accumulate(&group);
        assert_eq!(total.selected_tokens, 200);
        assert_eq!(total.matched_files, 4);
        assert_eq!(total.selected_files, 2);
        total.selected_tokens = usize::MAX - 1;
        total.matched_files = usize::MAX - 1;
        total.accumulate(&group);
        assert_eq!(total.selected_tokens, usize::MAX);
        assert_eq!(total.matched_files, usize::MAX);
    }
}
