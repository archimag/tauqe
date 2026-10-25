//! Plan horizon synthesis helpers.
//!
//! The owned replanning diagnostics and the plan-wide context-file collection
//! are the deterministic inputs of
//! [`super::engine::execute_plan_horizon_synthesis`]. They live here so the
//! synthesis engine itself stays a single-purpose turn driver.
//!
//! The Architect tier is deliberately absent: the lazy level up invariant is
//! domain policy shared with review finding decomposition, so
//! `tauqe_core::plan::prompt::resolve_architect_tier` owns it and the synthesis
//! engine calls that policy directly.

use std::collections::HashSet;

use tauqe_protocol::{Plan, PlanItem};

use crate::handlers::common::normalize_preload_path;

/// Diagnostics describing a failed or blocked step for horizon synthesis.
pub(super) struct PlanHorizonDiagnosticsOwned {
    pub blocked_step_id: String,
    pub blocked_reason: Option<String>,
    pub toolchain_status: Option<String>,
}

/// Recursively collects every declared target (`files`) and reference
/// (`readonly_files`) path across the whole plan, deduplicating normalized
/// paths while preserving first-seen order.
///
/// Leaves are the authoritative holders of declared files (see the leaf-only
/// file invariant), but the walk visits every node so a plan carrying residual
/// parent files still contributes them. The result feeds
/// `preload_readonly_files`, giving the architect visibility of every file an
/// existing step already targets or references rather than a single step's.
pub(super) fn collect_plan_context_files(plan: &Plan) -> Vec<String> {
    fn walk(items: &[PlanItem], out: &mut Vec<String>, seen: &mut HashSet<String>) {
        for item in items {
            for path in item.files.iter().chain(item.readonly_files.iter()) {
                let normalized = normalize_preload_path(path);
                if normalized.is_empty() {
                    continue;
                }
                if seen.insert(normalized.to_string()) {
                    out.push(normalized.to_string());
                }
            }
            walk(&item.children, out, seen);
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk(&plan.items, &mut out, &mut seen);
    out
}
