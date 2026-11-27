//! Research subsystem of the bounded turn.
//!
//! The subsystem executes the investigation commands of one proposal round —
//! context requests, repository searches, directory outlines and listings, and
//! documentation — and reconciles the auto context layer those requests widen.
//! Every capability is a registered handler, so a turn loop dispatches through
//! the trait instead of branching on the command it decoded.

use crate::context::ContextAccess;
pub mod journal;
pub mod search;
pub mod step;

use std::collections::BTreeSet;
use std::path::Path;
use tauqe_tools::stop::HardCancellation;

use crate::llm::discussion::DirListRequest;
use crate::llm::discussion::RepoSearchRequest;
use crate::llm::DocTopic;
use crate::llm::LlmCommand;

use crate::blocking::{run_blocking, run_blocking_async};
use crate::config::ContextConfig;
use crate::context::ContextManager;
use crate::edits::protocol::ContextRequest;
use crate::mockups;
use crate::outer_docs;
use crate::outer_docs::MaterializeError;

pub(crate) use search::{execute_repo_searches, format_search_feedback, RepoSearchResult};

/// Operational caps applied to one Research round.
///
/// Every cap that bounds how much material a round may report is resolved from
/// the project's `[context]` section, so a project that routinely works with
/// wider searches or larger listings raises the ceilings in `tauqe.toml`
/// instead of recompiling the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResearchLimits {
    /// Repository search queries honored in one round.
    max_search_requests_per_round: usize,
    /// Distinct targeted map requests honored in one round.
    max_repo_map_requests_per_round: usize,
}

impl Default for ResearchLimits {
    fn default() -> Self {
        Self::from_context_config(&ContextConfig::default())
    }
}

impl ResearchLimits {
    /// Reads the Research caps declared by the project configuration.
    pub fn from_context_config(config: &ContextConfig) -> Self {
        Self {
            max_search_requests_per_round: config.max_search_requests_per_round,
            max_repo_map_requests_per_round: config.max_repo_map_requests_per_round,
        }
    }
}

fn access_satisfies(current: Option<ContextAccess>, requested: ContextAccess) -> bool {
    match current {
        Some(ContextAccess::Editable) => true,
        Some(ContextAccess::ReadOnly) => requested == ContextAccess::ReadOnly,
        None => false,
    }
}

/// One external document the OuterDocs layer refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OuterDocsRejection {
    /// Repository-relative cache path the model asked for.
    pub path: String,
    /// Diagnostic reported by the OuterDocs layer.
    pub reason: String,
}

/// One declared external document the developer's trust declaration refused.
///
/// A trust refusal and a delivery failure are recorded apart because they are
/// addressed to different readers. A delivery failure stays with the model,
/// which routes around the material it could not obtain; a trust refusal names
/// a declaration the repository cannot reach, so it travels outward with the
/// exact cause and the `[[workspace]]` block that would authorize the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OuterDocsTrustRefusal {
    path: String,
    /// Exact cause reported by the developer's trust declaration.
    reason: String,
    /// Ready-to-paste declaration that would authorize the source, when one exists.
    remedy: Option<String>,
}
/// Outcome of applying context requests to ContextManager.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextApplicationOutcome {
    pub added: Vec<String>,
    missing: Vec<String>,
    already_satisfied: Vec<String>,
    pub outer_docs_rejected: Vec<OuterDocsRejection>,
}

/// Reconciles the auto context layer during Research (pure append-only growth).
///
/// Requested files are added to the Auto layer or have their access upgraded,
/// while a declared external document enters the dedicated reference layer.
/// Size admission belongs to the assembled model request.
///
/// A candidate is admitted either because Git tracks it or because it already
/// exists on disk inside one of the harness-owned sandboxes: `.tauqe/mockups/`
/// holds the throwaway prototypes earlier turns wrote, and
/// `.tauqe/cache/outer_docs/` holds the external snapshots the project declared.
/// Both trees are Git-ignored by construction, so the tracked file list never
/// names them, and demanding a tracked entry would make a mockup unreachable for
/// the very turn that must rewrite it and a snapshot unreachable through its
/// relative link.
///
/// A snapshot is read-only by construction and enters the dedicated OuterDocs
/// layer through [`ContextManager::add_outer_docs_file`]. Requesting an already
/// loaded snapshot changes nothing: the snapshot is immutable for the whole task and the layer
/// keeps no eviction order.
fn reconcile_research_context(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
) -> ContextApplicationOutcome {
    let mut outcome = ContextApplicationOutcome::default();

    // Admit in order so a later request may upgrade an earlier read-only one.
    for request in requests {
        let Ok(path) = context_manager.normalize_path(&request.path) else {
            if !outcome.missing.contains(&request.path) {
                outcome.missing.push(request.path.clone());
            }
            continue;
        };
        if !admits_research_path(context_manager, &path, available_files) {
            if !outcome.missing.contains(&path) {
                outcome.missing.push(path);
            }
            continue;
        }
        let requested_access = if outer_docs::is_outer_docs_cache_path(&path) {
            ContextAccess::ReadOnly
        } else {
            request.access
        };
        if access_satisfies(
            context_manager.effective_access_for(&path),
            requested_access,
        ) {
            if !outcome.added.contains(&path) && !outcome.already_satisfied.contains(&path) {
                outcome.already_satisfied.push(path);
            }
            continue;
        }
        if outer_docs::is_outer_docs_cache_path(&path) {
            match context_manager.add_outer_docs_file(&path) {
                Ok(_) => {
                    if !outcome.added.contains(&path) {
                        outcome.added.push(path);
                    }
                }
                Err(err) => {
                    // The layer applies no budget of its own, so a refusal here
                    // means the snapshot is no longer on disk. A refused
                    // document is never deleted from the cache: a later round
                    // may materialize it again and read it.
                    if !outcome
                        .outer_docs_rejected
                        .iter()
                        .any(|entry| entry.path == path)
                    {
                        outcome.outer_docs_rejected.push(OuterDocsRejection {
                            path,
                            reason: format!("{:#}", err),
                        });
                    }
                }
            }
            continue;
        }
        if context_manager
            .add_auto_file(&path, requested_access)
            .is_ok()
            && access_satisfies(
                context_manager.effective_access_for(&path),
                requested_access,
            )
        {
            outcome.already_satisfied.retain(|entry| entry != &path);
            if !outcome.added.contains(&path) {
                outcome.added.push(path);
            }
        }
    }

    outcome
}

