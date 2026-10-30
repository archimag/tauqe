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
//! the omissions are counted against the candidate set the limits apply to.

use std::collections::HashSet;
use std::path::Path;

use tauqe_core::context::ContextFileContent;
use tauqe_protocol::review::ReviewPreviewWindow;
use tauqe_protocol::{ContextAccess, GlobPattern};

/// Configured limits one audit window obeys, read once per run.
///
/// The file token budget is the only measure of how much a window may load, and
/// it is the budget discovery and the primary audit already enforce, so a single
/// expansion can never step outside what the project declares.
#[derive(Debug, Clone, Copy)]
pub(super) struct WindowBudgets {
    /// Maximum estimated tokens the loaded files may occupy.
    pub(super) files_tokens: usize,
}

/// Files a launch already holds before its declared paths expand.
///
/// A plain launch starts from the active context window, so the context tokens
/// are spent before the first matched file is read and a matched file the
/// context already carries is not added a second time. A grouped launch reads
/// no context and starts empty.
pub(super) struct WindowContext<'a> {
    /// Files the launch already holds.
    pub(super) files: &'a [ContextFileContent],
    /// Estimated tokens those files occupy.
    pub(super) tokens: usize,
}

impl WindowContext<'_> {
    /// A launch that reads no active context: the grouped preset path.
    pub(super) fn empty() -> Self {
        Self {
            files: &[],
            tokens: 0,
        }
    }
}

/// Expanded audit window: the files a launch reads and its typed projection.
pub(super) struct ExpandedWindow {
    /// Matched files the limits accepted, in the order the launch reads them.
    pub(super) files: Vec<ContextFileContent>,
    /// Estimated tokens the whole window occupies, the context included.
    pub(super) estimated_tokens: usize,
    /// Projection of the same window, as `review/preview` publishes it.
    pub(super) window: ReviewPreviewWindow,
}

impl ExpandedWindow {
    /// Whether the declared paths selected no repository file at all.
    ///
    /// A pattern that selects nothing is a mistyped or unsupported path, never
    /// a narrower audit, so a launch refuses it instead of reviewing a
    /// different file set than the developer named.
    pub(super) fn selected_nothing(&self) -> bool {
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
pub(super) async fn expand_window(
    repo_root: &Path,
    context: WindowContext<'_>,
    paths: &[String],
    budgets: WindowBudgets,
) -> Result<ExpandedWindow, String> {
    let patterns: Vec<String> = paths
        .iter()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
        .collect();
    if patterns.is_empty() {
        return Ok(ExpandedWindow {
            files: Vec::new(),
            estimated_tokens: context.tokens,
            window: ReviewPreviewWindow {
                context_files: context.files.len(),
                context_tokens: context.tokens,
                files_token_budget: budgets.files_tokens,
                ..ReviewPreviewWindow::default()
            },
        });
    }

    let root = repo_root.to_path_buf();
    let existing: HashSet<String> = context.files.iter().map(|file| file.path.clone()).collect();
    let (reads, matched_files, candidates) = crate::handlers::offload(move || {
        // A repository Git cannot inspect selects nothing; the launch then
        // reports that it found no file to audit instead of failing the run.
        let available = tauqe_core::git::list_repository_files(Some(&root)).unwrap_or_default();
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
        // already holds is found by the patterns and is not read twice, so the
        // count reports how much the patterns found.
        let matched_files = matched.len();
        matched.retain(|path| !existing.contains(path));
        let candidates = matched.len();
        let mut reads = Vec::new();
        for path in matched {
            if tauqe_core::context::verify_path_within_repo(&root, &path).is_err() {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(root.join(&path)) {
                reads.push((path, content));
            }
        }
        (reads, matched_files, candidates)
    })
    .await
    .map_err(|err| format!("Failed to list the repository for the audit window: {err}"))?;

    let mut files = Vec::new();
    let mut total_tokens = context.tokens;
    for (path, content) in reads {
        // A file costs its content plus the wrapper its own `<file>` element
        // renders, so the window is priced by the same builder that renders the
        // `<context>` block and never measures less than the audit reads.
        let wrapper =
            tauqe_core::prompt::context_element_overhead_tokens(&path, ContextAccess::ReadOnly);
        let content_tokens = tauqe_core::prompt::estimate_tokens_from_bytes(content.len() as u64);
        let tokens = content_tokens as usize + wrapper;
        // The expansion shares the discovery files token budget, so it stops at
        // the first file that would push the window past the configured limit.
        if total_tokens.saturating_add(tokens) > budgets.files_tokens {
            break;
        }
        total_tokens += tokens;
        files.push(ContextFileContent {
            path,
            access: ContextAccess::ReadOnly,
            content,
        });
    }

    let selected_files = files.len();
    let selected_tokens = total_tokens.saturating_sub(context.tokens);
    // The file token budget is the only knife: a matched candidate is either
    // selected or dropped because the budget could not pay for it.
    let omitted_by_budget = candidates.saturating_sub(selected_files);
    Ok(ExpandedWindow {
        files,
        estimated_tokens: total_tokens,
        window: ReviewPreviewWindow {
            context_files: context.files.len(),
            context_tokens: context.tokens,
            matched_files,
            selected_files,
            selected_tokens,
            omitted_files: omitted_by_budget,
            omitted_by_budget,
            files_token_budget: budgets.files_tokens,
        },
    })
}
