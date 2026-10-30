pub mod journal;
pub mod search;

use std::collections::BTreeSet;
use std::path::Path;

use tauqe_protocol::{
    ContextAccess, DirListRequest, DocTopic, LlmCapability, LlmCommand, RepoSearchRequest,
};

use crate::blocking::{run_blocking, run_blocking_async};
use crate::config::ContextConfig;
use crate::context::ContextManager;
use crate::edits::protocol::ContextRequest;
use crate::mockups;
use crate::refs;

pub use search::{
    execute_repo_search, execute_repo_searches, format_search_feedback,
    format_single_search_feedback, normalize_search_path, FileSearchMatch, RepoSearchResult,
    MAX_SEARCHABLE_FILE_SIZE, REGEX_DFA_SIZE_LIMIT, REGEX_SIZE_LIMIT,
};

/// Operational caps applied to one Discovery round.
///
/// Every cap that bounds how much material a round may report is resolved from
/// the project's `[context]` section, so a project that routinely works with
/// wider searches or larger listings raises the ceilings in `tauqe.toml`
/// instead of recompiling the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryLimits {
    /// Repository search queries honored in one round.
    pub max_search_requests_per_round: usize,
    /// Matching files reported for a single search query.
    pub max_matched_files_per_search: usize,
    /// Entries rendered by a single directory listing.
    pub max_dir_list_entries: usize,
    /// Distinct targeted map requests honored in one round.
    pub max_repo_map_requests_per_round: usize,
    /// Total token budget of the external reference documents kept in context.
    pub refs_token_budget: usize,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        Self::from_context_config(&ContextConfig::default())
    }
}

impl DiscoveryLimits {
    /// Reads the Discovery caps declared by the project configuration.
    pub fn from_context_config(config: &ContextConfig) -> Self {
        Self {
            max_search_requests_per_round: config.max_search_requests_per_round,
            max_matched_files_per_search: config.max_matched_files_per_search,
            max_dir_list_entries: config.max_dir_list_entries,
            max_repo_map_requests_per_round: config.max_repo_map_requests_per_round,
            refs_token_budget: config.refs_token_budget,
        }
    }
}

pub(crate) fn access_satisfies(current: Option<ContextAccess>, requested: ContextAccess) -> bool {
    match current {
        Some(ContextAccess::Editable) => true,
        Some(ContextAccess::ReadOnly) => requested == ContextAccess::ReadOnly,
        None => false,
    }
}

/// Diagnostic details when requested files exceed the configured files token budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextLimitExceeded {
    pub current_tokens: usize,
    pub requested_tokens: usize,
    pub token_budget: usize,
    pub requested_files: Vec<String>,
}

/// One external reference document the Refs layer refused to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefsRejection {
    /// Repository-relative cache path the model asked for.
    pub path: String,
    /// Diagnostic reported by the Refs layer.
    pub reason: String,
}

/// Outcome of applying context requests to ContextManager.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextApplicationOutcome {
    pub added: Vec<String>,
    pub missing: Vec<String>,
    pub already_satisfied: Vec<String>,
    pub limit_exceeded: Option<ContextLimitExceeded>,
    pub refs_rejected: Vec<RefsRejection>,
}

/// Reconciles the auto context layer during Discovery (pure append-only growth).
///
/// Requested files are added to the Auto layer or have their access upgraded,
/// while an external reference document is materialized into the Refs layer. The
/// `files_token_budget` limit is evaluated against the total estimated tokens of
/// current context files plus candidate additions.
///
/// A candidate is admitted either because Git tracks it or because it already
/// exists on disk inside one of the harness-owned sandboxes: `.tauqe/mockups/`
/// holds the throwaway prototypes earlier turns wrote, and `.tauqe/cache/refs/`
/// holds the external snapshots the reference registry materialized. Both trees
/// are Git-ignored by construction, so the tracked file list never names them,
/// and demanding a tracked entry would make a mockup unreachable for the very
/// turn that must rewrite it and a snapshot unreachable through its relative
/// link.
///
/// A snapshot is read-only by construction and enters the dedicated Refs layer
/// through [`ContextManager::add_refs_file`], which applies its own LRU eviction
/// against `refs_token_budget`; a document that alone exceeds that budget is
/// reported back instead of widening the context. Requesting an already loaded
/// snapshot refreshes its LRU watermark only.
pub fn reconcile_discovery_context(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
    files_token_budget: usize,
    refs_token_budget: usize,
) -> ContextApplicationOutcome {
    let mut outcome = ContextApplicationOutcome::default();

    // 1. Identify candidate additions and validate against the repository
    // Each candidate keeps the access it will be rendered with, because the
    // length of the `access` attribute belongs to the token price of its
    // `<file>` wrapper.
    let mut valid_new_files: Vec<(String, ContextAccess)> = Vec::new();

    for request in requests {
        let Ok(path) = context_manager.normalize_path(&request.path) else {
            if !outcome.missing.contains(&request.path) {
                outcome.missing.push(request.path.clone());
            }
            continue;
        };
        // A snapshot is read-only by construction, so a request for write
        // access is coerced instead of trusted.
        let requested_access = if refs::is_refs_cache_path(&path) {
            ContextAccess::ReadOnly
        } else {
            request.access
        };
        if !admits_discovery_path(context_manager, &path, available_files) {
            if !outcome.missing.contains(&path) {
                outcome.missing.push(path);
            }
            continue;
        }
        if access_satisfies(
            context_manager.effective_access_for(&path),
            requested_access,
        ) {
            // A repeated request for a loaded snapshot refreshes the watermark
            // that decides which document the Refs layer evicts first.
            if refs::is_refs_cache_path(&path) {
                context_manager.touch_refs_lru(&path);
            }
            if !outcome.already_satisfied.contains(&path) {
                outcome.already_satisfied.push(path);
            }
            continue;
        }
        if !context_manager.contains(&path).unwrap_or(false) {
            // A path requested twice is priced once. The editable spelling wins,
            // because that is the attribute the element renders after the
            // upgrade, so the wrapper is measured at its widest.
            if let Some(entry) = valid_new_files
                .iter_mut()
                .find(|entry| entry.0.as_str() == path.as_str())
            {
                if requested_access == ContextAccess::Editable {
                    entry.1 = ContextAccess::Editable;
                }
            } else {
                valid_new_files.push((path, requested_access));
            }
        }
    }

    // 2. Check files_token_budget limit
    // Token estimates rely on cached context sizes and file metadata instead of
    // reading full file contents, avoiding heavy blocking I/O on the async worker.
    // Both sides of the comparison are priced the same way. A file already in
    // context pays its content plus the wrapper its own `<file>` element
    // renders, so the budget never under-counts the material the model holds.
    let current_tokens: usize = context_manager
        .get_state()
        .items
        .iter()
        .map(|item| {
            item.estimated_tokens as usize
                + crate::prompt::context_element_overhead_tokens(&item.path, item.access)
        })
        .sum();

    // A candidate costs its content plus the same wrapper. Both terms come from
    // the builder that renders the `<context>` block, so the budget never
    // measures less than the model reads.
    let mut new_tokens = 0usize;
    for (path, access) in &valid_new_files {
        let full_path = context_manager.repo_root().join(path);
        if let Ok(meta) = std::fs::metadata(&full_path) {
            new_tokens += crate::prompt::estimate_tokens_from_bytes(meta.len()) as usize
                + crate::prompt::context_element_overhead_tokens(path, *access);
        }
    }

    if current_tokens + new_tokens > files_token_budget {
        outcome.limit_exceeded = Some(ContextLimitExceeded {
            current_tokens,
            requested_tokens: new_tokens,
            token_budget: files_token_budget,
            requested_files: valid_new_files
                .iter()
                .map(|(path, _)| path.clone())
                .collect(),
        });
        return outcome;
    }

    // 3. Apply additions and access upgrades
    for request in requests {
        let Ok(path) = context_manager.normalize_path(&request.path) else {
            continue;
        };
        if outcome.missing.contains(&path) || outcome.already_satisfied.contains(&path) {
            continue;
        }
        if refs::is_refs_cache_path(&path) {
            match context_manager.add_refs_file(&path, refs_token_budget) {
                Ok(_) => {
                    if !outcome.added.contains(&path) {
                        outcome.added.push(path);
                    }
                }
                Err(err) => {
                    if !outcome.refs_rejected.iter().any(|entry| entry.path == path) {
                        outcome.refs_rejected.push(RefsRejection {
                            path,
                            reason: format!("{:#}", err),
                        });
                    }
                }
            }
            continue;
        }
        if context_manager.add_auto_file(&path, request.access).is_ok()
            && access_satisfies(context_manager.effective_access_for(&path), request.access)
            && !outcome.added.contains(&path)
        {
            outcome.added.push(path);
        }
    }

    outcome
}