/// Reports whether a requested repository path may enter the context.
///
/// A tracked file is always admissible. A harness-owned sandbox path is
/// admissible when it already exists on disk, because Git never tracks it: both
/// sandbox trees are ignored by construction.
fn admits_research_path(
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
    let sandboxed = mockups::is_mockups_path(path) || outer_docs::is_outer_docs_cache_path(path);
    sandboxed && context_manager.repo_root().join(path).is_file()
}
/// Registers model-requested repository files and external documents in the
/// context with detailed diagnostics.
///
/// The complete prepared request is admitted against the shared input limit
/// before the next provider call.
pub fn apply_context_requests_detailed(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
) -> ContextApplicationOutcome {
    reconcile_research_context(context_manager, requests, available_files)
}

/// Registers model-requested repository files in the Auto layer.
/// Returns the paths that actually widened the effective context.
#[cfg(test)]
fn apply_context_requests(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
) -> Vec<String> {
    apply_context_requests_detailed(context_manager, requests, available_files).added
}

/// Immutable inputs shared by every Research capability handler.
///
/// The dispatcher resolves repository facts once per round and hands them to
/// every handler, so a handler never performs its own file listing and never
/// re-derives the same repository state.
struct ResearchRoundContext<'a> {
    /// Repository root used for path validation and file scans.
    repo_root: &'a Path,
    /// Repository-relative files known to exist in the working tree.
    available_files: &'a [String],
    /// Operational caps resolved from the project configuration.
    limits: ResearchLimits,
    /// Whether a request already satisfied by the context counts as progress.
    allow_already_satisfied_feedback: bool,
}

/// Accumulated effect of one Research round.
///
/// Every handler folds its own findings into this value, so a turn loop
/// interprets a round through one uniform structure instead of branching on
/// individual request types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResearchRoundOutcome {
    /// Files that widened the effective context this round.
    added_files: Vec<String>,
    /// Requested files that do not exist in the repository.
    missing_files: Vec<String>,
    /// Requested files whose current access already satisfied the request.
    already_satisfied_files: Vec<String>,
    /// External documents the OuterDocs layer refused, with the reported reason.
    outer_docs_rejected: Vec<OuterDocsRejection>,
    /// External documents the developer's trust declaration refused.
    ///
    /// Kept apart from [`Self::outer_docs_rejected`] because a trust refusal is
    /// answered by the developer, never by the model that requested the document.
    outer_docs_trust_refused: Vec<OuterDocsTrustRefusal>,
    /// Repository searches executed this round, with their matches.
    searches_executed: Vec<RepoSearchResult>,
    /// Documentation topics loaded this round.
    doc_topics_loaded: Vec<DocTopic>,
    /// Repository directory trees rendered this round.
    dir_trees_loaded: usize,
    /// Directory listings rendered this round.
    dir_lists_loaded: usize,
    /// Targeted repository maps rendered this round.
    repo_maps_loaded: usize,
    /// Feedback blocks to inject into the next proposal round.
    feedback_blocks: Vec<String>,
    /// Results of research commands, excluding instructions and admission diagnostics.
    evidence_blocks: Vec<String>,
}

impl ResearchRoundOutcome {
    pub fn evidence_blocks(&self) -> &[String] {
        &self.evidence_blocks
    }

