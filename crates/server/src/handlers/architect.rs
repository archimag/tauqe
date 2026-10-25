//! Architect role subsystem.
//!
//! Provider resolution, tier rendering, streaming structured-chat turns, and the
//! multi-round discovery loop are shared by plan horizon synthesis and review
//! finding decomposition: both are the same Architect operation, so they run on
//! exactly one implementation here instead of a copy per subsystem.
//!
//! Tier resolution and the lazy level-up escalation invariant are domain policy
//! owned by the core engine (`tauqe_core::plan::prompt::resolve_architect_tier`),
//! so this module renders the configuration that policy hands back and never
//! re-derives the ladder itself.

mod discovery;
mod stream;

use std::sync::Arc;

use tauqe_core::config::AppConfig;
use tauqe_core::providers::{create_provider, LlmProvider};
use tauqe_protocol::ArchitectTier;

use crate::state::AppState;

pub(crate) use discovery::{
    run_architect_discovery_loop, ArchitectDiscoveryError, ArchitectDiscoveryOutcome,
    ArchitectDiscoveryRequest,
};

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
