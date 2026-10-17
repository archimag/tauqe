pub mod search;

use tauqe_protocol::ContextAccess;

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
}