    fn push_evidence(&mut self, block: String) {
        self.evidence_blocks.push(block.clone());
        self.feedback_blocks.push(block);
    }

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
            || !self.outer_docs_rejected.is_empty()
            || !self.outer_docs_trust_refused.is_empty()
    }

    /// Records one external document the OuterDocs layer could not materialize.
    ///
    /// The audience of the failure decides its channel. A trust refusal is a
    /// decision of the developer's workspace declaration, so no round of the
    /// turn can satisfy it: it is recorded as a typed entry beside the exact
    /// cause and the declaration that would authorize the source, and the model
    /// is told to report the missing document instead of routing around the
    /// prohibition. A delivery failure addresses the model, which works with
    /// the material it already holds and retries when the source is reachable.
    fn record_outer_docs_failure(&mut self, path: &str, error: &MaterializeError) {
        let Some(refusal) = error.trust() else {
            self.record_delivery_failure(path, error.reason());
            return;
        };
        if self
            .outer_docs_trust_refused
            .iter()
            .any(|entry| entry.path == path)
        {
            return;
        }
        self.feedback_blocks.push(format!(
            "<outer_docs_feedback>\nThe external document `{path}` was refused by the developer's trust declaration: {reason}\nThis decision belongs to the developer and cannot be satisfied from inside the turn. Report the missing document and the declaration it needs instead of retrying the request: do not request the same path again, and do not look for another route to the same material.\n</outer_docs_feedback>",
            reason = refusal.reason()
        ));
        self.outer_docs_trust_refused.push(OuterDocsTrustRefusal {
            path: path.to_string(),
            reason: refusal.reason(),
            remedy: refusal.remedy(),
        });
    }

    /// Records one external document the harness could not deliver.
    ///
    /// A delivery failure is model-addressed: the source may be unreachable
    /// now and reachable later, so the model routes around the missing material
    /// and may attempt the same request in a later round.
    fn record_delivery_failure(&mut self, path: &str, reason: String) {
        if self
            .outer_docs_rejected
            .iter()
            .any(|entry| entry.path == path)
        {
            return;
        }
        self.feedback_blocks.push(format!(
            "<outer_docs_feedback>\nThe external document `{path}` could not be delivered: {reason}\nWork with the material you already hold and route around the missing document. The same request may be attempted again in a later round when the source is reachable.\n</outer_docs_feedback>"
        ));
        self.outer_docs_rejected.push(OuterDocsRejection {
            path: path.to_string(),
            reason,
        });
    }

    /// Joins the round's feedback blocks into one prompt fragment.
    #[cfg(test)]
    fn combined_feedback(&self) -> String {
        self.feedback_blocks
            .iter()
            .map(|block| block.trim())
            .filter(|block| !block.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Detailed round summary for the architect research banner.
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

/// One autonomous executor for a Research capability.
///
/// A handler owns the validation, disk work, and feedback text of its own
/// capability, so turn loops dispatch Research commands polymorphically instead
/// of branching on command types.
trait ResearchCapabilityHandler: Send + Sync {
    /// Reports whether the handler executes the given command.
    fn handles(&self, command: &LlmCommand) -> bool;

    /// Executes every command the handler owns, folding results into `outcome`.
    fn execute(
        &self,
        round: &ResearchRoundContext<'_>,
        commands: &[LlmCommand],
        context_manager: &mut ContextManager,
        outcome: &mut ResearchRoundOutcome,
    );
}

/// Executes context-file requests.
struct ContextRequestHandler;

impl ResearchCapabilityHandler for ContextRequestHandler {
    fn handles(&self, command: &LlmCommand) -> bool {
        matches!(command, LlmCommand::ContextRequest(_))
    }

    fn execute(
        &self,
        round: &ResearchRoundContext<'_>,
        commands: &[LlmCommand],
        context_manager: &mut ContextManager,
        outcome: &mut ResearchRoundOutcome,
    ) {
        let requests: Vec<ContextRequest> = commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::ContextRequest(command) => Some(ContextRequest {
                    path: command.path.clone(),
                    access: command.access,
                }),
                _ => None,
            })
            .collect();
        if requests.is_empty() {
            return;
        }

        let applied = reconcile_research_context(context_manager, &requests, round.available_files);
        outcome.added_files.extend(applied.added);
        outcome.missing_files.extend(applied.missing);
        outcome
            .already_satisfied_files
            .extend(applied.already_satisfied);

        if !outcome.missing_files.is_empty() {
            outcome.feedback_blocks.push(format!(
                "<context_feedback>\nThe following requested files do not exist in the repository: [{}]\nPlease check <repo_map> for exact existing file paths.\n</context_feedback>",
                outcome.missing_files.join(", ")
            ));
        }
        if !applied.outer_docs_rejected.is_empty() {
            // A snapshot that vanished is actionable knowledge, not a dead end:
            // the model learns that the declared document is no longer cached.
            let rejected = applied
                .outer_docs_rejected
                .iter()
                .map(|entry| format!("{}: {}", entry.path, entry.reason))
                .collect::<Vec<_>>()
                .join("; ");
            outcome.feedback_blocks.push(format!(
                "<outer_docs_feedback>\nThe following requested external documents could not be loaded: [{}]\nThe snapshot is no longer available in the harness cache. Report the missing document to the developer instead of retrying the same request in this turn.\n</outer_docs_feedback>",
                rejected
            ));
            outcome
                .outer_docs_rejected
                .extend(applied.outer_docs_rejected);
        }
    }
}

/// Executes repository regex searches and formats their feedback.
struct RepoSearchHandler;

impl ResearchCapabilityHandler for RepoSearchHandler {
    fn handles(&self, command: &LlmCommand) -> bool {
        matches!(command, LlmCommand::RepoSearch(_))
    }

