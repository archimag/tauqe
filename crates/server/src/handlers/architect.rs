//! Architect role subsystem.
//!
//! Provider resolution, tier rendering, and the launch skeleton of the shared
//! structured contour are the three seams plan horizon synthesis and review group
//! decomposition share: both are the same Architect operation, so they run on
//! exactly one implementation here instead of a copy per subsystem.
//!
//! The contour itself — the proposal rounds, the discovery journal, the schema
//! healing, the empty-answer budget, and the tier ladder — lives in `crates/core`,
//! so the daemon carries no domain logic of a synthesis. This module keeps only the
//! adapter-side seams: the provider client a promoted rung is resolved through, the
//! developer-facing rendering of an architect configuration, and the bridge that
//! translates contour telemetry into the wire events a client renders.
//!
//! Tier resolution and the lazy level-up escalation invariant are domain policy
//! owned by the core engine (`tauqe_core::plan::prompt::resolve_architect_tier`),
//! so this module renders the configuration that policy hands back and never
//! re-derives the ladder itself.

mod bridge;

// Both Architect contours of the daemon run on the shared launch skeleton of the
// bridge, which owns the provider seam, the telemetry relay, and the context
// merge. The resolver and the relay summary travel with it so an adapter can name
// the seam it implements instead of reconstructing the type.
#[allow(unused_imports)]
pub(crate) use bridge::{run_contour_synthesis, ArchitectProviderResolver, ContourRelaySummary};

use std::sync::Arc;

use tauqe_core::config::AppConfig;
use tauqe_core::plan::prompt::ArchitectLevelUp;
use tauqe_core::providers::{create_provider, LlmProvider};
use tauqe_protocol::ArchitectTier;

use crate::state::AppState;

/// Resolves the LLM provider for architect plan operations (refine and replan),
/// consulting the test override in test configurations before falling back to
/// network provider construction.
pub(crate) async fn resolve_architect_provider(
    _state: &Arc<AppState>,
    cfg: &AppConfig,
    provider_kind: tauqe_protocol::ProviderKind,
) -> Result<Arc<dyn LlmProvider>, String> {
    #[cfg(test)]
    {
        if let Some(p) = _state.provider_override.lock().await.clone() {
            return Ok(p);
        }
    }
    create_provider(provider_kind, cfg)
        .map(Arc::from)
        .map_err(|err| err.to_string())
}

/// Renders an Architect configuration for user-facing banners and logs.
pub(crate) fn format_architect_tier(tier: &ArchitectTier) -> String {
    match tier.effort {
        Some(effort) => format!("{} ({})", tier.model, effort),
        None => tier.model.to_string(),
    }
}

/// Renders the Architect configuration note published with a synthesis round.
///
/// A round that escalated reports the rung it entered. A replanning round that runs
/// on a configuration the automatic ladder excludes (`level_up = false`) reports
/// that escalation is unavailable instead, so a developer who pinned the Architect
/// by hand sees why no stronger rung was engaged rather than assuming a silent
/// downgrade.
pub(crate) fn format_architect_escalation_note(
    tier: &ArchitectTier,
    level_up: Option<&ArchitectLevelUp>,
    escalation_unavailable: bool,
) -> String {
    if let Some(level) = level_up {
        let from = level
            .from
            .as_ref()
            .map(format_architect_tier)
            .unwrap_or_else(|| "not recorded".to_string());
        return format!(
            "\n> Architect level up: {} → {}",
            from,
            format_architect_tier(&level.to)
        );
    }
    if escalation_unavailable {
        return format!(
            "\n> Architect escalation unavailable: `{}` is excluded from the automatic ladder (`level_up = false`), so this round stays on the selected configuration.",
            format_architect_tier(tier)
        );
    }
    String::new()
}
