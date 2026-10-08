use tauqe_protocol::{ContextAccess, ModelResult};

use crate::context::ContextManager;
use crate::edits::protocol::ContextRequest;

pub(crate) fn access_satisfies(current: Option<ContextAccess>, requested: ContextAccess) -> bool {
    match current {
        Some(ContextAccess::Editable) => true,
        Some(ContextAccess::ReadOnly) => requested == ContextAccess::ReadOnly,
        None => false,
    }
}

/// Registers model-requested repository files in the Auto layer.
/// Returns the paths that actually widened the effective context.
pub fn apply_context_requests(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
    max_auto_files: Option<usize>,
) -> Vec<String> {
    let mut added = Vec::new();
    for request in requests {
        if let Some(limit) = max_auto_files {
            if added.len() >= limit {
                break;
            }
        }
        let Ok(path) = context_manager.normalize_path(&request.path) else {
            continue;
        };
        if !available_files.contains(&path) && !context_manager.contains(&path).unwrap_or(false) {
            continue;
        }
        if access_satisfies(context_manager.effective_access_for(&path), request.access) {
            continue;
        }
        if context_manager.add_auto_file(&path, request.access).is_ok()
            && access_satisfies(context_manager.effective_access_for(&path), request.access)
        {
            added.push(path);
        }
    }
    added
}

pub(crate) fn has_proposed_edits(result: &ModelResult) -> bool {
    matches!(result, ModelResult::Edit { edits, .. } if !edits.is_empty())
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

        let added = apply_context_requests(&mut cm, &requests, &available, None);
        assert_eq!(added, vec!["a.rs".to_string()]);
        assert!(cm.is_editable("a.rs").unwrap());

        // Already satisfied: no further changes.
        assert!(apply_context_requests(&mut cm, &requests, &available, None).is_empty());

        cm.clear_auto();
        assert!(!cm.contains("a.rs").unwrap());
    }

    #[test]
    fn test_apply_context_requests_respects_max_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() {}").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["a.rs".to_string(), "b.rs".to_string()];
        let requests = vec![
            ContextRequest {
                path: "a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
            ContextRequest {
                path: "b.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        ];

        let added = apply_context_requests(&mut cm, &requests, &available, Some(1));
        assert_eq!(added.len(), 1);
        assert_eq!(added[0], "a.rs");
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

        let added = apply_context_requests(&mut cm, &requests, &available, None);
        assert_eq!(added, vec!["user.rs".to_string()]);
        assert!(cm.is_editable("user.rs").unwrap());
    }
}