/// Reports whether a requested repository path may enter the context.
///
/// A tracked file is always admissible. A harness-owned sandbox path is
/// admissible when it already exists on disk, because Git never tracks it: both
/// sandbox trees are ignored by construction.
fn admits_discovery_path(
    context_manager: &ContextManager,
    path: &str,
    available_files: &[String],
) -> bool {
    if available_files
        .iter()
        .any(|candidate| candidate.as_str() == path)
    {
        return true;
    }
    if context_manager.contains(path).unwrap_or(false) {
        return true;
    }
    let sandboxed = mockups::is_mockups_path(path) || refs::is_refs_cache_path(path);
    sandboxed && context_manager.repo_root().join(path).is_file()
}
/// Registers model-requested repository files in the Auto layer with detailed diagnostics.
/// Enforces the hard `files_token_budget` limit across all context layers and
/// routes external reference snapshots into the Refs layer under its own budget.
pub fn apply_context_requests_detailed(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
    files_token_budget: usize,
    refs_token_budget: usize,
) -> ContextApplicationOutcome {
    reconcile_discovery_context(
        context_manager,
        requests,
        available_files,
        files_token_budget,
        refs_token_budget,
    )
}

/// Registers model-requested repository files in the Auto layer.
/// Returns the paths that actually widened the effective context.
pub fn apply_context_requests(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
    files_token_budget: usize,
    refs_token_budget: usize,
) -> Vec<String> {
    apply_context_requests_detailed(
        context_manager,
        requests,
        available_files,
        files_token_budget,
        refs_token_budget,
    )
    .added
}

/// Immutable inputs shared by every Discovery capability handler.
///
/// The dispatcher resolves repository facts once per round and hands them to
/// every handler, so a handler never performs its own file listing and never
/// re-derives the same budget.
pub struct DiscoveryRoundContext<'a> {
    /// Repository root used for path validation and file scans.
    pub repo_root: &'a Path,
    /// Repository-relative files known to exist in the working tree.
    pub available_files: &'a [String],
    /// Hard token budget for context file additions.
    pub files_token_budget: usize,
    /// Token budget for a targeted repository map rendered this round.
    pub repomap_token_budget: usize,
    /// Operational caps resolved from the project configuration.
    pub limits: DiscoveryLimits,
    /// Whether a request already satisfied by the context counts as progress.
    pub allow_already_satisfied_feedback: bool,
}

/// Accumulated effect of one Discovery round.
///
/// Every handler folds its own findings into this value, so a turn loop
/// interprets a round through one uniform structure instead of branching on
/// individual request types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryRoundOutcome {
    /// Set when context additions would breach the files token budget.
    pub limit_exceeded: Option<ContextLimitExceeded>,
    /// Files that widened the effective context this round.
    pub added_files: Vec<String>,
    /// Requested files that do not exist in the repository.
    pub missing_files: Vec<String>,
    /// Requested files whose current access already satisfied the request.
    pub already_satisfied_files: Vec<String>,
    /// Reference documents the Refs layer refused, with the reported reason.
    pub refs_rejected: Vec<RefsRejection>,
    /// Repository searches executed this round, with their matches.
    pub searches_executed: Vec<RepoSearchResult>,
    /// Documentation topics loaded this round.
    pub doc_topics_loaded: Vec<DocTopic>,
    /// Repository directory trees rendered this round.
    pub dir_trees_loaded: usize,
    /// Directory listings rendered this round.
    pub dir_lists_loaded: usize,
    /// Targeted repository maps rendered this round.
    pub repo_maps_loaded: usize,
    /// Feedback blocks to inject into the next proposal round.
    pub feedback_blocks: Vec<String>,
}

impl DiscoveryRoundOutcome {
    /// Reports whether the round changed what the model can see or taught it
    /// something it has not been told yet.
    ///
    /// `allow_already_satisfied` treats a round that only re-requested loaded
    /// files as progress. The architect loop needs that: the model still
    /// deserves the explanation that those files are already visible, so such a
    /// round must not be mistaken for a dead end.
    pub fn has_progress(&self, allow_already_satisfied: bool) -> bool {
        !self.added_files.is_empty()
            || !self.missing_files.is_empty()
            || !self.searches_executed.is_empty()
            || self.dir_trees_loaded > 0
            || self.dir_lists_loaded > 0
            || self.repo_maps_loaded > 0
            || !self.doc_topics_loaded.is_empty()
            || (allow_already_satisfied && !self.already_satisfied_files.is_empty())
            || !self.refs_rejected.is_empty()
    }

    /// Joins the round's feedback blocks into one prompt fragment.
    pub fn combined_feedback(&self) -> String {
        self.feedback_blocks
            .iter()
            .map(|block| block.trim())
            .filter(|block| !block.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// One-line round summary for the FSM discovery banner.
    pub fn fsm_banner_summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.added_files.is_empty() {
            parts.push(format!("{} file(s) added", self.added_files.len()));
        }
        if !self.searches_executed.is_empty() {
            parts.push(format!(
                "{} search(es) executed",
                self.searches_executed.len()
            ));
        }
        if self.dir_trees_loaded > 0 {
            parts.push(format!(
                "{} directory tree(s) rendered",
                self.dir_trees_loaded
            ));
        }
        if self.dir_lists_loaded > 0 {
            parts.push(format!(
                "{} directory listing(s) rendered",
                self.dir_lists_loaded
            ));
        }
        if self.repo_maps_loaded > 0 {
            parts.push(format!(
                "{} targeted map(s) rendered",
                self.repo_maps_loaded
            ));
        }
        if !self.doc_topics_loaded.is_empty() {
            parts.push(format!(
                "{} doc topic(s) loaded",
                self.doc_topics_loaded.len()
            ));
        }
        if parts.is_empty() {
            format!("{} file(s) added.", self.added_files.len())
        } else {
            format!("{}.", parts.join(", "))
        }
    }

    /// Detailed round summary for the architect discovery banner.
    pub fn architect_banner_summary(&self) -> String {
        let mut notes = Vec::new();
        if !self.added_files.is_empty() {
            notes.push(format!(
                "Added {} file(s) to context: {}",
                self.added_files.len(),
                self.added_files.join(", ")
            ));
        }
        if !self.searches_executed.is_empty() {
            notes.push(format!(
                "Executed {} repository search(es)",
                self.searches_executed.len()
            ));
        }
        if self.dir_trees_loaded > 0 {
            notes.push("Rendered the repository directory tree".to_string());
        }
        if self.dir_lists_loaded > 0 {
            notes.push(format!(
                "Rendered {} directory listing(s)",
                self.dir_lists_loaded
            ));
        }
        if self.repo_maps_loaded > 0 {
            notes.push(format!(
                "Rendered {} targeted repository map(s)",
                self.repo_maps_loaded
            ));
        }
        if !self.missing_files.is_empty() {
            notes.push(format!(
                "Files not found in repository: {}",
                self.missing_files.join(", ")
            ));
        }
        if self.added_files.is_empty()
            && self.searches_executed.is_empty()
            && !self.already_satisfied_files.is_empty()
        {
            notes.push(format!(
                "Files already in context: {}",
                self.already_satisfied_files.join(", ")
            ));
        }
        notes.join("; ")
    }
}

/// One autonomous executor for a Discovery capability.
///
/// A handler owns the validation, disk work, and feedback text of its own
/// capability, so turn loops dispatch Discovery commands polymorphically instead
/// of branching on command types.
pub trait DiscoveryCapabilityHandler: Send + Sync {
    /// Capability realized by this handler.
    fn capability(&self) -> LlmCapability;