    fn execute(
        &self,
        round: &ResearchRoundContext<'_>,
        commands: &[LlmCommand],
        _context_manager: &mut ContextManager,
        outcome: &mut ResearchRoundOutcome,
    ) {
        let queries: Vec<RepoSearchRequest> = commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::RepoSearch(command) => Some(RepoSearchRequest::new(
                    command.path.as_str(),
                    command.regex.as_str(),
                )),
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
            outcome.push_evidence(format_search_feedback(&results));
        }
        outcome.searches_executed.extend(results);
    }
}

/// Renders the compact repository directory tree on demand.
struct RepoDirsHandler;

impl ResearchCapabilityHandler for RepoDirsHandler {
    fn handles(&self, command: &LlmCommand) -> bool {
        matches!(command, LlmCommand::RepoDirs(_))
    }

    fn execute(
        &self,
        round: &ResearchRoundContext<'_>,
        commands: &[LlmCommand],
        context_manager: &mut ContextManager,
        outcome: &mut ResearchRoundOutcome,
    ) {
        if !commands
            .iter()
            .any(|command| matches!(command, LlmCommand::RepoDirs(_)))
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
        outcome.push_evidence(format!(
            "<repo_dirs_feedback>\n{}\n</repo_dirs_feedback>",
            body
        ));
        outcome.dir_trees_loaded += 1;
    }
}

/// Renders the targeted symbol map of a requested subdirectory.
struct RepoMapRequestHandler;

impl ResearchCapabilityHandler for RepoMapRequestHandler {
    fn handles(&self, command: &LlmCommand) -> bool {
        matches!(command, LlmCommand::RepoMap(_))
    }

    fn execute(
        &self,
        round: &ResearchRoundContext<'_>,
        commands: &[LlmCommand],
        _context_manager: &mut ContextManager,
        outcome: &mut ResearchRoundOutcome,
    ) {
        let max_requests = round.limits.max_repo_map_requests_per_round;
        let mut requested: Vec<String> = Vec::new();
        for command in commands {
            let LlmCommand::RepoMap(command) = command else {
                continue;
            };
            let target = command.path.trim().to_string();
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
            outcome.push_evidence(format!(
                "<repomap_feedback path=\"{}\">\n{}\n</repomap_feedback>",
                display_path, body
            ));
            outcome.repo_maps_loaded += 1;
        }
    }
}

/// Lists the tracked files of a requested directory.
struct DirListHandler;

impl ResearchCapabilityHandler for DirListHandler {
    fn handles(&self, command: &LlmCommand) -> bool {
        matches!(command, LlmCommand::DirList(_))
    }

