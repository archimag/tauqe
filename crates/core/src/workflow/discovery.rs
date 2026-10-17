pub mod search;

use std::path::Path;

use tauqe_protocol::{ContextAccess, DocTopic, LlmCapability, LlmCommand, RepoSearchRequest};

use crate::blocking::{run_blocking, run_blocking_async};
use crate::context::ContextManager;
use crate::edits::protocol::ContextRequest;

pub use search::{
    execute_repo_search, execute_repo_searches, format_search_feedback,
    format_single_search_feedback, normalize_search_path, FileSearchMatch, RepoSearchResult,
    MAX_MATCHED_FILES_PER_SEARCH, MAX_SEARCHABLE_FILE_SIZE, MAX_SEARCH_REQUESTS_PER_ROUND,
    REGEX_DFA_SIZE_LIMIT, REGEX_SIZE_LIMIT,
};

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

/// Outcome of applying context requests to ContextManager.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextApplicationOutcome {
    pub added: Vec<String>,
    pub missing: Vec<String>,
    pub already_satisfied: Vec<String>,
    pub limit_exceeded: Option<ContextLimitExceeded>,
}

/// Reconciles the auto context layer during Discovery (pure append-only growth).
///
/// Requested files are added to the Auto layer or have their access upgraded.
/// The `files_token_budget` limit is evaluated against the total estimated tokens
/// of current context files plus candidate additions.
pub fn reconcile_discovery_context(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
    files_token_budget: usize,
) -> ContextApplicationOutcome {
    let mut outcome = ContextApplicationOutcome::default();

    // 1. Identify candidate additions and validate against repository
    let mut valid_new_files = Vec::new();

    for request in requests {
        let Ok(path) = context_manager.normalize_path(&request.path) else {
            if !outcome.missing.contains(&request.path) {
                outcome.missing.push(request.path.clone());
            }
            continue;
        };
        if !available_files.contains(&path) && !context_manager.contains(&path).unwrap_or(false) {
            if !outcome.missing.contains(&path) {
                outcome.missing.push(path);
            }
            continue;
        }
        if access_satisfies(context_manager.effective_access_for(&path), request.access) {
            if !outcome.already_satisfied.contains(&path) {
                outcome.already_satisfied.push(path);
            }
            continue;
        }
        if !context_manager.contains(&path).unwrap_or(false) && !valid_new_files.contains(&path) {
            valid_new_files.push(path);
        }
    }

    // 2. Check files_token_budget limit
    // Token estimates rely on cached context sizes and file metadata instead of
    // reading full file contents, avoiding heavy blocking I/O on the async worker.
    let current_tokens: usize = context_manager.get_state().total_estimated_tokens as usize;

    let mut new_tokens = 0usize;
    for path in &valid_new_files {
        let full_path = context_manager.repo_root().join(path);
        if let Ok(meta) = std::fs::metadata(&full_path) {
            new_tokens += crate::prompt::estimate_tokens_from_bytes(meta.len()) as usize;
        }
    }

    if current_tokens + new_tokens > files_token_budget {
        outcome.limit_exceeded = Some(ContextLimitExceeded {
            current_tokens,
            requested_tokens: new_tokens,
            token_budget: files_token_budget,
            requested_files: valid_new_files,
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
        if context_manager.add_auto_file(&path, request.access).is_ok()
            && access_satisfies(context_manager.effective_access_for(&path), request.access)
            && !outcome.added.contains(&path)
        {
            outcome.added.push(path);
        }
    }

    outcome
}

/// Registers model-requested repository files in the Auto layer with detailed diagnostics.
/// Enforces the hard `files_token_budget` limit across all context layers.
pub fn apply_context_requests_detailed(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
    files_token_budget: usize,
) -> ContextApplicationOutcome {
    reconcile_discovery_context(
        context_manager,
        requests,
        available_files,
        files_token_budget,
    )
}

/// Registers model-requested repository files in the Auto layer.
/// Returns the paths that actually widened the effective context.
pub fn apply_context_requests(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
    files_token_budget: usize,
) -> Vec<String> {
    apply_context_requests_detailed(context_manager, requests, available_files, files_token_budget).added
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
    /// Repository searches executed this round, with their matches.
    pub searches_executed: Vec<RepoSearchResult>,
    /// Documentation topics loaded this round.
    pub doc_topics_loaded: Vec<DocTopic>,
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
            || !self.doc_topics_loaded.is_empty()
            || (allow_already_satisfied && !self.already_satisfied_files.is_empty())
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
            parts.push(format!("{} search(es) executed", self.searches_executed.len()));
        }
        if !self.doc_topics_loaded.is_empty() {
            parts.push(format!("{} doc topic(s) loaded", self.doc_topics_loaded.len()));
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

        let results = execute_repo_searches(round.repo_root, &queries, round.available_files);
        if !results.is_empty() {
            outcome.feedback_blocks.push(format_search_feedback(&results));
        }
        outcome.searches_executed.extend(results);
    }
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
static DISCOVERY_HANDLERS: &[&dyn DiscoveryCapabilityHandler] =
    &[&ContextRequestHandler, &RepoSearchHandler, &DocRequestHandler];

/// Returns the registered Discovery capability handlers in dispatch order.
pub fn discovery_handlers() -> &'static [&'static dyn DiscoveryCapabilityHandler] {
    DISCOVERY_HANDLERS
}

/// Executes Discovery commands synchronously against an already resolved
/// repository file list.
///
/// This is the core the asynchronous facade and the server architect loop call
/// from inside `run_blocking` / `with_context_mutation`, so no handler ever
/// blocks an async worker thread.
pub fn execute_discovery_commands_sync(
    context_manager: &mut ContextManager,
    repo_root: &Path,
    available_files: &[String],
    commands: &[LlmCommand],
    files_token_budget: usize,
    allow_already_satisfied_feedback: bool,
) -> DiscoveryRoundOutcome {
    let round = DiscoveryRoundContext {
        repo_root,
        available_files,
        files_token_budget,
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
            "<context_feedback>\nThe following files are already loaded in context: [{}]\nYou may proceed with inspecting them and refining the plan.\n</context_feedback>",
            outcome.already_satisfied_files.join(", ")
        ));
    }

    outcome
}