    /// Reports whether the handler executes the given command.
    fn handles(&self, command: &LlmCommand) -> bool;

    /// Executes every command the handler owns, folding results into `outcome`.
    fn execute(
        &self,
        round: &DiscoveryRoundContext<'_>,
        commands: &[LlmCommand],
        context_manager: &mut ContextManager,
        outcome: &mut DiscoveryRoundOutcome,
    );
}

/// Executes context-file requests under the files token budget.
struct ContextRequestHandler;

impl DiscoveryCapabilityHandler for ContextRequestHandler {
    fn capability(&self) -> LlmCapability {
        LlmCapability::Discovery
    }

    fn handles(&self, command: &LlmCommand) -> bool {
        matches!(command, LlmCommand::ContextRequest { .. })
    }

    fn execute(
        &self,
        round: &DiscoveryRoundContext<'_>,
        commands: &[LlmCommand],
        context_manager: &mut ContextManager,
        outcome: &mut DiscoveryRoundOutcome,
    ) {
        let requests: Vec<ContextRequest> = commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::ContextRequest { path, access } => Some(ContextRequest {
                    path: path.clone(),
                    access: *access,
                }),
                _ => None,
            })
            .collect();
        if requests.is_empty() {
            return;
        }

        let applied = reconcile_discovery_context(
            context_manager,
            &requests,
            round.available_files,
            round.files_token_budget,
            round.limits.refs_token_budget,
        );
        outcome.added_files.extend(applied.added);
        outcome.missing_files.extend(applied.missing);
        outcome
            .already_satisfied_files
            .extend(applied.already_satisfied);
        outcome.limit_exceeded = applied.limit_exceeded;

        if !outcome.missing_files.is_empty() {
            outcome.feedback_blocks.push(format!(
                "<context_feedback>\nThe following requested files do not exist in the repository: [{}]\nPlease check <repo_map> for exact existing file paths.\n</context_feedback>",
                outcome.missing_files.join(", ")
            ));
        }
        if !applied.refs_rejected.is_empty() {
            // A refused snapshot is actionable knowledge, not a dead end: the
            // model learns that the declared document cannot widen the context.
            let rejected = applied
                .refs_rejected
                .iter()
                .map(|entry| format!("{}: {}", entry.path, entry.reason))
                .collect::<Vec<_>>()
                .join("; ");
            outcome.feedback_blocks.push(format!(
                "<refs_feedback>\nThe following requested reference documents could not be loaded: [{}]\nRaise `[context].refs_token_budget` in the project configuration when the budget itself is the obstacle, and request a narrower section otherwise.\n</refs_feedback>",
                rejected
            ));
            outcome.refs_rejected.extend(applied.refs_rejected);
        }
    }
}

/// Executes repository regex searches and formats their feedback.
struct RepoSearchHandler;

impl DiscoveryCapabilityHandler for RepoSearchHandler {
    fn capability(&self) -> LlmCapability {
        LlmCapability::Discovery
    }

    fn handles(&self, command: &LlmCommand) -> bool {
        matches!(command, LlmCommand::RepoSearch { .. })
    }

    fn execute(
        &self,
        round: &DiscoveryRoundContext<'_>,
        commands: &[LlmCommand],
        _context_manager: &mut ContextManager,
        outcome: &mut DiscoveryRoundOutcome,
    ) {
        let queries: Vec<RepoSearchRequest> = commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::RepoSearch { path, regex } => {
                    Some(RepoSearchRequest::new(path.clone(), regex.clone()))
                }
                _ => None,
            })
            .collect();
        if queries.is_empty() {
            return;
        }

        let results = execute_repo_searches(
            round.repo_root,
            &queries,
            round.available_files,
            round.limits,
        );
        if !results.is_empty() {
            outcome
                .feedback_blocks
                .push(format_search_feedback(&results));
        }
        outcome.searches_executed.extend(results);
    }
}

/// Renders the compact repository directory tree on demand.
struct RepoDirsHandler;

impl DiscoveryCapabilityHandler for RepoDirsHandler {
    fn capability(&self) -> LlmCapability {
        LlmCapability::Discovery
    }

    fn handles(&self, command: &LlmCommand) -> bool {
        matches!(command, LlmCommand::RepoDirs)
    }

    fn execute(
        &self,
        round: &DiscoveryRoundContext<'_>,
        commands: &[LlmCommand],
        context_manager: &mut ContextManager,
        outcome: &mut DiscoveryRoundOutcome,
    ) {
        if !commands
            .iter()
            .any(|command| matches!(command, LlmCommand::RepoDirs))
        {
            return;
        }

        // The outline is derived deterministically from the tracked file list,
        // so the model reads the repository layout instead of guessing it. Files
        // anchored by the pinned context layer are marked, so the model knows
        // that authoritative conventions already travelled with the prompt.
        let tree = crate::repomap::dirs::generate_repo_dirs_with_pinned(
            round.available_files,
            context_manager.pinned_patterns(),
        );
        let body = if tree.trim().is_empty() {
            "The repository contains no subdirectories.".to_string()
        } else {
            tree.trim_end().to_string()
        };
        outcome.feedback_blocks.push(format!(
            "<repo_dirs_feedback>\n{}\n</repo_dirs_feedback>",
            body
        ));
        outcome.dir_trees_loaded += 1;
    }
}

/// Renders the targeted symbol map of a requested subdirectory.
struct RepoMapRequestHandler;

impl DiscoveryCapabilityHandler for RepoMapRequestHandler {
    fn capability(&self) -> LlmCapability {
        LlmCapability::Discovery
    }

    fn handles(&self, command: &LlmCommand) -> bool {
        matches!(command, LlmCommand::RepoMap { .. })
    }

    fn execute(
        &self,
        round: &DiscoveryRoundContext<'_>,
        commands: &[LlmCommand],
        _context_manager: &mut ContextManager,
        outcome: &mut DiscoveryRoundOutcome,
    ) {
        let max_requests = round.limits.max_repo_map_requests_per_round;
        let mut requested: Vec<String> = Vec::new();
        for command in commands {
            let LlmCommand::RepoMap { path } = command else {
                continue;
            };
            let target = path.trim().to_string();
            if requested.contains(&target) {
                continue;
            }
            if requested.len() >= max_requests {
                tracing::warn!(
                    limit = max_requests,
                    "targeted repository map request limit reached; ignoring the rest"
                );
                break;
            }
            requested.push(target);
        }

        for target in requested {
            let display_path = if target.is_empty() {
                ".".to_string()
            } else {
                target.clone()
            };
            // The outline is derived deterministically from the tracked file list
            // and the Tree-sitter symbols under the requested prefix, so the model
            // reads the subsystem's real contour instead of guessing it.
            let map = match crate::repomap::generate_targeted_repo_map(
                round.repo_root,
                round.available_files,
                &target,
                round.repomap_token_budget,
            ) {
                Ok(map) => map,
                Err(err) => {
                    tracing::warn!(
                        path = display_path.as_str(),
                        error = %err,
                        "targeted repository map generation failed"
                    );
                    String::new()
                }
            };
            let body = if map.trim().is_empty() {
                format!(
                    "No tracked source files under `{}`. Check <repo_dirs> for existing paths.",
                    display_path
                )
            } else {
                map.trim_end().to_string()
            };
            outcome.feedback_blocks.push(format!(
                "<repomap_feedback path=\"{}\">\n{}\n</repomap_feedback>",
                display_path, body
            ));
            outcome.repo_maps_loaded += 1;
        }
    }
}

/// Lists the tracked files of a requested directory.
struct DirListHandler;

impl DiscoveryCapabilityHandler for DirListHandler {
    fn capability(&self) -> LlmCapability {
        LlmCapability::Discovery
    }

    fn handles(&self, command: &LlmCommand) -> bool {
        matches!(command, LlmCommand::DirList { .. })
    }

