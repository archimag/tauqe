use tauqe_protocol::{ContextAccess, ModelResult};

use crate::config::DiscoveryMode;
use crate::context::ContextManager;
use crate::edits::protocol::ContextRequest;

pub(crate) fn access_satisfies(current: Option<ContextAccess>, requested: ContextAccess) -> bool {
    match current {
        Some(ContextAccess::Editable) => true,
        Some(ContextAccess::ReadOnly) => requested == ContextAccess::ReadOnly,
        None => false,
    }
}

/// Diagnostic details when requested files exceed the configured hard maximum context limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextLimitExceeded {
    pub current_count: usize,
    pub requested_count: usize,
    pub max_files: usize,
    pub requested_files: Vec<String>,
}

/// Outcome of applying context requests to ContextManager.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextApplicationOutcome {
    pub added: Vec<String>,
    pub dropped: Vec<String>,
    pub missing: Vec<String>,
    pub already_satisfied: Vec<String>,
    pub limit_exceeded: Option<ContextLimitExceeded>,
}

/// Reconciles the auto context layer based on the configured discovery strategy.
///
/// In `Monotonic` mode:
/// - Pure append-only growth; no evictions or drops occur.
/// - Requested files are added to the Auto layer or have their access upgraded.
///
/// In `Incremental` mode:
/// - Explicitly dropped files in `drops` are removed exclusively from the Auto layer.
/// - Attempts to drop User or Pinned files are safely ignored.
/// - Requested files are added to the Auto layer or have their access upgraded.
///
/// In `Snapshot` mode:
/// - Any files currently in the Auto layer that are omitted from `requests` (or present in `drops`)
///   are automatically evicted.
/// - User and Pinned files are strictly preserved and never evicted.
/// - Newly requested files are loaded into the Auto layer; already loaded files are retained.
///
/// The hard `max_files` limit is evaluated after eviction/reconciliation for any newly added files.
pub fn reconcile_discovery_context(
    context_manager: &mut ContextManager,
    mode: DiscoveryMode,
    requests: &[ContextRequest],
    drops: &[String],
    available_files: &[String],
    max_files: usize,
) -> ContextApplicationOutcome {
    let mut outcome = ContextApplicationOutcome::default();

    // 1. Eviction / drop phase
    match mode {
        DiscoveryMode::Monotonic => {
            // Pure append-only context growth: explicit drops and evictions are disabled.
        }
        DiscoveryMode::Incremental => {
            for drop_path in drops {
                let Ok(path) = context_manager.normalize_path(drop_path) else {
                    continue;
                };
                if context_manager.drop_auto_file(&path).unwrap_or(false)
                    && !outcome.dropped.contains(&path)
                {
                    outcome.dropped.push(path);
                }
            }
        }
        DiscoveryMode::Snapshot => {
            let mut desired_auto_paths = std::collections::BTreeSet::new();
            for req in requests {
                if let Ok(path) = context_manager.normalize_path(&req.path) {
                    desired_auto_paths.insert(path);
                }
            }
            let mut explicit_drops = std::collections::BTreeSet::new();
            for drop_path in drops {
                if let Ok(path) = context_manager.normalize_path(drop_path) {
                    explicit_drops.insert(path);
                }
            }

            for auto_path in context_manager.auto_files() {
                if (!desired_auto_paths.contains(&auto_path) || explicit_drops.contains(&auto_path))
                    && context_manager.drop_auto_file(&auto_path).unwrap_or(false)
                    && !outcome.dropped.contains(&auto_path)
                {
                    outcome.dropped.push(auto_path);
                }
            }
        }
    }

    // 2. Identify candidate additions and validate against repository
    let current_count = context_manager.read_context_files().len();
    let mut valid_new_files = Vec::new();

    for request in requests {
        let Ok(path) = context_manager.normalize_path(&request.path) else {
            if !outcome.missing.contains(&request.path) {
                outcome.missing.push(request.path.clone());
            }
            continue;
        };
        if outcome.dropped.contains(&path) {
            continue;
        }
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

    // 3. Check hard max_files limit post-eviction
    if !valid_new_files.is_empty() && current_count + valid_new_files.len() > max_files {
        outcome.limit_exceeded = Some(ContextLimitExceeded {
            current_count,
            requested_count: valid_new_files.len(),
            max_files,
            requested_files: valid_new_files,
        });
        return outcome;
    }

    // 4. Apply additions and access upgrades
    for request in requests {
        let Ok(path) = context_manager.normalize_path(&request.path) else {
            continue;
        };
        if outcome.missing.contains(&path)
            || outcome.already_satisfied.contains(&path)
            || outcome.dropped.contains(&path)
        {
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
/// Enforces the hard `max_files` limit across all context layers using Incremental strategy.
pub fn apply_context_requests_detailed(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
    max_files: usize,
) -> ContextApplicationOutcome {
    reconcile_discovery_context(
        context_manager,
        DiscoveryMode::Monotonic,
        requests,
        &[],
        available_files,
        max_files,
    )
}

/// Registers model-requested repository files in the Auto layer.
/// Returns the paths that actually widened the effective context.
pub fn apply_context_requests(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
    max_files: usize,
) -> Vec<String> {
    apply_context_requests_detailed(context_manager, requests, available_files, max_files).added
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

        let added = apply_context_requests(&mut cm, &requests, &available, 25);
        assert_eq!(added, vec!["a.rs".to_string()]);
        assert!(cm.is_editable("a.rs").unwrap());

        // Already satisfied: no further changes.
        assert!(apply_context_requests(&mut cm, &requests, &available, 25).is_empty());

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

        // Hard limit: max_files = 1. Model requests 2 files, which exceeds 1.
        let outcome = apply_context_requests_detailed(&mut cm, &requests, &available, 1);
        assert!(outcome.added.is_empty());
        let limit = outcome.limit_exceeded.expect("Expected limit_exceeded");
        assert_eq!(limit.current_count, 0);
        assert_eq!(limit.requested_count, 2);
        assert_eq!(limit.max_files, 1);
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

        let added = apply_context_requests(&mut cm, &requests, &available, 25);
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

        let outcome = apply_context_requests_detailed(&mut cm, &requests, &available, 25);
        assert!(outcome.added.is_empty());
        assert_eq!(outcome.already_satisfied, vec!["a.rs".to_string()]);
        assert_eq!(outcome.missing, vec!["missing.rs".to_string()]);
    }

    #[test]
    fn test_reconcile_incremental_mode_drops_auto_file_and_protects_user_layer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("user.rs"), "fn user() {}").unwrap();
        std::fs::write(dir.path().join("auto.rs"), "fn auto() {}").unwrap();
        std::fs::write(dir.path().join("next.rs"), "fn next() {}").unwrap();

        let mut cm = ContextManager::new(dir.path().to_path_buf());
        cm.add_file("user.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly).unwrap();

        let available = vec![
            "user.rs".to_string(),
            "auto.rs".to_string(),
            "next.rs".to_string(),
        ];
        let requests = vec![ContextRequest {
            path: "next.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }];
        let drops = vec!["auto.rs".to_string(), "user.rs".to_string()];

        let outcome = reconcile_discovery_context(
            &mut cm,
            DiscoveryMode::Incremental,
            &requests,
            &drops,
            &available,
            10,
        );

        assert_eq!(outcome.dropped, vec!["auto.rs".to_string()]);
        assert_eq!(outcome.added, vec!["next.rs".to_string()]);
        // user.rs is protected and stays intact
        assert!(cm.contains("user.rs").unwrap());
        assert!(!cm.contains("auto.rs").unwrap());
        assert!(cm.contains("next.rs").unwrap());
    }

    #[test]
    fn test_reconcile_snapshot_mode_evicts_omitted_auto_files_preserving_pinned_and_user() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pinned.rs"), "fn pinned() {}").unwrap();
        std::fs::write(dir.path().join("user.rs"), "fn user() {}").unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn keep() {}").unwrap();
        std::fs::write(dir.path().join("stale.rs"), "fn stale() {}").unwrap();
        std::fs::write(dir.path().join("new.rs"), "fn new() {}").unwrap();

        let mut cm = ContextManager::new(dir.path().to_path_buf());
        cm.load_pinned(&["pinned.rs".to_string()]);
        cm.add_file("user.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_auto_file("keep.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_auto_file("stale.rs", ContextAccess::ReadOnly).unwrap();

        let available = vec![
            "pinned.rs".to_string(),
            "user.rs".to_string(),
            "keep.rs".to_string(),
            "stale.rs".to_string(),
            "new.rs".to_string(),
        ];

        // Desired snapshot: keep.rs and new.rs. stale.rs is omitted.
        let requests = vec![
            ContextRequest {
                path: "keep.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
            ContextRequest {
                path: "new.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        ];

        let outcome = reconcile_discovery_context(
            &mut cm,
            DiscoveryMode::Snapshot,
            &requests,
            &[],
            &available,
            10,
        );

        assert_eq!(outcome.dropped, vec!["stale.rs".to_string()]);
        assert_eq!(outcome.added, vec!["new.rs".to_string()]);
        assert_eq!(outcome.already_satisfied, vec!["keep.rs".to_string()]);

        // Invariants: pinned and user preserved
        assert!(cm.contains("pinned.rs").unwrap());
        assert!(cm.contains("user.rs").unwrap());
        assert!(cm.contains("keep.rs").unwrap());
        assert!(cm.contains("new.rs").unwrap());
        assert!(!cm.contains("stale.rs").unwrap());
    }

    #[test]
    fn test_reconcile_max_files_limit_evaluated_post_eviction() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("base.rs"), "fn base() {}").unwrap();
        std::fs::write(dir.path().join("stale.rs"), "fn stale() {}").unwrap();
        std::fs::write(dir.path().join("new.rs"), "fn new() {}").unwrap();

        let mut cm = ContextManager::new(dir.path().to_path_buf());
        cm.add_file("base.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_auto_file("stale.rs", ContextAccess::ReadOnly).unwrap();

        let available = vec![
            "base.rs".to_string(),
            "stale.rs".to_string(),
            "new.rs".to_string(),
        ];

        // max_files = 2.
        // Initially context has 2 files (base.rs, stale.rs).
        // In snapshot mode, stale.rs is omitted, freeing 1 slot.
        // new.rs can then fit within max_files = 2!
        let requests = vec![ContextRequest {
            path: "new.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }];

        let outcome = reconcile_discovery_context(
            &mut cm,
            DiscoveryMode::Snapshot,
            &requests,
            &[],
            &available,
            2,
        );

        assert!(outcome.limit_exceeded.is_none());
        assert_eq!(outcome.dropped, vec!["stale.rs".to_string()]);
        assert_eq!(outcome.added, vec!["new.rs".to_string()]);
        assert!(cm.contains("new.rs").unwrap());
        assert!(!cm.contains("stale.rs").unwrap());
    }

    #[test]
    fn test_reconcile_incremental_multi_round_dead_ends() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mod_a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.path().join("dead_end.rs"), "fn dead() {}").unwrap();
        std::fs::write(dir.path().join("mod_b.rs"), "fn b() {}").unwrap();

        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec![
            "mod_a.rs".to_string(),
            "dead_end.rs".to_string(),
            "mod_b.rs".to_string(),
        ];

        // Round 1: Model requests mod_a.rs and dead_end.rs
        let round1_requests = vec![
            ContextRequest {
                path: "mod_a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
            ContextRequest {
                path: "dead_end.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        ];
        let outcome1 = reconcile_discovery_context(
            &mut cm,
            DiscoveryMode::Incremental,
            &round1_requests,
            &[],
            &available,
            10,
        );
        assert_eq!(outcome1.added.len(), 2);
        assert!(cm.contains("mod_a.rs").unwrap());
        assert!(cm.contains("dead_end.rs").unwrap());

        // Round 2: Model identifies dead_end.rs as investigative dead end, drops it and requests mod_b.rs
        let round2_requests = vec![
            ContextRequest {
                path: "mod_a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
            ContextRequest {
                path: "mod_b.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        ];
        let round2_drops = vec!["dead_end.rs".to_string()];

        let outcome2 = reconcile_discovery_context(
            &mut cm,
            DiscoveryMode::Incremental,
            &round2_requests,
            &round2_drops,
            &available,
            10,
        );

        assert_eq!(outcome2.dropped, vec!["dead_end.rs".to_string()]);
        assert_eq!(outcome2.added, vec!["mod_b.rs".to_string()]);
        assert_eq!(outcome2.already_satisfied, vec!["mod_a.rs".to_string()]);

        assert!(cm.contains("mod_a.rs").unwrap());
        assert!(cm.contains("mod_b.rs").unwrap());
        assert!(!cm.contains("dead_end.rs").unwrap());
    }

    #[test]
    fn test_reconcile_snapshot_multi_round_with_access_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shared.rs"), "fn shared() {}").unwrap();
        std::fs::write(dir.path().join("temp.rs"), "fn temp() {}").unwrap();
        std::fs::write(dir.path().join("feature.rs"), "fn feature() {}").unwrap();

        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec![
            "shared.rs".to_string(),
            "temp.rs".to_string(),
            "feature.rs".to_string(),
        ];

        // Round 1: Model requests shared.rs and temp.rs (both ReadOnly)
        let round1_requests = vec![
            ContextRequest {
                path: "shared.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
            ContextRequest {
                path: "temp.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        ];
        reconcile_discovery_context(
            &mut cm,
            DiscoveryMode::Snapshot,
            &round1_requests,
            &[],
            &available,
            10,
        );
        assert!(!cm.is_editable("shared.rs").unwrap());

        // Round 2: Model declares new snapshot: shared.rs upgraded to Editable, feature.rs added, temp.rs omitted
        let round2_requests = vec![
            ContextRequest {
                path: "shared.rs".to_string(),
                access: ContextAccess::Editable,
            },
            ContextRequest {
                path: "feature.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        ];
        let outcome2 = reconcile_discovery_context(
            &mut cm,
            DiscoveryMode::Snapshot,
            &round2_requests,
            &[],
            &available,
            10,
        );

        assert_eq!(outcome2.dropped, vec!["temp.rs".to_string()]);
        assert!(outcome2.added.contains(&"shared.rs".to_string()));
        assert!(outcome2.added.contains(&"feature.rs".to_string()));

        assert!(cm.is_editable("shared.rs").unwrap());
        assert!(cm.contains("feature.rs").unwrap());
        assert!(!cm.contains("temp.rs").unwrap());
    }

    #[test]
    fn test_reconcile_layer_protection_invariant_against_drops_and_eviction() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pinned.rs"), "fn pinned() {}").unwrap();
        std::fs::write(dir.path().join("user.rs"), "fn user() {}").unwrap();
        std::fs::write(dir.path().join("auto.rs"), "fn auto() {}").unwrap();

        let mut cm = ContextManager::new(dir.path().to_path_buf());
        cm.load_pinned(&["pinned.rs".to_string()]);
        cm.add_file("user.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly).unwrap();

        let available = vec![
            "pinned.rs".to_string(),
            "user.rs".to_string(),
            "auto.rs".to_string(),
        ];

        // 1. Incremental mode with explicit drops targeting user and pinned files
        let drops = vec![
            "pinned.rs".to_string(),
            "user.rs".to_string(),
            "auto.rs".to_string(),
        ];
        let outcome_inc = reconcile_discovery_context(
            &mut cm,
            DiscoveryMode::Incremental,
            &[],
            &drops,
            &available,
            10,
        );

        // Only auto.rs is dropped; pinned and user are strictly immune
        assert_eq!(outcome_inc.dropped, vec!["auto.rs".to_string()]);
        assert!(cm.contains("pinned.rs").unwrap());
        assert!(cm.contains("user.rs").unwrap());
        assert!(!cm.contains("auto.rs").unwrap());

        // Re-add auto.rs
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly).unwrap();

        // 2. Snapshot mode with empty requests and explicit drops targeting pinned and user
        let outcome_snap = reconcile_discovery_context(
            &mut cm,
            DiscoveryMode::Snapshot,
            &[],
            &drops,
            &available,
            10,
        );

        assert_eq!(outcome_snap.dropped, vec!["auto.rs".to_string()]);
        assert!(cm.contains("pinned.rs").unwrap());
        assert!(cm.contains("user.rs").unwrap());
        assert!(!cm.contains("auto.rs").unwrap());
    }

    #[test]
    fn test_reconcile_monotonic_mode_ignores_drops_and_accumulates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() {}").unwrap();

        let mut cm = ContextManager::new(dir.path().to_path_buf());
        cm.add_auto_file("a.rs", ContextAccess::ReadOnly).unwrap();

        let available = vec!["a.rs".to_string(), "b.rs".to_string()];
        let requests = vec![ContextRequest {
            path: "b.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }];
        let drops = vec!["a.rs".to_string()];

        let outcome = reconcile_discovery_context(
            &mut cm,
            DiscoveryMode::Monotonic,
            &requests,
            &drops,
            &available,
            10,
        );

        assert!(outcome.dropped.is_empty());
        assert_eq!(outcome.added, vec!["b.rs".to_string()]);
        assert!(cm.contains("a.rs").unwrap());
        assert!(cm.contains("b.rs").unwrap());
    }

    #[test]
    fn test_reconcile_handles_unnormalized_and_malformed_drop_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("auto.rs"), "fn auto() {}").unwrap();

        let mut cm = ContextManager::new(dir.path().to_path_buf());
        cm.add_auto_file("auto.rs", ContextAccess::ReadOnly).unwrap();
        let available = vec!["auto.rs".to_string()];

        let drops = vec![
            "./auto.rs".to_string(),
            "../escaping_root.rs".to_string(),
            "".to_string(),
        ];

        let outcome = reconcile_discovery_context(
            &mut cm,
            DiscoveryMode::Incremental,
            &[],
            &drops,
            &available,
            10,
        );

        assert_eq!(outcome.dropped, vec!["auto.rs".to_string()]);
        assert!(!cm.contains("auto.rs").unwrap());
    }
}