/// Executes Discovery commands asynchronously, resolving repository facts off
/// the async executor thread.
///
/// The repository file list is read once per round, and every handler runs
/// inside `run_blocking`, so path canonicalization, file metadata reads, and
/// regex scans never stall a Tokio worker.
pub async fn execute_discovery_commands(
    context_manager: &mut ContextManager,
    repo_root: &Path,
    commands: &[LlmCommand],
    files_token_budget: usize,
    allow_already_satisfied_feedback: bool,
) -> DiscoveryRoundOutcome {
    if commands.is_empty() {
        return DiscoveryRoundOutcome::default();
    }

    let needs_file_list = commands.iter().any(|command| {
        matches!(
            command,
            LlmCommand::ContextRequest { .. } | LlmCommand::RepoSearch { .. }
        )
    });
    let available_files = if needs_file_list {
        let root = repo_root.to_path_buf();
        run_blocking_async(move || {
            crate::git::list_repository_files(Some(&root)).unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    run_blocking(|| {
        execute_discovery_commands_sync(
            context_manager,
            repo_root,
            &available_files,
            commands,
            files_token_budget,
            allow_already_satisfied_feedback,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let added = apply_context_requests(&mut cm, &requests, &available, 300_000);
        assert_eq!(added, vec!["a.rs".to_string()]);
        assert!(cm.is_editable("a.rs").unwrap());

        // Already satisfied: no further changes.
        assert!(apply_context_requests(&mut cm, &requests, &available, 300_000).is_empty());

        cm.clear_auto();
        assert!(!cm.contains("a.rs").unwrap());
    }

    #[test]
    fn test_apply_context_requests_respects_token_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() { let x = 12345; }").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["a.rs".to_string()];
        let requests = vec![
            ContextRequest {
                path: "a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        ];

        // Budget is only 2 tokens; file content is ~7 tokens.
        let outcome = apply_context_requests_detailed(&mut cm, &requests, &available, 2);
        assert!(outcome.added.is_empty());
        let limit = outcome.limit_exceeded.expect("Expected limit_exceeded");
        assert_eq!(limit.token_budget, 2);
        assert!(limit.requested_tokens > 2);
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

        let added = apply_context_requests(&mut cm, &requests, &available, 300_000);
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

        let outcome = apply_context_requests_detailed(&mut cm, &requests, &available, 300_000);
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
        assert_eq!(satisfied.architect_banner_summary(), "Added 1 file(s) to context: b.rs");
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

        let outcome =
            execute_discovery_commands_sync(&mut cm, dir.path(), &available, &commands, 300_000, false);

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

        let outcome = execute_discovery_commands_sync(&mut cm, dir.path(), &available, &commands, 1, false);

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

        let outcome =
            execute_discovery_commands_sync(&mut cm, dir.path(), &available, &commands, 300_000, false);
        assert_eq!(outcome.already_satisfied_files, vec!["a.rs".to_string()]);
        assert!(!outcome.has_progress(false));
        assert!(outcome.combined_feedback().is_empty());

        let outcome =
            execute_discovery_commands_sync(&mut cm, dir.path(), &available, &commands, 300_000, true);
        assert!(outcome.has_progress(true));
        assert!(outcome.combined_feedback().contains("already loaded in context"));
    }

    #[test]
    fn test_execute_discovery_commands_ignores_commands_outside_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());

        let commands = vec![LlmCommand::Text {
            content: "no discovery work".to_string(),
        }];
        let outcome = execute_discovery_commands_sync(&mut cm, dir.path(), &[], &commands, 300_000, true);

        assert_eq!(outcome, DiscoveryRoundOutcome::default());
        assert!(!outcome.has_progress(true));
    }
}