    fn execute(
        &self,
        round: &DiscoveryRoundContext<'_>,
        commands: &[LlmCommand],
        _context_manager: &mut ContextManager,
        outcome: &mut DiscoveryRoundOutcome,
    ) {
        let max_entries = round.limits.max_dir_list_entries;
        let mut requested: Vec<DirListRequest> = Vec::new();
        for command in commands {
            let LlmCommand::DirList { path, recursive } = command else {
                continue;
            };
            let Some(request) = DirListRequest::new(path.clone(), *recursive).normalized() else {
                tracing::warn!(
                    path = path.as_str(),
                    "invalid directory listing request ignored"
                );
                continue;
            };
            if requested
                .iter()
                .any(|existing| existing.path == request.path)
            {
                continue;
            }
            requested.push(request);
        }

        for request in requested {
            let display_path = if request.path.is_empty() {
                ".".to_string()
            } else {
                request.path.clone()
            };
            // The listing is derived deterministically from the tracked file
            // set, so the model reads the directory contents instead of
            // guessing which files exist.
            let mut entries =
                render_dir_listing(round.available_files, &request.path, request.recursive);
            // A listing is a navigation aid, not a file dump: a recursive
            // request over a wide tree would otherwise consume the whole context
            // budget, so the configured cap is applied and reported explicitly.
            let omitted = entries.len().saturating_sub(max_entries);
            if omitted > 0 {
                entries.truncate(max_entries);
            }
            let mut body = if entries.is_empty() {
                format!(
                    "No tracked files under `{}`. Check <repo_dirs> for existing paths.",
                    display_path
                )
            } else {
                entries.join("\n")
            };
            if omitted > 0 {
                body.push_str(&format!(
                    "\n[{} more entries omitted: a listing is capped at {} entries]",
                    omitted, max_entries
                ));
            }
            outcome.feedback_blocks.push(format!(
                "<dir_list_feedback path=\"{}\" recursive=\"{}\">\n{}\n</dir_list_feedback>",
                display_path, request.recursive, body
            ));
            outcome.dir_lists_loaded += 1;
        }
    }
}

/// Renders the tracked files under a directory prefix.
///
/// A shallow listing reports the direct children of the directory and marks
/// subdirectories with a trailing slash; a recursive listing reports every
/// tracked file underneath. Entries are sorted and deduplicated, so the same
/// request always produces the same block.
fn render_dir_listing(available_files: &[String], target: &str, recursive: bool) -> Vec<String> {
    let prefix = if target.is_empty() {
        String::new()
    } else {
        format!("{}/", target)
    };
    let mut entries = BTreeSet::new();
    for file in available_files {
        // Excluded paths never appear in a listing, exactly as they never
        // appear in search results or in the directory tree.
        if search::is_excluded_path(file) {
            continue;
        }
        let Some(remainder) = file.strip_prefix(prefix.as_str()) else {
            continue;
        };
        if recursive {
            entries.insert(remainder.to_string());
        } else if let Some((subdirectory, _)) = remainder.split_once('/') {
            entries.insert(format!("{}/", subdirectory));
        } else {
            entries.insert(remainder.to_string());
        }
    }
    entries.into_iter().collect()
}

/// Loads authoritative documentation chapters for requested topics.
struct DocRequestHandler;

impl DiscoveryCapabilityHandler for DocRequestHandler {
    fn capability(&self) -> LlmCapability {
        LlmCapability::DocRequest
    }

    fn handles(&self, command: &LlmCommand) -> bool {
        matches!(command, LlmCommand::DocRequest { .. })
    }

    fn execute(
        &self,
        _round: &DiscoveryRoundContext<'_>,
        commands: &[LlmCommand],
        _context_manager: &mut ContextManager,
        outcome: &mut DiscoveryRoundOutcome,
    ) {
        let topics: Vec<DocTopic> = commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::DocRequest { topic } => Some(*topic),
                _ => None,
            })
            .collect();
        if topics.is_empty() {
            return;
        }

        let documentation = topics
            .iter()
            .map(|topic| crate::docs::get_documentation(*topic))
            .collect::<Vec<_>>()
            .join("\n\n");
        if !documentation.trim().is_empty() {
            outcome.feedback_blocks.push(format!(
                "<system_documentation>\n{}\n</system_documentation>",
                documentation.trim()
            ));
        }
        outcome.doc_topics_loaded.extend(topics);
    }
}

/// Registered Discovery handlers, in dispatch order.
static DISCOVERY_HANDLERS: &[&dyn DiscoveryCapabilityHandler] = &[
    &ContextRequestHandler,
    &RepoSearchHandler,
    &RepoDirsHandler,
    &RepoMapRequestHandler,
    &DirListHandler,
    &DocRequestHandler,
];

/// Returns the registered Discovery capability handlers in dispatch order.
pub fn discovery_handlers() -> &'static [&'static dyn DiscoveryCapabilityHandler] {
    DISCOVERY_HANDLERS
}

/// Executes Discovery commands synchronously against an already resolved
/// repository file list.
///
/// This is the core the asynchronous facade and the server architect loop call
/// from inside `run_blocking` / `with_context_mutation`, so no handler ever
/// blocks an async worker thread. The `limits` caps travel with the call, so a
/// round applies exactly the bounds the project declared.
#[allow(clippy::too_many_arguments)]
pub fn execute_discovery_commands_sync(
    context_manager: &mut ContextManager,
    repo_root: &Path,
    available_files: &[String],
    commands: &[LlmCommand],
    files_token_budget: usize,
    repomap_token_budget: usize,
    limits: DiscoveryLimits,
    allow_already_satisfied_feedback: bool,
) -> DiscoveryRoundOutcome {
    let round = DiscoveryRoundContext {
        repo_root,
        available_files,
        files_token_budget,
        repomap_token_budget,
        limits,
        allow_already_satisfied_feedback,
    };
    let mut outcome = DiscoveryRoundOutcome::default();

    for handler in discovery_handlers() {
        if !commands.iter().any(|command| handler.handles(command)) {
            continue;
        }
        handler.execute(&round, commands, context_manager, &mut outcome);
        // A breached budget rejects the whole addition set, so later
        // capabilities must not run against a context that was never widened.
        if outcome.limit_exceeded.is_some() {
            return outcome;
        }
    }

    // The already-satisfied note depends on what the whole round did: it only
    // explains a round that added nothing and searched nothing.
    if round.allow_already_satisfied_feedback
        && outcome.added_files.is_empty()
        && outcome.searches_executed.is_empty()
        && !outcome.already_satisfied_files.is_empty()
    {
        outcome.feedback_blocks.push(format!(
            "<context_feedback>\nThe following files are already loaded in context: [{}]\nYou may proceed with inspecting them and refining the plan. Do not request those paths again: answer the task from the material you already hold, and write your analysis or conclusion in plain conversational text unless the code itself must change.\n</context_feedback>",
            outcome.already_satisfied_files.join(", ")
        ));
    }

    outcome
}

/// Resolves the tracked repository file list off the async executor thread.
///
/// Every failure — a `git` invocation that cannot run and a panicked or
/// cancelled offloaded task alike — is recorded before the caller degrades,
/// so an uninspectable repository stays distinguishable from an empty one.
/// Returns `None` when the list could not be resolved.
pub async fn resolve_repository_files(repo_root: &Path) -> Option<Vec<String>> {
    let root = repo_root.to_path_buf();
    match run_blocking_async(move || crate::git::list_repository_files(Some(&root))).await {
        Ok(Ok(files)) => Some(files),
        Ok(Err(error)) => {
            tracing::warn!(
                error = %error,
                "failed to list repository files; the repository is treated as uninspectable"
            );
            None
        }
        Err(join_error) => {
            crate::blocking::log_join_error(&join_error, "repository file listing");
            None
        }
    }
}

/// Round outcome returned when the tracked repository file list could not be
/// resolved.
///
/// The round must not dispatch handlers against an invented empty repository:
/// every requested file would otherwise surface to the model as missing. The
/// feedback block names the inspection failure instead, and the warn record
/// of [`resolve_repository_files`] keeps the degradation observable.
fn uninspectable_repository_outcome() -> DiscoveryRoundOutcome {
    let mut outcome = DiscoveryRoundOutcome::default();
    outcome.feedback_blocks.push(
        "<context_feedback>\nThe repository could not be inspected, so the tracked \
         file list and repository searches are unavailable for this round. Do not \
         conclude that requested files are missing: proceed with the material \
         already loaded in context.\n</context_feedback>"
            .to_string(),
    );
    outcome
}

