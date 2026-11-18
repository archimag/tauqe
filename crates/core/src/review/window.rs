//! Deterministic projection of the audit window a review launch reads.
//!
//! A launch never reads every declared path: the files token budget stops the
//! expansion, so one wide glob cannot drag an unbounded slice of the
//! repository into an audit. The window is therefore a pure function of the
//! declared paths, the files the launch already holds, and the configured
//! limits — and that function lives
//! here once instead of being re-implemented by every launch shape.
//!
//! The plain launch, the grouped preset launch, and the read-only preview all
//! consume this expansion, so the window a developer previews is the window the
//! launch reads (#c-1, #c-4). The counters follow the preview contract declared
//! in `crates/protocol`: matches are counted before the context filter, while
//! the omissions are counted against the candidate set the limits apply to. A
//! matched path the window cannot read is not a candidate at all, so it is
//! counted as unreadable and can neither be selected nor fabricate a
//! truncation the budget did not cause.

use crate::context::ContextAccess;
use std::collections::HashSet;
use std::path::Path;

use crate::context::ContextFileContent;
use tauqe_tools::path_patterns::GlobPattern;

/// Domain counters for one audit window, independent of the RPC preview DTO.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewWindow {
    pub context_files: usize,
    pub context_tokens: usize,
    pub matched_files: usize,
    pub selected_files: usize,
    pub selected_tokens: usize,
    pub omitted_by_budget: usize,
    pub unreadable_files: usize,
    pub files_token_budget: usize,
}

impl ReviewWindow {
    /// Groups spend independent budgets; both counters and allowances sum.
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

/// Configured limits one audit window obeys, read once per run.
///
/// The file token budget is the only measure of how much a window may load, and
/// it is the budget research and the primary audit already enforce, so a single
/// expansion can never step outside what the project declares.
#[derive(Debug, Clone, Copy)]
pub struct WindowBudgets {
    /// Maximum estimated tokens the loaded files may occupy.
    pub files_tokens: usize,
}

/// Files a launch already holds before its declared paths expand.
///
/// A plain launch starts from the active context window, so the context tokens
/// are spent before the first matched file is read and a matched file the
/// context already carries is not added a second time. A grouped launch reads
/// no context and starts empty.
pub struct WindowContext<'a> {
    /// Files the launch already holds.
    pub files: &'a [ContextFileContent],
    /// Estimated tokens those files occupy.
    pub tokens: usize,
}

impl WindowContext<'_> {
    /// A launch that reads no active context: the grouped preset path.
    pub fn empty() -> Self {
        Self {
            files: &[],
            tokens: 0,
        }
    }
}

/// Expanded audit window: the files a launch reads and its typed projection.
pub struct ExpandedWindow {
    /// Matched files the limits accepted, in the order the launch reads them.
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
/// matcher the workspace uses. Listing the repository and reading the matched
/// files are blocking Git and file-system work, so they run on the blocking
/// pool; only a failure of that pool is returned as `Err`, while a repository
/// Git cannot inspect simply selects nothing.
pub async fn expand_window(
    repo_root: &Path,
    context: WindowContext<'_>,
    paths: &[String],
    budgets: WindowBudgets,
) -> Result<ExpandedWindow, AuditWindowError> {
    let patterns: Vec<String> = paths
        .iter()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
        .collect();
    if patterns.is_empty() {
        return Ok(ExpandedWindow {
            files: Vec::new(),
            estimated_tokens: context.tokens,
            window: ReviewWindow {
                context_files: context.files.len(),
                context_tokens: context.tokens,
                files_token_budget: budgets.files_tokens,
                ..ReviewWindow::default()
            },
        });
    }

    let root = repo_root.to_path_buf();
    let existing: HashSet<String> = context.files.iter().map(|file| file.path.clone()).collect();
    let context_files = context.files.len();
    let context_tokens = context.tokens;
    let (files, estimated_tokens, matched_files, unreadable_files, omitted_by_budget) =
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

            // The window prices every candidate as it reads it and stops at the
            // first file the budget cannot pay for, so a wide pattern never
            // materializes the whole matched tree and then discards most of it.
            let mut files = Vec::new();
            let mut total_tokens = context_tokens;
            let mut unreadable = 0usize;
            let mut omitted = 0usize;
            let mut candidates = matched.into_iter();
            while let Some(path) = candidates.next() {
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
                // The expansion shares the files token budget, so it
                // stops at the first file that would push the window past the
                // configured limit; that file and every match behind it are
                // candidates the budget did not pay for and are never read.
                if total_tokens.saturating_add(tokens) > budgets.files_tokens {
                    omitted = omitted.saturating_add(1).saturating_add(candidates.count());
                    break;
                }
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

            (files, total_tokens, matched_files, unreadable, omitted)
        })
        .await
        .map_err(|error| {
            AuditWindowError(format!(
                "Failed to list the repository for the audit window: blocking task failed: {error}"
            ))
        })?;