    fn execute(
        &self,
        round: &ResearchRoundContext<'_>,
        commands: &[LlmCommand],
        _context_manager: &mut ContextManager,
        outcome: &mut ResearchRoundOutcome,
    ) {
        let mut requested: Vec<DirListRequest> = Vec::new();
        for command in commands {
            let LlmCommand::DirList(command) = command else {
                continue;
            };
            let request = DirListRequest::new(command.path.clone(), command.recursive);
            let Some(request) = request.normalized() else {
                tracing::warn!(
                    path = command.path.as_str(),
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
            let entries =
                render_dir_listing(round.available_files, &request.path, request.recursive);
            let body = if entries.is_empty() {
                format!(
                    "No tracked files under `{}`. Check <repo_dirs> for existing paths.",
                    display_path
                )
            } else {
                entries.join("\n")
            };
            outcome.push_evidence(format!(
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

impl ResearchCapabilityHandler for DocRequestHandler {
    fn handles(&self, command: &LlmCommand) -> bool {
        matches!(command, LlmCommand::DocRequest(_))
    }

    fn execute(
        &self,
        _round: &ResearchRoundContext<'_>,
        commands: &[LlmCommand],
        _context_manager: &mut ContextManager,
        outcome: &mut ResearchRoundOutcome,
    ) {
        let topics: Vec<DocTopic> = commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::DocRequest(command) => Some(command.topic),
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
            outcome.push_evidence(format!(
                "<system_documentation>\n{}\n</system_documentation>",
                documentation.trim()
            ));
        }
        outcome.doc_topics_loaded.extend(topics);
    }
}

/// Registered Research handlers, in dispatch order.
static RESEARCH_HANDLERS: &[&dyn ResearchCapabilityHandler] = &[
    &ContextRequestHandler,
    &RepoSearchHandler,
    &RepoDirsHandler,
    &RepoMapRequestHandler,
    &DirListHandler,
    &DocRequestHandler,
];

/// Returns the registered Research capability handlers in dispatch order.
fn research_handlers() -> &'static [&'static dyn ResearchCapabilityHandler] {
    RESEARCH_HANDLERS
}

/// Executes Research commands synchronously against an already resolved
/// repository file list.
///
/// This is the core the asynchronous facade and the server architect loop call
/// from inside `run_blocking` / `with_context_mutation`, so no handler ever
/// blocks an async worker thread. The `limits` caps travel with the call, so a
/// round applies exactly the bounds the project declared.
fn execute_research_commands_sync(
    context_manager: &mut ContextManager,
    repo_root: &Path,
    available_files: &[String],
    commands: &[LlmCommand],
    limits: ResearchLimits,
    allow_already_satisfied_feedback: bool,
) -> ResearchRoundOutcome {
    let round = ResearchRoundContext {
        repo_root,
        available_files,
        limits,
        allow_already_satisfied_feedback,
    };
    let mut outcome = ResearchRoundOutcome::default();

    for handler in research_handlers() {
        if !commands.iter().any(|command| handler.handles(command)) {
            continue;
        }
        handler.execute(&round, commands, context_manager, &mut outcome);
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
pub(crate) async fn resolve_repository_files(repo_root: &Path) -> Option<Vec<String>> {
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
fn uninspectable_repository_outcome() -> ResearchRoundOutcome {
    let mut outcome = ResearchRoundOutcome::default();
    outcome.feedback_blocks.push(
        "<context_feedback>\nThe repository could not be inspected, so the tracked \
         file list and repository searches are unavailable for this round. Do not \
         conclude that requested files are missing: proceed with the material \
         already loaded in context.\n</context_feedback>"
            .to_string(),
    );
    outcome
}
/// Executes Research commands asynchronously, resolving repository facts off
/// the async executor thread.
///
/// The repository file list is read once per round, and every handler runs
/// inside `run_blocking`, so path canonicalization, file metadata reads, and
/// regex scans never stall a Tokio worker. A repository that cannot be
/// inspected ends the round with an inspection-failure feedback block instead
/// of reporting every requested file as missing.
///
/// The optional cancel signal is the turn's own: a round the developer already
/// abandoned resolves immediately, before the repository file list is read and
/// before a single handler runs.
pub async fn execute_research_commands(
    context_manager: &mut ContextManager,
    repo_root: &Path,
    commands: &[LlmCommand],
    limits: ResearchLimits,
    allow_already_satisfied_feedback: bool,
    cancel: Option<&HardCancellation>,
) -> ResearchRoundOutcome {
    if commands.is_empty() || cancel.is_some_and(HardCancellation::is_cancelled) {
        return ResearchRoundOutcome::default();
    }
    let needs_file_list = commands.iter().any(|command| {
        matches!(
            command,
            LlmCommand::ContextRequest(_)
                | LlmCommand::RepoSearch(_)
                | LlmCommand::RepoDirs(_)
                | LlmCommand::RepoMap(_)
                | LlmCommand::DirList(_)
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
                return outcome;
            }
        }
    } else {
        Vec::new()
    };

    run_blocking(|| {
        execute_research_commands_sync(
            context_manager,
            repo_root,
            &available_files,
            commands,
            limits,
            allow_already_satisfied_feedback,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::search::FileSearchMatch;
    use super::*;

    /// Research caps applied by tests that do not exercise a specific limit.
    fn default_limits() -> ResearchLimits {
        ResearchLimits::default()
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

        let added = apply_context_requests(&mut cm, &requests, &available);
        assert_eq!(added, vec!["a.rs".to_string()]);
        assert!(cm.is_editable("a.rs").unwrap());

        // Already satisfied: no further changes.
        assert!(apply_context_requests(&mut cm, &requests, &available).is_empty());

        cm.clear_auto();
        assert!(!cm.contains("a.rs").unwrap());
    }

    #[test]
    fn test_apply_context_requests_admits_large_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() { let x = 12345; }").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["a.rs".to_string()];
        let requests = vec![ContextRequest {
            path: "a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }];

        let outcome = apply_context_requests_detailed(&mut cm, &requests, &available);
        assert_eq!(outcome.added, vec!["a.rs"]);
        assert!(cm.contains("a.rs").unwrap());
    }

    #[test]
    fn test_repeated_requests_upgrade_access() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["a.rs".to_string()];
        cm.add_file("a.rs", ContextAccess::ReadOnly).unwrap();
        let requests = vec![
            ContextRequest {
                path: "a.rs".into(),
                access: ContextAccess::ReadOnly,
            },
            ContextRequest {
                path: "a.rs".into(),
                access: ContextAccess::Editable,
            },
        ];
        let outcome = apply_context_requests_detailed(&mut cm, &requests, &available);
        assert_eq!(outcome.added, vec!["a.rs".to_string()]);
        assert!(outcome.already_satisfied.is_empty());
        assert!(cm.is_editable("a.rs").unwrap());
    }

    #[test]
    fn test_loaded_file_does_not_prevent_another_admission() {
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

        let outcome = apply_context_requests_detailed(&mut cm, &requests, &available);
        assert_eq!(outcome.added, vec!["next.rs".to_string()]);
        assert!(cm.contains("loaded.rs").unwrap());
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
        let added = apply_context_requests(&mut cm, &requests, &available);
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

        let outcome = apply_context_requests_detailed(&mut cm, &requests, &available);
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
            unsearched_files: 0,
            error: None,
        }
    }

    #[test]
    fn test_round_outcome_banner_summaries() {
        let mut outcome = ResearchRoundOutcome::default();
        assert_eq!(outcome.architect_banner_summary(), "");

        outcome.added_files.push("a.rs".to_string());
        outcome.added_files.push("b.rs".to_string());
        outcome.missing_files.push("c.rs".to_string());
        outcome.searches_executed.push(sample_search_result());
        outcome.doc_topics_loaded.push(DocTopic::Workflow);
        assert_eq!(
            outcome.architect_banner_summary(),
            "Added 2 file(s) to context: a.rs, b.rs; Executed 1 repository search(es); \
             Files not found in repository: c.rs"
        );

        // The already-satisfied note only explains a round that did nothing else.
        let mut satisfied = ResearchRoundOutcome::default();
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
    fn test_execute_research_commands_dispatches_every_capability() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "pub fn beta() {}\n").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["a.rs".to_string(), "b.rs".to_string()];

        let commands = vec![
            LlmCommand::ContextRequest(crate::llm::ContextRequestCommand {
                path: "a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            }),
            LlmCommand::ContextRequest(crate::llm::ContextRequestCommand {
                path: "missing.rs".to_string(),
                access: ContextAccess::ReadOnly,
            }),
            LlmCommand::RepoSearch(crate::llm::RepoSearchCommand {
                path: String::new(),
                regex: "pub fn".to_string(),
            }),
            LlmCommand::DocRequest(crate::llm::DocRequestCommand {
                topic: DocTopic::Workflow,
            }),
        ];

        let outcome = execute_research_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
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
    fn test_execute_research_commands_admits_files_and_continues_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("big.rs"),
            "pub fn big() { let x = 1234567; }\n",
        )
        .unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["big.rs".to_string()];

        let commands = vec![
            LlmCommand::ContextRequest(crate::llm::ContextRequestCommand {
                path: "big.rs".to_string(),
                access: ContextAccess::ReadOnly,
            }),
            LlmCommand::DocRequest(crate::llm::DocRequestCommand {
                topic: DocTopic::Workflow,
            }),
        ];

        let outcome = execute_research_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            default_limits(),
            false,
        );

        assert_eq!(outcome.added_files, ["big.rs"]);
        assert_eq!(outcome.doc_topics_loaded, [DocTopic::Workflow]);
        assert!(outcome.has_progress(false));
    }

    #[test]
    fn test_execute_research_commands_already_satisfied_feedback_is_aggregate_aware() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn alpha() {}\n").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        cm.add_file("a.rs", ContextAccess::ReadOnly).unwrap();
        let available = vec!["a.rs".to_string()];
        let commands = vec![LlmCommand::ContextRequest(
            crate::llm::ContextRequestCommand {
                path: "a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        )];

        let outcome = execute_research_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            default_limits(),
            false,
        );
        assert_eq!(outcome.already_satisfied_files, vec!["a.rs".to_string()]);
        assert!(!outcome.has_progress(false));
        assert!(outcome.combined_feedback().is_empty());

        let outcome = execute_research_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            default_limits(),
            true,
        );
        assert!(outcome.has_progress(true));
        assert!(outcome
            .combined_feedback()
            .contains("already loaded in context"));
    }

    #[test]
    fn test_execute_research_commands_renders_the_directory_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec![
            "src/main.rs".to_string(),
            "src/nested/module.rs".to_string(),
        ];

        let commands = vec![LlmCommand::RepoDirs(crate::llm::RepoDirsCommand {})];
        let outcome = execute_research_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            default_limits(),
            false,
        );

        assert_eq!(outcome.dir_trees_loaded, 1);
        assert!(outcome.has_progress(false));
        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("<repo_dirs_feedback>"));
        assert!(feedback.contains("src\n  nested"));
    }

    #[test]
    fn test_execute_research_commands_lists_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec![
            "src/main.rs".to_string(),
            "src/nested/module.rs".to_string(),
            "README.md".to_string(),
        ];

        // A shallow listing reports the direct children of the directory.
        let commands = vec![LlmCommand::DirList(crate::llm::DirListCommand {
            path: "src".to_string(),
            recursive: false,
        })];
        let outcome = execute_research_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            default_limits(),
            false,
        );
        assert_eq!(outcome.dir_lists_loaded, 1);
        assert!(outcome.has_progress(false));
        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("<dir_list_feedback path=\"src\" recursive=\"false\">"));
        assert!(feedback.contains("main.rs"));
        assert!(feedback.contains("nested/"));
        assert!(!feedback.contains("README.md"));

        // A recursive listing reports every tracked file underneath.
        let commands = vec![LlmCommand::DirList(crate::llm::DirListCommand {
            path: "src".to_string(),
            recursive: true,
        })];
        let outcome = execute_research_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            default_limits(),
            false,
        );
        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("<dir_list_feedback path=\"src\" recursive=\"true\">"));
        assert!(feedback.contains("main.rs"));
        assert!(feedback.contains("nested/module.rs"));
    }

    #[test]
    fn test_execute_research_commands_returns_every_listing_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available: Vec<String> = (0..150)
            .map(|index| format!("src/file_{index:03}.rs"))
            .collect();
        let commands = vec![LlmCommand::DirList(crate::llm::DirListCommand {
            path: "src".to_string(),
            recursive: true,
        })];
        let outcome = execute_research_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            default_limits(),
            false,
        );