/// Executes Discovery commands asynchronously, resolving repository facts off
/// the async executor thread.
///
/// The repository file list is read once per round, and every handler runs
/// inside `run_blocking`, so path canonicalization, file metadata reads, and
/// regex scans never stall a Tokio worker. A repository that cannot be
/// inspected ends the round with an inspection-failure feedback block instead
/// of reporting every requested file as missing.
///
/// A request that names a declared reference is materialized into the isolated
/// cache before the blocking phase, so the round widens the context with the
/// snapshot the registry declares instead of reporting a file that does not
/// exist on disk. The optional cancel signal is the turn's own, so an abandoned
/// round stops waiting for a slow reference host, and the diagnostics of a
/// refused materialization lead the round's feedback.
#[allow(clippy::too_many_arguments)]
pub async fn execute_discovery_commands(
    context_manager: &mut ContextManager,
    repo_root: &Path,
    commands: &[LlmCommand],
    files_token_budget: usize,
    repomap_token_budget: usize,
    limits: DiscoveryLimits,
    allow_already_satisfied_feedback: bool,
    cancel_rx: Option<&tokio::sync::watch::Receiver<bool>>,
) -> DiscoveryRoundOutcome {
    if commands.is_empty() {
        return DiscoveryRoundOutcome::default();
    }

    // A declared reference has no file on disk until it is materialized, so the
    // registry requests are resolved into their isolated snapshots first. The
    // pre-step may fetch a remote document, so it runs before the blocking
    // phase that applies the round.
    let (commands, refs_feedback) = resolve_refs_requests(repo_root, commands, cancel_rx).await;

    let needs_file_list = commands.iter().any(|command| {
        matches!(
            command,
            LlmCommand::ContextRequest { .. }
                | LlmCommand::RepoSearch { .. }
                | LlmCommand::RepoDirs
                | LlmCommand::RepoMap { .. }
                | LlmCommand::DirList { .. }
        )
    });
    let available_files = if needs_file_list {
        match resolve_repository_files(repo_root).await {
            Some(files) => files,
            // A repository that could not be inspected must not surface every
            // requested file as missing: the round reports the inspection
            // failure instead of dispatching handlers against an invented
            // empty repository.
            None => {
                let outcome = uninspectable_repository_outcome();
                return with_refs_feedback(outcome, refs_feedback);
            }
        }
    } else {
        Vec::new()
    };

    let outcome = run_blocking(|| {
        execute_discovery_commands_sync(
            context_manager,
            repo_root,
            &available_files,
            &commands,
            files_token_budget,
            repomap_token_budget,
            limits,
            allow_already_satisfied_feedback,
        )
    });
    with_refs_feedback(outcome, refs_feedback)
}

/// Resolves the declared reference requests of one round into their snapshots.
///
/// The pre-step answers one request per input in the order the round declared
/// them, so the command list is rebuilt position by position and every other
/// command keeps its place. A request the pre-step leaves alone — an existing
/// registry file, the manifest itself, an undeclared name, or a refused
/// materialization — therefore keeps its original path and is reported by the
/// round exactly as before.
async fn resolve_refs_requests(
    repo_root: &Path,
    commands: &[LlmCommand],
    cancel_rx: Option<&tokio::sync::watch::Receiver<bool>>,
) -> (Vec<LlmCommand>, Vec<String>) {
    let requests: Vec<ContextRequest> = commands
        .iter()
        .filter_map(|command| match command {
            LlmCommand::ContextRequest { path, access } => Some(ContextRequest {
                path: path.clone(),
                access: *access,
            }),
            _ => None,
        })
        .collect();
    if requests.is_empty() {
        return (commands.to_vec(), Vec::new());
    }

    let (resolved, feedback) =
        crate::refs::materialize::materialize_requests(repo_root, &requests, cancel_rx).await;
    let mut substituted = Vec::with_capacity(commands.len());
    let mut resolved = resolved.into_iter();
    for command in commands {
        let LlmCommand::ContextRequest { .. } = command else {
            substituted.push(command.clone());
            continue;
        };
        let Some(request) = resolved.next() else {
            substituted.push(command.clone());
            continue;
        };
        substituted.push(LlmCommand::ContextRequest {
            path: request.path,
            access: request.access,
        });
    }

    (substituted, feedback)
}

