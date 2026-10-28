//! Architect tier-resolution policy.
//!
//! Plan horizon synthesis and review finding decomposition are the same
//! Architect operation, so both resolve the configuration they run on through
//! exactly one function here instead of re-deriving the policy locally. The
//! policy reads three deterministic inputs — an explicit developer selection,
//! the configuration recorded on the entity, and the failed step the round
//! answers — and yields the rung to run together with a typed escalation report
//! the interface renders.
//!
//! The lazy level up invariant lives here: a stronger Architect is spent only on
//! a failure that already happened twice, because only then has the recorded
//! configuration demonstrably failed. Keeping the invariant in one place stops
//! plan and review escalation from drifting apart.

use crate::config::AppConfig;
use tauqe_protocol::{ArchitectTier, ModelRole, ModelSelection};

/// One Architect escalation applied to a horizon.
///
/// `from` carries the configuration recorded on the plan or the finding, which
/// is `None` only when the entity predates provenance recording; `to` is the
/// rung the ladder handed out for this round.
pub struct ArchitectLevelUp {
    pub from: Option<ArchitectTier>,
    pub to: ArchitectTier,
}

/// Canonical comparison key for a plan or subplan step identifier.
///
/// A blocked step identifier arrives from a failed turn, while the recorded
/// identifier was authored by the Architect, so the two may differ by case or
/// surrounding whitespace. Trimming and ASCII-lowercasing collapses those
/// differences, so "2", " 2 ", and "2" compare as one step and a repeated
/// failure is recognized instead of being mistaken for a fresh one.
fn normalize_step_id(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

/// Reports whether a step has already failed a reprojection round.
///
/// This is the single predicate behind the lazy level up invariant: an
/// escalation is justified only when the step that just failed is the very step
/// whose horizon was already reprojected, because only then has the recorded
/// Architect configuration demonstrably failed twice.
fn is_repeated_replan(step_id: &str, last_replanned_step: Option<&str>) -> bool {
    match last_replanned_step {
        Some(last) => normalize_step_id(last) == normalize_step_id(step_id),
        None => false,
    }
}

/// Resolves the Architect configuration for one synthesis round.
///
/// An explicit developer selection is an override: the ladder is not consulted
/// and no escalation is reported. The first synthesis of a horizon always starts
/// from the base rung.
///
/// A replanning round (`failed_step_id` present) applies the lazy level up
/// invariant: the configuration recorded on the entity is reused while the
/// failed step has never been reprojected, so the first failure of a step is
/// answered by the Architect that planned the horizon. Only a repeated failure of
/// the same step escalates to the next rung, because only then has the recorded
/// configuration demonstrably failed twice. The returned [`ArchitectLevelUp`] is
/// `Some` exactly when this round escalated the Architect.
pub fn resolve_architect_tier(
    cfg: &AppConfig,
    recorded: Option<&ArchitectTier>,
    custom_model: Option<&ModelSelection>,
    failed_step_id: Option<&str>,
    last_replanned_step: Option<&str>,
) -> (ArchitectTier, Option<ArchitectLevelUp>) {
    if let Some(selection) = custom_model {
        let model = cfg.resolve_model(selection);
        let effort = cfg
            .models
            .resolve_selection_effort(ModelRole::Architect, selection);
        return (ArchitectTier::new(model, effort), None);
    }

    let Some(failed_step_id) = failed_step_id else {
        return (cfg.models.architect_tier(0), None);
    };

    if !is_repeated_replan(failed_step_id, last_replanned_step) {
        let tier = recorded
            .cloned()
            .unwrap_or_else(|| cfg.models.architect_tier(0));
        return (tier, None);
    }

    // A recorded rung the ladder excludes by `level_up = false` is a configuration
    // the developer pinned by hand: the ladder is already saturated, so the
    // reprojection stays on it and no level up is reported.
    if let Some(tier) = recorded.filter(|tier| {
        cfg.models
            .excluded_from_escalation(ModelRole::Architect, &tier.model)
    }) {
        return (tier.clone(), None);
    }

    let tier = cfg.models.next_architect_tier(recorded);
    let level_up = ArchitectLevelUp {
        from: recorded.cloned(),
        to: tier.clone(),
    };
    (tier, Some(level_up))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelsConfig;
    use tauqe_protocol::{ModelEntry, ModelRef, ReasoningEffort};

    /// Builds a configuration whose Architect ladder is the given list.
    fn config_with_ladder(architect: Vec<ModelEntry>) -> AppConfig {
        AppConfig {
            models: ModelsConfig {
                primary: vec![ModelRef::openrouter("base/primary").into()],
                architect: Some(architect),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn custom_selection_uses_its_own_effort_and_never_escalates() {
        let cfg = config_with_ladder(vec![
            ModelEntry::new(
                ModelRef::openrouter("deepseek/deepseek-flash"),
                Some(ReasoningEffort::Max),
            ),
            ModelEntry::new(
                ModelRef::openrouter("google/gemini-flash"),
                Some(ReasoningEffort::High),
            ),
            ModelEntry::from(ModelRef::openrouter("google/gemini-argon")),
        ]);

        // A specific selection keeps its own configured depth instead of
        // borrowing the `max` of the base rung.
        let selection = ModelSelection::Specific(ModelRef::openrouter("google/gemini-flash"));
        let (tier, level_up) = resolve_architect_tier(&cfg, None, Some(&selection), None, None);
        assert_eq!(tier.model, ModelRef::openrouter("google/gemini-flash"));
        assert_eq!(tier.effort, Some(ReasoningEffort::High));
        assert!(level_up.is_none());

        // A selection whose entry declares no depth resolves to no effort.
        let bare = ModelSelection::Specific(ModelRef::openrouter("google/gemini-argon"));
        let (tier, level_up) = resolve_architect_tier(&cfg, None, Some(&bare), None, None);
        assert_eq!(tier.model, ModelRef::openrouter("google/gemini-argon"));
        assert_eq!(tier.effort, None);
        assert!(level_up.is_none());

        // A model outside the ladder keeps the selection and declares no depth.
        // The override runs even for a repeated failure, so it can never be
        // silently escalated to the base rung.
        let unknown = ModelSelection::Specific(ModelRef::openrouter("unknown/model"));
        let (tier, level_up) =
            resolve_architect_tier(&cfg, None, Some(&unknown), Some("s-1"), Some("s-1"));
        assert_eq!(tier.model, ModelRef::openrouter("unknown/model"));
        assert_eq!(tier.effort, None);
        assert!(
            level_up.is_none(),
            "an explicit developer selection never escalates"
        );
    }

    #[test]
    fn escalation_is_deferred_until_a_step_fails_twice() {
        let cfg = config_with_ladder(vec![
            ModelEntry::new(ModelRef::openrouter("planner/one"), None),
            ModelEntry::new(ModelRef::openrouter("planner/two"), None),
        ]);
        let tier_one = ArchitectTier::from(ModelRef::openrouter("planner/one"));
        let tier_two = ArchitectTier::from(ModelRef::openrouter("planner/two"));

        // The first synthesis of a horizon starts from the base rung.
        let (tier, level_up) = resolve_architect_tier(&cfg, Some(&tier_one), None, None, None);
        assert_eq!(tier, tier_one);
        assert!(level_up.is_none());

        // The first failure of a never-reprojected step is answered by the
        // Architect that planned the horizon, so nothing escalates.
        let (tier, level_up) =
            resolve_architect_tier(&cfg, Some(&tier_one), None, Some("s-1"), None);
        assert_eq!(tier, tier_one);
        assert!(level_up.is_none());

        // Without provenance the base rung still answers the first failure.
        let (tier, level_up) = resolve_architect_tier(&cfg, None, None, Some("s-1"), None);
        assert_eq!(tier, tier_one);
        assert!(level_up.is_none());

        // A failure of a different step is again a first failure.
        let (tier, level_up) =
            resolve_architect_tier(&cfg, Some(&tier_one), None, Some("s-2"), Some("s-1"));
        assert_eq!(tier, tier_one);
        assert!(level_up.is_none());

        // A repeated failure of the same step escalates exactly one rung and
        // reports both configurations. The comparison tolerates the whitespace
        // and casing differences of a reported step identifier.
        let (tier, level_up) =
            resolve_architect_tier(&cfg, Some(&tier_one), None, Some(" S-1 "), Some("s-1"));
        assert_eq!(tier, tier_two);
        let level_up = level_up.expect("a repeated failure of the same step must escalate");
        assert_eq!(level_up.from, Some(tier_one));
        assert_eq!(level_up.to, tier_two);

        // The ladder saturates on its strongest rung instead of panicking.
        let (tier, level_up) =
            resolve_architect_tier(&cfg, Some(&tier_two), None, Some("s-1"), Some("s-1"));
        assert_eq!(tier, tier_two);
        let level_up = level_up.expect("a repeated failure always reports the escalation");
        assert_eq!(level_up.from, Some(tier_two.clone()));
        assert_eq!(level_up.to, tier_two);
    }

    #[test]
    fn manual_selection_on_an_excluded_architect_stays_saturated() {
        let manual_model = ModelRef::openrouter("planner/manual");
        let mut manual_entry = ModelEntry::new(manual_model.clone(), None);
        manual_entry.level_up = Some(false);
        let cfg = config_with_ladder(vec![
            ModelEntry::new(ModelRef::openrouter("planner/one"), None),
            manual_entry,
            ModelEntry::new(ModelRef::openrouter("planner/two"), None),
        ]);
        let manual = ArchitectTier::from(manual_model);

        // A repeated failure of the same step normally escalates one rung. The
        // recorded configuration is excluded from the ladder though, so it is a
        // manual run the developer pinned: the round stays on it and reports no
        // level up instead of silently restarting at the base rung.
        let (tier, level_up) =
            resolve_architect_tier(&cfg, Some(&manual), None, Some("s-1"), Some("s-1"));
        assert_eq!(tier, manual);
        assert!(level_up.is_none());

        // An explicit developer selection naming the excluded model never escalates
        // either, so both entry points honour the pinned configuration.
        let selection = ModelSelection::Specific(manual.model.clone());
        let (tier, level_up) =
            resolve_architect_tier(&cfg, None, Some(&selection), Some("s-1"), Some("s-1"));
        assert_eq!(tier, manual);
        assert!(level_up.is_none());
    }
}