    let selected_files = files.len();
    let selected_tokens = estimated_tokens.saturating_sub(context_tokens);
    // The file token budget is the only knife: a matched candidate is either
    // selected or dropped because the budget could not pay for it, while a
    // match nobody could read is counted on its own counter.
    Ok(ExpandedWindow {
        files,
        estimated_tokens,
        window: ReviewWindow {
            context_files,
            context_tokens,
            matched_files,
            selected_files,
            selected_tokens,
            omitted_by_budget,
            unreadable_files,
            files_token_budget: budgets.files_tokens,
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
        let context = [ContextFileContent {
            path: "context-only.rs".into(),
            access: ContextAccess::ReadOnly,
            content: "already read".into(),
            insertion_seq: 5,
        }];
        let result = expand_window(
            root.path(),
            WindowContext {
                files: &context,
                tokens: 7,
            },
            &[" ".into()],
            WindowBudgets { files_tokens: 2 },
        )
        .await
        .unwrap();
        assert_eq!(result.estimated_tokens, 7);
        assert_eq!(result.window.context_files, 1);
        assert_eq!(result.window.context_tokens, 7);
        assert_eq!(result.window.files_token_budget, 2);
        assert_eq!(result.window.matched_files, 0);
        assert!(result.files.is_empty());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn expansion_deduplicates_context_and_stops_at_the_first_unaffordable_file() {
        let root = tempfile::tempdir().unwrap();
        init(root.path());
        std::fs::write(root.path().join("a.bin"), [0, 255]).unwrap();
        std::fs::write(root.path().join("b.rs"), "b").unwrap();
        std::fs::write(root.path().join("c.rs"), "c").unwrap();
        std::fs::write(root.path().join("held.rs"), "held").unwrap();
        tauqe_tools::git::cmd::run_git(Some(root.path()), &["add", "-A"]).unwrap();
        let context = [ContextFileContent {
            path: "held.rs".into(),
            access: ContextAccess::ReadOnly,
            content: "held".into(),
            insertion_seq: 9,
        }];
        let price = crate::prompt::context_element_overhead_tokens("b.rs", ContextAccess::ReadOnly)
            + crate::prompt::estimate_tokens_from_bytes(1) as usize;
        let result = expand_window(
            root.path(),
            WindowContext {
                files: &context,
                tokens: 11,
            },
            &[" * ".into(), "b.rs".into(), "*".into()],
            WindowBudgets {
                files_tokens: 11 + price,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            result
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["b.rs"]
        );
        assert_eq!(result.files[0].access, ContextAccess::ReadOnly);
        assert_eq!(result.files[0].insertion_seq, 0);
        assert_eq!(result.window.context_files, 1);
        assert_eq!(result.window.matched_files, 4);
        assert_eq!(result.window.unreadable_files, 1);
        assert_eq!(result.window.omitted_by_budget, 1);
        assert_eq!(result.window.selected_tokens, price);
        assert_eq!(result.estimated_tokens, 11 + price);
        assert!(!result.selected_nothing());
        assert_eq!(std::fs::read(root.path().join("a.bin")).unwrap(), [0, 255]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binary_and_escaping_symlink_matches_do_not_consume_the_budget() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        init(root.path());
        std::fs::write(root.path().join("a.bin"), [255]).unwrap();
        let secret = outside.path().join("outside.rs");
        std::fs::write(&secret, "outside content").unwrap();
        std::os::unix::fs::symlink(&secret, root.path().join("escape.rs")).unwrap();
        tauqe_tools::git::cmd::run_git(Some(root.path()), &["add", "-A"]).unwrap();
        let result = expand_window(
            root.path(),
            WindowContext::empty(),
            &["*".into()],
            WindowBudgets { files_tokens: 1000 },
        )
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
    fn independent_group_budgets_and_counters_accumulate_with_saturation() {
        let group = ReviewWindow {
            matched_files: 2,
            selected_files: 1,
            omitted_by_budget: 1,
            files_token_budget: 100,
            ..Default::default()
        };
        let mut total = ReviewWindow::default();
        total.accumulate(&group);
        total.accumulate(&group);
        assert_eq!(total.files_token_budget, 200);
        assert_eq!(total.matched_files, 4);
        assert_eq!(total.selected_files, 2);
        assert_eq!(total.omitted_by_budget, 2);
        total.files_token_budget = usize::MAX - 1;
        total.matched_files = usize::MAX - 1;
        total.accumulate(&group);
        assert_eq!(total.files_token_budget, usize::MAX);
        assert_eq!(total.matched_files, usize::MAX);
    }
}
