//! Plan horizon synthesis helpers.
//!
//! Architect tier resolution, the owned replanning diagnostics, and the
//! plan-wide context-file collection are the deterministic inputs of
//! [`super::engine::execute_plan_horizon_synthesis`]. They live here so the
//! synthesis engine itself stays a single-purpose turn driver.

use std::collections::HashSet;

use tauqe_protocol::{ArchitectTier, ModelRole, ModelSelection, Plan, PlanItem};

use crate::handlers::architect::architect_alias;
use crate::handlers::common::normalize_preload_path;

/// The Architect escalation applied to one replanning wave.
///
/// `from` carries the configuration recorded on the plan, which is `None` when
/// the plan predates provenance recording; `to` is the rung the ladder handed
/// out for this wave.
pub(super) struct ArchitectLevelUp {
    pub(super) from: Option<ArchitectTier>,
    pub(super) to: ArchitectTier,
}

/// Resolves the Architect configuration for one horizon synthesis round.
///
/// An explicit developer selection is an override: the architect ladder is not
/// consulted and no escalation is reported. Otherwise a replanning wave
/// (`diagnostics` present) extracts the configuration recorded on the plan and
/// asks the ladder for the next rung, while the first synthesis of a horizon
/// always starts from the base rung. The returned [`ArchitectLevelUp`] is
/// `Some` only when a replanning wave escalated the Architect.
pub(super) fn resolve_architect_tier(
    cfg: &tauqe_core::config::AppConfig,
    plan: &Plan,
    custom_model: Option<&ModelSelection>,
    diagnostics: Option<&PlanHorizonDiagnosticsOwned>,
) -> (ArchitectTier, Option<String>, Option<ArchitectLevelUp>) {
    if let Some(selection) = custom_model {
        let model = cfg.resolve_model(selection);
        let effort = cfg
            .models
            .resolve_selection_effort(ModelRole::Architect, selection);
        let tier = ArchitectTier::new(model, effort);
        let alias = architect_alias(cfg, &tier);
        return (tier, alias, None);
    }

    let tier = match diagnostics {
        Some(_) => cfg.models.next_architect_tier(plan.architect.as_ref()),
        None => cfg.models.architect_tier(0),
    };
    let level_up = diagnostics.map(|_| ArchitectLevelUp {
        from: plan.architect.clone(),
        to: tier.clone(),
    });
    let alias = architect_alias(cfg, &tier);
    (tier, alias, level_up)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ModelEntry, ModelRef, ReasoningEffort};

    #[test]
    fn test_resolve_architect_tier_custom_model_uses_its_own_effort() {
        let cfg = tauqe_core::config::AppConfig {
            models: tauqe_core::config::ModelsConfig {
                primary: vec![ModelRef::openrouter("base/primary").into()],
                architect: Some(vec![
                    ModelEntry::new(
                        ModelRef::openrouter("deepseek/deepseek-flash"),
                        Some(ReasoningEffort::Max),
                        None,
                    ),
                    ModelEntry::new(
                        ModelRef::openrouter("google/gemini-flash"),
                        Some(ReasoningEffort::High),
                        None,
                    ),
                    ModelEntry::from(ModelRef::openrouter("google/gemini-argon")),
                ]),
                ..Default::default()
            },
            ..Default::default()
        };

        let plan = Plan {
            id: "test-plan".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        };

        // 1. None selects tier 0 (deepseek with max)
        let (tier0, _, _) = resolve_architect_tier(&cfg, &plan, None, None);
        assert_eq!(
            tier0.model,
            tauqe_protocol::ModelRef::openrouter("deepseek/deepseek-flash")
        );
        assert_eq!(tier0.effort, Some(ReasoningEffort::Max));

        // 2. Custom selection of gemini-flash must use its OWN effort (high), NOT max from deepseek!
        let selection = ModelSelection::Specific(tauqe_protocol::ModelRef::openrouter(
            "google/gemini-flash",
        ));
        let (tier_custom, _, _) = resolve_architect_tier(&cfg, &plan, Some(&selection), None);
        assert_eq!(
            tier_custom.model,
            tauqe_protocol::ModelRef::openrouter("google/gemini-flash")
        );
        assert_eq!(tier_custom.effort, Some(ReasoningEffort::High));

        // 3. Custom selection of gemini-argon without effort returns None
        let selection_argon = ModelSelection::Specific(tauqe_protocol::ModelRef::openrouter(
            "google/gemini-argon",
        ));
        let (tier_argon, _, _) =
            resolve_architect_tier(&cfg, &plan, Some(&selection_argon), None);
        assert_eq!(
            tier_argon.model,
            tauqe_protocol::ModelRef::openrouter("google/gemini-argon")
        );
        assert_eq!(tier_argon.effort, None);
    }
}