        assert_eq!(outcome.dir_lists_loaded, 1);
        let feedback = outcome.combined_feedback();
        for file in &available {
            assert!(feedback.contains(file.strip_prefix("src/").unwrap()));
        }
        assert!(!feedback.contains("omitted"));
    }

    #[test]
    fn test_execute_research_commands_ignores_commands_outside_research() {
        let dir = tempfile::tempdir().unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());

        let commands = vec![LlmCommand::Text(crate::llm::TextCommand {
            content: "no research work".to_string(),
        })];
        let outcome = execute_research_commands_sync(
            &mut cm,
            dir.path(),
            &[],
            &commands,
            default_limits(),
            true,
        );

        assert_eq!(outcome, ResearchRoundOutcome::default());
        assert!(!outcome.has_progress(true));
    }

    #[test]
    fn test_execute_research_commands_renders_a_targeted_repo_map() {
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
            LlmCommand::RepoMap(crate::llm::RepoMapCommand {
                path: "src".to_string(),
            }),
            LlmCommand::RepoMap(crate::llm::RepoMapCommand {
                path: "src".to_string(),
            }),
        ];
        let outcome = execute_research_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            default_limits(),
            false,
        );

        assert_eq!(outcome.repo_maps_loaded, 1);
        assert!(outcome.has_progress(false));

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
    fn test_execute_research_commands_reports_an_empty_targeted_map() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/alpha.rs"), "pub fn alpha() {}\n").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["src/alpha.rs".to_string()];

        let commands = vec![LlmCommand::RepoMap(crate::llm::RepoMapCommand {
            path: "docs".to_string(),
        })];
        let outcome = execute_research_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
            default_limits(),
            false,
        );

        assert_eq!(outcome.repo_maps_loaded, 1);
        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("<repomap_feedback path=\"docs\">"));
        assert!(feedback.contains("No tracked source files under `docs`"));
    }

    #[test]
    fn test_execute_research_commands_caps_targeted_repo_maps_per_round() {
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
            .map(|index| {
                LlmCommand::RepoMap(crate::llm::RepoMapCommand {
                    path: format!("module{index}"),
                })
            })
            .collect();

        // The per-round request cap is configuration-driven: the shared handler
        // owns it, so an untrusted list of requested contours can never turn one
        // round into a full repository re-index.
        let limits = ResearchLimits {
            max_repo_map_requests_per_round: 2,
            ..default_limits()
        };
        let outcome = execute_research_commands_sync(
            &mut cm,
            dir.path(),
            &available,
            &commands,
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
        let commands = vec![LlmCommand::ContextRequest(
            crate::llm::ContextRequestCommand {
                path: "a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        )];

        let (buffer, _guard) = crate::blocking::log_capture::install();
        let outcome = execute_research_commands(
            &mut cm,
            &missing_root,
            &commands,
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
    /// list never names it. Research must still admit it, otherwise the model
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
        let outcome = apply_context_requests_detailed(&mut cm, &requests, &[]);

        assert_eq!(outcome.added, vec![mockup.clone()]);
        assert!(outcome.missing.is_empty());
        assert!(cm.is_editable(&mockup).unwrap());
    }

    /// A cached external document is Git-ignored and read-only by
    /// construction: it lands in the dedicated OuterDocs layer even when the
    /// model asks for write access.
    #[test]
    fn test_a_cached_external_document_enters_the_outer_docs_layer_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let cache = root.join(crate::outer_docs::OUTER_DOCS_CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("api.md"), "upstream notes\n").unwrap();
        let mut cm = ContextManager::new(root);
        let cached = format!("{}/api.md", crate::outer_docs::OUTER_DOCS_CACHE_DIR);
        let requests = vec![ContextRequest {
            path: cached.clone(),
            access: ContextAccess::Editable,
        }];

        let outcome = apply_context_requests_detailed(&mut cm, &requests, &[]);

        assert_eq!(outcome.added, vec![cached.clone()]);
        // The layer addresses the document by its logical path, never by the
        // cache path it is stored under.
        assert_eq!(cm.outer_docs_files(), vec!["api.md".to_string()]);
        assert!(!cm.is_editable(&cached).unwrap());
        assert!(outcome.outer_docs_rejected.is_empty());
    }

    /// Re-requesting a loaded snapshot is a no-op, and the layer keeps every
    /// admitted document: no eviction order survives the rename.
    #[test]
    fn test_a_repeated_external_document_request_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let cache = root.join(crate::outer_docs::OUTER_DOCS_CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        for name in ["a.md", "b.md"] {
            std::fs::write(cache.join(name), "external notes\n").unwrap();
        }
        let mut cm = ContextManager::new(root);
        let cached = |name: &str| format!("{}/{name}", crate::outer_docs::OUTER_DOCS_CACHE_DIR);
        let (first, second) = (cached("a.md"), cached("b.md"));
        let request = |path: &str| {
            vec![ContextRequest {
                path: path.to_string(),
                access: ContextAccess::ReadOnly,
            }]
        };

        let outcome = apply_context_requests_detailed(&mut cm, &request(&first), &[]);
        assert_eq!(outcome.added, vec![first.clone()]);

        let repeat = apply_context_requests_detailed(&mut cm, &request(&first), &[]);
        assert!(repeat.added.is_empty());
        assert_eq!(repeat.already_satisfied, vec![first.clone()]);

        // A second document joins the first: the layer holds both instead of
        // evicting by a recency rank it no longer keeps.
        let outcome = apply_context_requests_detailed(&mut cm, &request(&second), &[]);
        assert_eq!(outcome.added, vec![second.clone()]);
        assert_eq!(
            cm.outer_docs_files(),
            vec!["a.md".to_string(), "b.md".to_string()]
        );
    }

    /// A large snapshot enters context without an independent size gate.
    #[test]
    fn test_a_large_external_document_is_admitted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let cache = root.join(crate::outer_docs::OUTER_DOCS_CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("huge.md"), "x".repeat(8192)).unwrap();
        let mut cm = ContextManager::new(root.clone());
        let cached = format!("{}/huge.md", crate::outer_docs::OUTER_DOCS_CACHE_DIR);
        let requests = vec![ContextRequest {
            path: cached.clone(),
            access: ContextAccess::ReadOnly,
        }];

        let outcome = apply_context_requests_detailed(&mut cm, &requests, &[]);

        assert_eq!(outcome.added.as_slice(), std::slice::from_ref(&cached));
        assert!(outcome.outer_docs_rejected.is_empty());
        assert_eq!(cm.outer_docs_files(), ["huge.md"]);
        assert!(root.join(&cached).is_file());
    }

    /// Repository files and snapshots are admitted together without eviction.
    #[test]
    fn test_mixed_repository_files_and_external_documents_are_admitted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
        let cache = root.join(crate::outer_docs::OUTER_DOCS_CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("api.md"), "upstream notes\n").unwrap();
        let mut cm = ContextManager::new(root);
        let cached = format!("{}/api.md", crate::outer_docs::OUTER_DOCS_CACHE_DIR);
        let requests = vec![
            ContextRequest {
                path: "a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
            ContextRequest {
                path: cached.clone(),
                access: ContextAccess::ReadOnly,
            },
        ];
        let available = vec!["a.rs".to_string()];

        let outcome = apply_context_requests_detailed(&mut cm, &requests, &available);

        assert_eq!(outcome.added, vec!["a.rs".to_string(), cached.clone()]);
        assert_eq!(cm.outer_docs_files(), vec!["api.md".to_string()]);
        assert!(cm.contains("a.rs").unwrap());
    }
    /// A trust decision is recorded apart from a delivery failure: the round
    /// outcome carries the exact cause and the declaration that would authorize
    /// the source, and the model is told to report it instead of routing around
    /// the prohibition.
    #[test]
    fn test_a_trust_refusal_is_recorded_apart_from_a_delivery_failure() {
        use crate::outer_docs::{TrustRefusal, WorkspaceDeclaration};

        let mut outcome = ResearchRoundOutcome::default();
        let path = format!("{}/api.md", crate::outer_docs::OUTER_DOCS_CACHE_DIR);
        let refusal = MaterializeError::Trust(TrustRefusal::WorkspaceNotAuthorized {
            workspace: WorkspaceDeclaration {
                root: "/work/project".to_string(),
                local_roots: Vec::new(),
                allow_private_network: false,
            },
        });

        outcome.record_outer_docs_failure(&path, &refusal);
        // A repeated refusal of the same document is recorded once.
        outcome.record_outer_docs_failure(&path, &refusal);

        assert!(outcome.outer_docs_rejected.is_empty());
        assert_eq!(outcome.outer_docs_trust_refused.len(), 1);
        let entry = &outcome.outer_docs_trust_refused[0];
        assert_eq!(entry.path, path);
        assert!(entry.reason.contains("no entry"), "{entry:?}");
        let remedy = entry
            .remedy
            .as_deref()
            .expect("the refusal carries its declaration");
        assert!(remedy.contains("[[workspace]]"), "{remedy}");
        assert!(outcome.has_progress(false));

        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("<outer_docs_feedback>"), "{feedback}");
        assert!(feedback.contains("trust declaration"), "{feedback}");
        assert!(feedback.contains("developer"), "{feedback}");
        assert!(
            feedback.contains("do not look for another route"),
            "{feedback}"
        );
    }

    /// A failed transfer is model-addressed: it is recorded as a delivery
    /// failure, and its feedback routes the model around the missing document
    /// instead of forbidding a later attempt.
    #[test]
    fn test_a_delivery_failure_stays_with_the_model() {
        let mut outcome = ResearchRoundOutcome::default();
        let path = format!("{}/api.md", crate::outer_docs::OUTER_DOCS_CACHE_DIR);
        let failure = MaterializeError::Delivery(
            "the document `https://example.com/llm.txt` answered with the HTTP status 404"
                .to_string(),
        );

        outcome.record_outer_docs_failure(&path, &failure);
        outcome.record_outer_docs_failure(&path, &failure);

        assert!(outcome.outer_docs_trust_refused.is_empty());
        assert_eq!(outcome.outer_docs_rejected.len(), 1);
        assert_eq!(outcome.outer_docs_rejected[0].path, path);
        assert!(outcome.outer_docs_rejected[0].reason.contains("404"));
        assert!(outcome.has_progress(false));

        let feedback = outcome.combined_feedback();
        assert!(feedback.contains("could not be delivered"), "{feedback}");
        assert!(feedback.contains("may be attempted again"), "{feedback}");
        assert!(
            !feedback.contains("do not look for another route"),
            "{feedback}"
        );
    }
}