/// Prepends the reference diagnostics to a round outcome.
///
/// A refused materialization is what the round discovered first, so its
/// diagnostic leads the feedback the handlers contribute afterwards.
fn with_refs_feedback(
    mut outcome: DiscoveryRoundOutcome,
    refs_feedback: Vec<String>,
) -> DiscoveryRoundOutcome {
    if refs_feedback.is_empty() {
        return outcome;
    }
    let mut blocks = refs_feedback;
    blocks.append(&mut outcome.feedback_blocks);
    outcome.feedback_blocks = blocks;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discovery caps applied by tests that do not exercise a specific limit.
    fn default_limits() -> DiscoveryLimits {
        DiscoveryLimits::default()
    }

    #[test]
    fn test_apply_context_requests_adds_existing_files_to_auto_layer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["a.rs".to_string()];
        let requests = vec![
            ContextRequest {
                path: "a.rs".to_string(),
                access: ContextAccess::Editable,
            },
            ContextRequest {
                path: "missing.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        ];

        let added = apply_context_requests(&mut cm, &requests, &available, 300_000, 50_000);
        assert_eq!(added, vec!["a.rs".to_string()]);
        assert!(cm.is_editable("a.rs").unwrap());

        // Already satisfied: no further changes.
        assert!(apply_context_requests(&mut cm, &requests, &available, 300_000, 50_000).is_empty());

        cm.clear_auto();
        assert!(!cm.contains("a.rs").unwrap());
    }

    #[test]
    fn test_apply_context_requests_respects_token_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() { let x = 12345; }").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["a.rs".to_string()];
        let requests = vec![ContextRequest {
            path: "a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }];

        // Budget is only 2 tokens; file content is ~7 tokens.
        let outcome = apply_context_requests_detailed(&mut cm, &requests, &available, 2, 50_000);
        assert!(outcome.added.is_empty());
        let limit = outcome.limit_exceeded.expect("Expected limit_exceeded");
        assert_eq!(limit.token_budget, 2);
        assert!(limit.requested_tokens > 2);
    }

    #[test]
    fn test_the_budget_check_charges_the_file_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["a.rs".to_string()];
        let requests = vec![ContextRequest {
            path: "a.rs".to_string(),
            access: ContextAccess::Editable,
        }];

        let content_tokens = crate::prompt::estimate_tokens("fn a() {}\n");
        let wrapper =
            crate::prompt::context_element_overhead_tokens("a.rs", ContextAccess::Editable);
        assert!(wrapper > 0, "the rendered <file> element costs tokens");

        // A budget that fits the bare content but not the element the model
        // actually reads refuses the file and reports the priced cost.
        let outcome = apply_context_requests_detailed(
            &mut cm,
            &requests,
            &available,
            content_tokens + wrapper - 1,
            50_000,
        );
        let limit = outcome.limit_exceeded.expect("the wrapper must be charged");
        assert_eq!(limit.requested_tokens, content_tokens + wrapper);
        assert!(outcome.added.is_empty());

        // One token more admits the very same file.
        let outcome = apply_context_requests_detailed(
            &mut cm,
            &requests,
            &available,
            content_tokens + wrapper,
            50_000,
        );
        assert!(outcome.limit_exceeded.is_none());
        assert_eq!(outcome.added, vec!["a.rs".to_string()]);
    }

    #[test]
    fn test_the_budget_check_charges_the_wrapper_of_a_loaded_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loaded.rs"), "fn loaded() {}\n").unwrap();
        std::fs::write(dir.path().join("next.rs"), "fn next() {}\n").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        cm.add_file("loaded.rs", ContextAccess::ReadOnly).unwrap();
        let available = vec!["loaded.rs".to_string(), "next.rs".to_string()];
        let requests = vec![ContextRequest {
            path: "next.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }];

        let loaded_tokens = crate::prompt::estimate_tokens("fn loaded() {}\n")
            + crate::prompt::context_element_overhead_tokens("loaded.rs", ContextAccess::ReadOnly);
        let next_tokens = crate::prompt::estimate_tokens("fn next() {}\n")
            + crate::prompt::context_element_overhead_tokens("next.rs", ContextAccess::ReadOnly);

        // The file already in context is priced together with its own wrapper,
        // so the reported current cost exceeds its bare content.
        let outcome = apply_context_requests_detailed(
            &mut cm,
            &requests,
            &available,
            loaded_tokens + next_tokens - 1,
            50_000,
        );
        let limit = outcome.limit_exceeded.expect("the budget is exceeded");
        assert_eq!(limit.current_tokens, loaded_tokens);
        assert_eq!(limit.requested_tokens, next_tokens);

        let outcome = apply_context_requests_detailed(
            &mut cm,
            &requests,
            &available,
            loaded_tokens + next_tokens,
            50_000,
        );
        assert!(outcome.limit_exceeded.is_none());
        assert_eq!(outcome.added, vec!["next.rs".to_string()]);
    }

    #[test]
    fn test_apply_context_requests_upgrades_user_readonly_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("user.rs"), "fn user() {}").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        cm.add_file("user.rs", ContextAccess::ReadOnly).unwrap();
        assert!(!cm.is_editable("user.rs").unwrap());

        let available = vec!["user.rs".to_string()];
        let requests = vec![ContextRequest {
            path: "user.rs".to_string(),
            access: ContextAccess::Editable,
        }];
        let added = apply_context_requests(&mut cm, &requests, &available, 300_000, 50_000);
        assert_eq!(added, vec!["user.rs".to_string()]);
        assert!(cm.is_editable("user.rs").unwrap());
    }

    #[test]
    fn test_apply_context_requests_detailed_tracks_missing_and_satisfied() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        cm.add_file("a.rs", ContextAccess::ReadOnly).unwrap();

        let available = vec!["a.rs".to_string()];
        let requests = vec![
            ContextRequest {
                path: "a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
            ContextRequest {
                path: "missing.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        ];

        let outcome =
            apply_context_requests_detailed(&mut cm, &requests, &available, 300_000, 50_000);
        assert!(outcome.added.is_empty());
        assert_eq!(outcome.already_satisfied, vec!["a.rs".to_string()]);
        assert_eq!(outcome.missing, vec!["missing.rs".to_string()]);
    }

    fn sample_search_result() -> RepoSearchResult {
        RepoSearchResult {
            query: RepoSearchRequest::new("src", "needle"),
            matches: vec![FileSearchMatch {
                path: "src/a.rs".to_string(),
                count: 2,
            }],
            total_matched_files: 1,
            total_matches: 2,
            omitted_files: 0,
            error: None,
        }
    }

    #[test]
    fn test_round_outcome_banner_summaries() {
        let mut outcome = DiscoveryRoundOutcome::default();
        assert_eq!(outcome.fsm_banner_summary(), "0 file(s) added.");
        assert_eq!(outcome.architect_banner_summary(), "");

        outcome.added_files.push("a.rs".to_string());
        outcome.added_files.push("b.rs".to_string());
        outcome.missing_files.push("c.rs".to_string());
        outcome.searches_executed.push(sample_search_result());
        outcome.doc_topics_loaded.push(DocTopic::Workflow);

        assert_eq!(
            outcome.fsm_banner_summary(),
            "2 file(s) added, 1 search(es) executed, 1 doc topic(s) loaded."
        );
        assert_eq!(
            outcome.architect_banner_summary(),
            "Added 2 file(s) to context: a.rs, b.rs; Executed 1 repository search(es); \
             Files not found in repository: c.rs"
        );

        // The already-satisfied note only explains a round that did nothing else.
        let mut satisfied = DiscoveryRoundOutcome::default();
        satisfied.already_satisfied_files.push("a.rs".to_string());
        assert_eq!(
            satisfied.architect_banner_summary(),
            "Files already in context: a.rs"
        );
        satisfied.added_files.push("b.rs".to_string());
        assert_eq!(
            satisfied.architect_banner_summary(),
            "Added 1 file(s) to context: b.rs"
        );
    }

    #[test]
    fn test_execute_discovery_commands_dispatches_every_capability() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "pub fn beta() {}\n").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["a.rs".to_string(), "b.rs".to_string()];

        let commands = vec![
            LlmCommand::ContextRequest {
                path: "a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
            LlmCommand::ContextRequest {
                path: "missing.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
            LlmCommand::RepoSearch {
                path: String::new(),
                regex: "pub fn".to_string(),
            },
            LlmCommand::DocRequest {
                topic: DocTopic::Workflow,
            },
        ];

        let outcome = execute_discovery_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            300_000,
            300_000,
            default_limits(),
            false,
        );

        assert_eq!(outcome.added_files, vec!["a.rs".to_string()]);
        assert_eq!(outcome.missing_files, vec!["missing.rs".to_string()]);
        assert_eq!(outcome.searches_executed.len(), 1);
        assert_eq!(outcome.searches_executed[0].total_matched_files, 2);
        assert_eq!(outcome.doc_topics_loaded, vec![DocTopic::Workflow]);
        assert!(outcome.has_progress(false));
        assert!(cm.contains("a.rs").unwrap());

        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("<context_feedback>"));
        assert!(feedback.contains("<search_feedback path=\".\" regex=\"pub fn\">"));
        assert!(feedback.contains("<system_documentation>"));
    }

    #[test]
    fn test_execute_discovery_commands_stops_on_token_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("big.rs"),
            "pub fn big() { let x = 1234567; }\n",
        )
        .unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["big.rs".to_string()];

        let commands = vec![
            LlmCommand::ContextRequest {
                path: "big.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
            LlmCommand::DocRequest {
                topic: DocTopic::Workflow,
            },
        ];

        let outcome = execute_discovery_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            1,
            300_000,
            default_limits(),
            false,
        );

        let limit = outcome
            .limit_exceeded
            .as_ref()
            .expect("budget must be exceeded");
        assert_eq!(limit.token_budget, 1);
        assert!(outcome.added_files.is_empty());
        // The dispatcher halts before later capabilities run.
        assert!(outcome.doc_topics_loaded.is_empty());
        assert!(!outcome.has_progress(false));
    }

    #[test]
    fn test_execute_discovery_commands_already_satisfied_feedback_is_aggregate_aware() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn alpha() {}\n").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        cm.add_file("a.rs", ContextAccess::ReadOnly).unwrap();
        let available = vec!["a.rs".to_string()];
        let commands = vec![LlmCommand::ContextRequest {
            path: "a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }];

        let outcome = execute_discovery_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            300_000,
            300_000,
            default_limits(),
            false,
        );
        assert_eq!(outcome.already_satisfied_files, vec!["a.rs".to_string()]);
        assert!(!outcome.has_progress(false));
        assert!(outcome.combined_feedback().is_empty());

        let outcome = execute_discovery_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            300_000,
            300_000,
            default_limits(),
            true,
        );
        assert!(outcome.has_progress(true));
        assert!(outcome
            .combined_feedback()
            .contains("already loaded in context"));
    }

    #[test]
    fn test_execute_discovery_commands_renders_the_directory_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec![
            "src/main.rs".to_string(),
            "src/nested/module.rs".to_string(),
        ];

        let commands = vec![LlmCommand::RepoDirs];
        let outcome = execute_discovery_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            300_000,
            300_000,
            default_limits(),
            false,
        );

        assert_eq!(outcome.dir_trees_loaded, 1);
        assert!(outcome.has_progress(false));
        assert_eq!(
            outcome.fsm_banner_summary(),
            "1 directory tree(s) rendered."
        );
        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("<repo_dirs_feedback>"));
        assert!(feedback.contains("src\n  nested"));
    }

    #[test]
    fn test_execute_discovery_commands_lists_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec![
            "src/main.rs".to_string(),
            "src/nested/module.rs".to_string(),
            "README.md".to_string(),
        ];

        // A shallow listing reports the direct children of the directory.
        let commands = vec![LlmCommand::DirList {
            path: "src".to_string(),
            recursive: false,
        }];
        let outcome = execute_discovery_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            300_000,
            300_000,
            default_limits(),
            false,
        );
        assert_eq!(outcome.dir_lists_loaded, 1);
        assert!(outcome.has_progress(false));
        assert_eq!(
            outcome.fsm_banner_summary(),
            "1 directory listing(s) rendered."
        );
        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("<dir_list_feedback path=\"src\" recursive=\"false\">"));
        assert!(feedback.contains("main.rs"));
        assert!(feedback.contains("nested/"));
        assert!(!feedback.contains("README.md"));

        // A recursive listing reports every tracked file underneath.
        let commands = vec![LlmCommand::DirList {
            path: "src".to_string(),
            recursive: true,
        }];
        let outcome = execute_discovery_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            300_000,
            300_000,
            default_limits(),
            false,
        );
        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("<dir_list_feedback path=\"src\" recursive=\"true\">"));
        assert!(feedback.contains("main.rs"));
        assert!(feedback.contains("nested/module.rs"));
    }

    #[test]
    fn test_execute_discovery_commands_caps_a_listing_at_the_configured_bound() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available: Vec<String> = (0..5).map(|index| format!("src/file_{index}.rs")).collect();

        // The listing cap is configuration-driven, so a project raises or
        // narrows it in `tauqe.toml` instead of recompiling the engine.
        let limits = DiscoveryLimits {
            max_dir_list_entries: 2,
            ..default_limits()
        };
        let commands = vec![LlmCommand::DirList {
            path: "src".to_string(),
            recursive: true,
        }];
        let outcome = execute_discovery_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            300_000,
            300_000,
            limits,
            false,
        );

        assert_eq!(outcome.dir_lists_loaded, 1);
        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("file_0.rs"));
        assert!(feedback.contains("file_1.rs"));
        assert!(!feedback.contains("file_2.rs"));
        assert!(feedback.contains("[3 more entries omitted: a listing is capped at 2 entries]"));
    }

    #[test]
    fn test_execute_discovery_commands_ignores_commands_outside_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());

        let commands = vec![LlmCommand::Text {
            content: "no discovery work".to_string(),
        }];
        let outcome = execute_discovery_commands_sync(
            &mut cm,
            dir.path(),
            &[],
            &commands,
            300_000,
            300_000,
            default_limits(),
            true,
        );

        assert_eq!(outcome, DiscoveryRoundOutcome::default());
        assert!(!outcome.has_progress(true));
    }

    #[test]
    fn test_execute_discovery_commands_renders_a_targeted_repo_map() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        std::fs::write(dir.path().join("src/alpha.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join("src/nested/beta.rs"), "pub fn beta() {}\n").unwrap();
        std::fs::write(dir.path().join("other/gamma.rs"), "pub fn gamma() {}\n").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec![
            "src/alpha.rs".to_string(),
            "src/nested/beta.rs".to_string(),
            "other/gamma.rs".to_string(),
        ];

        // The same target requested twice renders exactly one block.
        let commands = vec![
            LlmCommand::RepoMap {
                path: "src".to_string(),
            },
            LlmCommand::RepoMap {
                path: "src".to_string(),
            },
        ];
        let outcome = execute_discovery_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            300_000,
            300_000,
            default_limits(),
            false,
        );

        assert_eq!(outcome.repo_maps_loaded, 1);
        assert!(outcome.has_progress(false));
        assert_eq!(outcome.fsm_banner_summary(), "1 targeted map(s) rendered.");

        let feedback = outcome.combined_feedback();
        assert_eq!(
            feedback.matches("<repomap_feedback path=\"src\">").count(),
            1
        );
        assert!(feedback.contains("pub fn alpha"));
        assert!(feedback.contains("pub fn beta"));
        assert!(!feedback.contains("other/gamma.rs"));
        assert!(!feedback.contains("pub fn gamma"));
    }

    #[test]
    fn test_execute_discovery_commands_reports_an_empty_targeted_map() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/alpha.rs"), "pub fn alpha() {}\n").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["src/alpha.rs".to_string()];

        let commands = vec![LlmCommand::RepoMap {
            path: "docs".to_string(),
        }];
        let outcome = execute_discovery_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            300_000,
            300_000,
            default_limits(),
            false,
        );

        assert_eq!(outcome.repo_maps_loaded, 1);
        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("<repomap_feedback path=\"docs\">"));
        assert!(feedback.contains("No tracked source files under `docs`"));
    }

    #[test]
    fn test_execute_discovery_commands_caps_targeted_repo_maps_per_round() {
        let dir = tempfile::tempdir().unwrap();
        let mut available = Vec::new();
        for index in 0..5 {
            let subdirectory = format!("module{index}");
            std::fs::create_dir_all(dir.path().join(&subdirectory)).unwrap();
            let file = format!("{subdirectory}/lib.rs");
            std::fs::write(
                dir.path().join(&file),
                format!("pub fn function_{index}() {{}}\n"),
            )
            .unwrap();
            available.push(file);
        }
        let mut cm = ContextManager::new(dir.path().to_path_buf());

        let commands: Vec<LlmCommand> = (0..5)
            .map(|index| LlmCommand::RepoMap {
                path: format!("module{index}"),
            })
            .collect();

        // The per-round request cap is configuration-driven: the shared handler
        // owns it, so an untrusted list of requested contours can never turn one
        // round into a full repository re-index.
        let limits = DiscoveryLimits {
            max_repo_map_requests_per_round: 2,
            ..default_limits()
        };
        let outcome = execute_discovery_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            300_000,
            300_000,
            limits,
            false,
        );

        assert_eq!(outcome.repo_maps_loaded, 2);
        let feedback = outcome.combined_feedback();
        assert_eq!(feedback.matches("<repomap_feedback path=").count(), 2);
        assert!(feedback.contains("pub fn function_0"));
        assert!(feedback.contains("pub fn function_1"));
        assert!(!feedback.contains("pub fn function_2"));
    }

    /// A repository whose tracked file list cannot be resolved must not lie to
    /// the model: the round reports the inspection failure as a feedback
    /// block, never as a list of "missing" files, and the degradation leaves
    /// a warn record.
    #[tokio::test]
    async fn uninspectable_repository_reports_feedback_instead_of_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing_root = dir.path().join("no-such-repository");
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let commands = vec![LlmCommand::ContextRequest {
            path: "a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }];

        let (buffer, _guard) = crate::blocking::log_capture::install();
        let outcome = execute_discovery_commands(
            &mut cm,
            &missing_root,
            &commands,
            300_000,
            300_000,
            default_limits(),
            false,
            None,
        )
        .await;

        assert!(outcome.added_files.is_empty());
        assert!(
            outcome.missing_files.is_empty(),
            "an uninspectable repository must not report requested files as missing"
        );
        assert!(!outcome.has_progress(false));
        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("The repository could not be inspected"));
        assert!(feedback.contains("Do not conclude that requested files are missing"));

        let logs = crate::blocking::log_capture::contents(&buffer);
        assert!(
            logs.contains("failed to list repository files"),
            "the degradation must be observable: {logs}"
        );
    }

    /// A mockup written by an earlier turn is Git-ignored, so the tracked file
    /// list never names it. Discovery must still admit it, otherwise the model
    /// could never rewrite the prototype it just created.
    #[test]
    fn test_a_sandbox_mockup_outside_the_tracked_list_is_admitted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let sandbox = root.join(crate::mockups::MOCKUPS_DIR);
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::write(sandbox.join("prototype.rs"), "pub fn draft() {}\n").unwrap();
        let mut cm = ContextManager::new(root);
        let mockup = format!("{}/prototype.rs", crate::mockups::MOCKUPS_DIR);
        let requests = vec![ContextRequest {
            path: mockup.clone(),
            access: ContextAccess::Editable,
        }];

        // The file is invisible to Git, so an empty tracked list proves the
        // sandbox rule is what admits it.
        let outcome = apply_context_requests_detailed(&mut cm, &requests, &[], 300_000, 50_000);

        assert_eq!(outcome.added, vec![mockup.clone()]);
        assert!(outcome.missing.is_empty());
        assert!(cm.is_editable(&mockup).unwrap());
    }

    /// A cached reference document is Git-ignored and read-only by
    /// construction: it lands in the dedicated Refs layer even when the model
    /// asks for write access.
    #[test]
    fn test_a_cached_reference_document_enters_the_refs_layer_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let cache = root.join(crate::refs::REFS_CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("api.md"), "upstream notes\n").unwrap();
        let mut cm = ContextManager::new(root);
        let cached = format!("{}/api.md", crate::refs::REFS_CACHE_DIR);
        let requests = vec![ContextRequest {
            path: cached.clone(),
            access: ContextAccess::Editable,
        }];

        let outcome = apply_context_requests_detailed(&mut cm, &requests, &[], 300_000, 50_000);

        assert_eq!(outcome.added, vec![cached.clone()]);
        assert_eq!(cm.refs_files(), vec![cached.clone()]);
        assert!(!cm.is_editable(&cached).unwrap());
        assert!(outcome.refs_rejected.is_empty());
    }

    /// Re-requesting a loaded snapshot refreshes its LRU watermark, so the
    /// document the dialogue keeps asking for outranks one requested earlier.
    #[test]
    fn test_a_repeated_reference_request_refreshes_the_eviction_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let cache = root.join(crate::refs::REFS_CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        for name in ["a.md", "b.md", "c.md"] {
            std::fs::write(cache.join(name), "reference notes\n").unwrap();
        }
        let mut cm = ContextManager::new(root);
        let cached = |name: &str| format!("{}/{name}", crate::refs::REFS_CACHE_DIR);
        let (first, second, third) = (cached("a.md"), cached("b.md"), cached("c.md"));
        let request = |path: &str| {
            vec![ContextRequest {
                path: path.to_string(),
                access: ContextAccess::ReadOnly,
            }]
        };

        apply_context_requests_detailed(&mut cm, &request(&first), &[], 300_000, 1_000_000);
        apply_context_requests_detailed(&mut cm, &request(&second), &[], 300_000, 1_000_000);
        let repeat =
            apply_context_requests_detailed(&mut cm, &request(&first), &[], 300_000, 1_000_000);
        assert_eq!(repeat.already_satisfied, vec![first.clone()]);

        // A budget that fits exactly two equal documents drops the least
        // recently requested one when the third arrives.
        let per_document = cm
            .get_state()
            .items
            .iter()
            .find(|item| item.path == first)
            .map(|item| item.estimated_tokens as usize)
            .expect("the first snapshot is loaded");
        let outcome = apply_context_requests_detailed(
            &mut cm,
            &request(&third),
            &[],
            300_000,
            2 * per_document,
        );

        assert_eq!(outcome.added, vec![third.clone()]);
        assert_eq!(cm.refs_files(), vec![first, third]);
    }

    /// A document larger than the whole Refs budget is refused with a
    /// diagnostic instead of evicting every other document and failing anyway.
    #[test]
    fn test_a_reference_document_larger_than_its_budget_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let cache = root.join(crate::refs::REFS_CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("huge.md"), "x".repeat(8192)).unwrap();
        let mut cm = ContextManager::new(root);
        let cached = format!("{}/huge.md", crate::refs::REFS_CACHE_DIR);
        let requests = vec![ContextRequest {
            path: cached.clone(),
            access: ContextAccess::ReadOnly,
        }];

        let outcome = apply_context_requests_detailed(&mut cm, &requests, &[], 300_000, 1);

        assert!(outcome.added.is_empty());
        assert_eq!(outcome.refs_rejected.len(), 1);
        assert_eq!(outcome.refs_rejected[0].path, cached);
        assert!(outcome.refs_rejected[0].reason.contains("refs budget"));
        assert!(cm.refs_files().is_empty());
    }

    /// A refused snapshot is reported back as round feedback instead of a dead
    /// end, because the model must learn that the declared document cannot
    /// widen the context.
    #[test]
    fn test_execute_discovery_commands_reports_a_refused_reference_document() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let cache = root.join(crate::refs::REFS_CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("huge.md"), "x".repeat(8192)).unwrap();
        let mut cm = ContextManager::new(root.clone());
        let cached = format!("{}/huge.md", crate::refs::REFS_CACHE_DIR);
        let commands = vec![LlmCommand::ContextRequest {
            path: cached.clone(),
            access: ContextAccess::ReadOnly,
        }];
        let limits = DiscoveryLimits {
            refs_token_budget: 1,
            ..default_limits()
        };

        let outcome = execute_discovery_commands_sync(
            &mut cm,
            &root,
            &[],
            &commands,
            300_000,
            300_000,
            limits,
            false,
        );

        assert!(outcome.added_files.is_empty());
        assert_eq!(outcome.refs_rejected.len(), 1);
        assert!(outcome.has_progress(false));
        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("<refs_feedback>"));
        assert!(feedback.contains(cached.as_str()));
        assert!(cm.refs_files().is_empty());
    }

    /// A reference the registry declares becomes visible in the round that asks
    /// for it: the pre-step materializes the snapshot, the rewritten request
    /// widens the context, and the snapshot enters the Refs layer read-only.
    #[tokio::test]
    async fn test_a_declared_reference_is_materialized_by_the_round_that_requests_it() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/llm.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string("upstream notes"))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&root), true).unwrap();
        let manifest = root.join(crate::refs::REFS_MANIFEST);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        let url = format!("{}/llm.txt", server.uri());
        let declared = format!("- [reference](vendor/api.md) = {url}\n");
        std::fs::write(&manifest, &declared).unwrap();

        let mut cm = ContextManager::new(root.clone());
        let commands = vec![LlmCommand::ContextRequest {
            path: ".tauqe/refs/vendor/api.md".to_string(),
            access: ContextAccess::ReadOnly,
        }];

        let outcome = execute_discovery_commands(
            &mut cm,
            &root,
            &commands,
            300_000,
            300_000,
            default_limits(),
            false,
            None,
        )
        .await;

        let cached = crate::refs::cache_path_for("vendor/api.md");
        assert_eq!(outcome.added_files, vec![cached.clone()]);
        assert!(outcome.missing_files.is_empty());
        assert!(outcome.has_progress(false));
        assert_eq!(cm.refs_files(), vec![cached.clone()]);
        assert!(!cm.is_editable(&cached).unwrap());
        assert_eq!(
            std::fs::read_to_string(root.join(&cached)).unwrap(),
            "upstream notes"
        );

        // A second round requesting the same name reuses the snapshot, so the
        // material the model reasoned about stays stable and no download repeats.
        let outcome = execute_discovery_commands(
            &mut cm,
            &root,
            &commands,
            300_000,
            300_000,
            default_limits(),
            false,
            None,
        )
        .await;
        assert!(outcome.added_files.is_empty());
        assert_eq!(outcome.already_satisfied_files, vec![cached]);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a materialized snapshot must not be downloaded again"
        );
    }

    /// A reference that cannot be materialized is reported first and still
    /// surfaces as missing, so the model learns why the declared document is
    /// unavailable instead of waiting for a file that never appears.
    #[tokio::test]
    async fn test_a_failed_reference_materialization_leads_the_round_feedback() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing.txt"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&root), true).unwrap();
        let manifest = root.join(crate::refs::REFS_MANIFEST);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        let url = format!("{}/missing.txt", server.uri());
        let declared = format!("- [missing](vendor/missing.md) = {url}\n");
        std::fs::write(&manifest, &declared).unwrap();

        let mut cm = ContextManager::new(root.clone());
        let requested = ".tauqe/refs/vendor/missing.md".to_string();
        let commands = vec![LlmCommand::ContextRequest {
            path: requested.clone(),
            access: ContextAccess::ReadOnly,
        }];

        let outcome = execute_discovery_commands(
            &mut cm,
            &root,
            &commands,
            300_000,
            300_000,
            default_limits(),
            false,
            None,
        )
        .await;

        assert!(outcome.added_files.is_empty());
        assert_eq!(outcome.missing_files, vec![requested]);
        let feedback = outcome.combined_feedback();
        assert!(
            feedback.starts_with("<refs_feedback>"),
            "the refusal must lead the round feedback: {feedback:?}"
        );
        assert!(feedback.contains("404"));
        assert!(cm.refs_files().is_empty());
    }
}
