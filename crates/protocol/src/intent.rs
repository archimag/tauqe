//! Semantic model of the bounded turn intent.
//!
//! The harness upholds the invariant *"Never guess what is deterministically
//! known"*: at the moment a turn starts, the calling code already knows where
//! the turn came from and what it is allowed to do. The turn mode therefore
//! travels as a typed value, [`TurnIntent`], and is never inferred from the
//! text of a prompt — no prefix checks, no keyword scans, no regular
//! expressions over free-form strings.
//!
//! Inferring a mode from prompt text is a silent failure waiting to happen: a
//! user message that merely resembles an internal heading could switch the
//! turn into the wrong mode, and any wording change in prompt assembly would
//! silently alter behavior. A typed value removes that failure class, keeps a
//! single source of truth, and makes every turn declare its intent explicitly.
//!
//! The type is deliberately kept free of protocol-level orchestration logic.
//! Mapping an external target (for example a discussion anchor) onto an intent
//! belongs to the caller, so this module stays a pure declarative vocabulary.
//! Its only protocol dependency is [`TurnIntent::narrow_capabilities`], a pure
//! mapping onto the capability vocabulary: the intent is the single place that
//! decides which capabilities a turn advertises and accepts.
//!
//! [`TurnIntent`] intentionally implements neither `Default` nor any implicit
//! conversion: every turn must name its mode, because a default value would
//! turn a forgotten argument into a silent mode substitution.

use crate::command::{CapabilityProfile, CapabilitySet, LlmCapability};
use serde::{Deserialize, Serialize};

/// Mode of a bounded turn, declared by the caller that starts it.
///
/// The variants are mutually exclusive execution regimes:
///
/// - [`TurnIntent::InteractiveRepl`] is a free interactive dialogue started
///   from the REPL, including a dialogue anchored on a review session.
/// - [`TurnIntent::PlanDiscussion`] is an architectural discussion of a plan.
/// - [`TurnIntent::IsolatedStep`] is the isolated execution of one plan step
///   or one review finding.
///
/// The value is `Copy` and freely shareable, because it is a pure mode tag
/// rather than turn state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnIntent {
    /// Free interactive dialogue started from the REPL.
    InteractiveRepl,
    /// Architectural discussion of a plan.
    PlanDiscussion,
    /// Isolated execution of one plan step or one review finding.
    IsolatedStep,
}

impl TurnIntent {
    /// Reports whether this is a free interactive REPL dialogue.
    pub fn is_interactive_repl(self) -> bool {
        matches!(self, Self::InteractiveRepl)
    }

    /// Reports whether this is an isolated plan-step or review-finding run.
    pub fn is_isolated_step(self) -> bool {
        matches!(self, Self::IsolatedStep)
    }

    /// Reports whether this is a plan discussion.
    pub fn is_plan_discussion(self) -> bool {
        matches!(self, Self::PlanDiscussion)
    }
    /// Returns the regime profile this intent composes.
    ///
    /// The mapping is the single source of truth for the pair of typed lists a
    /// turn composes: the core surface that renders into the byte-stable head
    /// shared by every dialogue contour, and the contour delta that renders into
    /// the trailing user message. The review discussion inherits
    /// [`TurnIntent::InteractiveRepl`], so it composes the shared head with an
    /// empty delta instead of a profile of its own.
    pub fn regime_profile(self) -> CapabilityProfile {
        match self {
            Self::InteractiveRepl => CapabilityProfile::interactive_repl(),
            Self::PlanDiscussion => CapabilityProfile::discussion(),
            Self::IsolatedStep => CapabilityProfile::isolated_step(),
        }
    }
    /// Narrows a protocol capability set to what this turn intent permits.
    ///
    /// This mapping is the single source of truth for the turn's capability
    /// surface: prompt composition, the structured response schema, the stream
    /// filter, and command decoding all narrow through it, so a capability can
    /// never be advertised in the instructions while remaining undecodable, nor
    /// accepted by the decoder while being hidden from the instructions.
    ///
    /// - [`TurnIntent::InteractiveRepl`] keeps the protocol's own surface.
    /// - [`TurnIntent::PlanDiscussion`] keeps the discussion profile: the
    ///   dialogue head plus the plan mutation the regime authorizes.
    /// - [`TurnIntent::IsolatedStep`] keeps the isolated-step profile and drops
    ///   toolchain verification, because the harness owns verification for a
    ///   step deterministically.
    ///
    /// The isolated-step profile carries neither plan authoring nor
    /// documentation requests: a step executes an approved design inside the
    /// codebase, so knowledge about the harness itself is never part of its
    /// surface.
    ///
    /// The narrowing always intersects with a regime profile, never with a
    /// wire-format profile, so both transports converge on the same capability
    /// surface for a given intent.
    pub fn narrow_capabilities(self, capabilities: &CapabilitySet) -> CapabilitySet {
        match self {
            Self::InteractiveRepl => capabilities.clone(),
            Self::PlanDiscussion => Self::intersect(capabilities, &CapabilitySet::discussion()),
            Self::IsolatedStep => Self::intersect(capabilities, &CapabilitySet::isolated_step())
                .without(LlmCapability::ToolchainVerify),
        }
    }

    /// Keeps only the capabilities present in both sets, preserving input order.
    fn intersect(capabilities: &CapabilitySet, allowed: &CapabilitySet) -> CapabilitySet {
        let kept: Vec<LlmCapability> = capabilities
            .iter()
            .copied()
            .filter(|capability| allowed.contains(*capability))
            .collect();
        CapabilitySet::from_slice(&kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [TurnIntent; 3] = [
        TurnIntent::InteractiveRepl,
        TurnIntent::PlanDiscussion,
        TurnIntent::IsolatedStep,
    ];

    #[test]
    fn test_predicates_are_mutually_exclusive() {
        for intent in ALL {
            let flags = [
                intent.is_interactive_repl(),
                intent.is_plan_discussion(),
                intent.is_isolated_step(),
            ];
            // Exactly one predicate holds for every variant, which is the
            // contract the execution pipeline relies on when it branches.
            assert_eq!(
                flags.iter().filter(|held| **held).count(),
                1,
                "intent {intent:?} must satisfy exactly one predicate"
            );
        }

        assert!(TurnIntent::InteractiveRepl.is_interactive_repl());
        assert!(TurnIntent::PlanDiscussion.is_plan_discussion());
        assert!(TurnIntent::IsolatedStep.is_isolated_step());
    }

    #[test]
    fn test_serde_snake_case_roundtrip() {
        let cases = [
            (TurnIntent::InteractiveRepl, "\"interactive_repl\""),
            (TurnIntent::PlanDiscussion, "\"plan_discussion\""),
            (TurnIntent::IsolatedStep, "\"isolated_step\""),
        ];

        for (intent, encoded) in cases {
            assert_eq!(serde_json::to_string(&intent).unwrap(), encoded);
            let decoded: TurnIntent = serde_json::from_str(encoded).unwrap();
            assert_eq!(decoded, intent);
        }
    }

    #[test]
    fn test_narrow_capabilities_is_identity_for_interactive_repl() {
        let advertised = CapabilitySet::from_slice(LlmCapability::ALL);
        assert_eq!(
            TurnIntent::InteractiveRepl.narrow_capabilities(&advertised),
            advertised
        );
    }

    #[test]
    fn test_narrow_capabilities_applies_the_discussion_profile() {
        let advertised = CapabilitySet::from_slice(LlmCapability::ALL);
        let narrowed = TurnIntent::PlanDiscussion.narrow_capabilities(&advertised);

        assert!(narrowed.contains(LlmCapability::TextMessage));
        assert!(narrowed.contains(LlmCapability::Discovery));
        assert!(narrowed.contains(LlmCapability::DocRequest));
        assert!(narrowed.contains(LlmCapability::PlanCreation));
        assert!(narrowed.contains(LlmCapability::PlanUpdate));
        // A plan discussion carries the whole dialogue surface, so the developer
        // may ask for the fix directly instead of leaving the discussion.
        assert!(narrowed.contains(LlmCapability::FileEdits));
        assert!(narrowed.contains(LlmCapability::ToolchainVerify));
        assert!(narrowed.contains(LlmCapability::Scratchpad));
        assert!(narrowed.contains(LlmCapability::Outcome));
        assert!(narrowed.contains(LlmCapability::Blocked));
        assert!(narrowed.contains(LlmCapability::Abort));
        assert!(narrowed.contains(LlmCapability::CustomTools));
        assert!(narrowed.contains(LlmCapability::UserLanguage));
        assert_eq!(narrowed, CapabilitySet::discussion());
    }

    #[test]
    fn test_narrow_capabilities_isolated_step_drops_plan_verification_and_documentation() {
        let advertised = CapabilitySet::from_slice(LlmCapability::ALL);
        let narrowed = TurnIntent::IsolatedStep.narrow_capabilities(&advertised);

        assert!(narrowed.contains(LlmCapability::FileEdits));
        assert!(narrowed.contains(LlmCapability::Discovery));
        assert!(narrowed.contains(LlmCapability::Blocked));
        assert!(!narrowed.contains(LlmCapability::PlanCreation));
        assert!(!narrowed.contains(LlmCapability::PlanUpdate));
        assert!(!narrowed.contains(LlmCapability::ToolchainVerify));
        // A step owns the implementation only, so the documentation channel of
        // the interactive dialogue is absent from its surface.
        assert!(!narrowed.contains(LlmCapability::DocRequest));
    }

    #[test]
    fn test_narrow_capabilities_isolated_step_retains_the_abort_surface() {
        // An isolated step runs unattended, so narrowing the advertised
        // surface must keep the escape hatch that lets the model demand a host
        // action instead of improvising a surrogate change or misreporting a
        // `blocked` outcome.
        let advertised = CapabilitySet::from_slice(LlmCapability::ALL);
        let step = TurnIntent::IsolatedStep.narrow_capabilities(&advertised);
        assert!(step.contains(LlmCapability::Abort));

        // A plan discussion carries the whole dialogue surface, so it keeps the
        // escape hatch as well.
        let discussion = TurnIntent::PlanDiscussion.narrow_capabilities(&advertised);
        assert!(discussion.contains(LlmCapability::Abort));
    }

    #[test]
    fn test_narrow_capabilities_isolated_step_carries_no_plan_permission() {
        // Whichever base the turn was narrowed from, an isolated step never
        // authorizes a plan change: the plan is immutable while the step runs.
        for base in [
            CapabilitySet::from_slice(LlmCapability::ALL),
            CapabilitySet::interactive_repl(),
            CapabilitySet::discussion(),
        ] {
            let narrowed = TurnIntent::IsolatedStep.narrow_capabilities(&base);
            assert!(!narrowed.contains(LlmCapability::PlanCreation));
            assert!(!narrowed.contains(LlmCapability::PlanUpdate));
        }

        // Every base that carries the execution surface keeps it after the
        // narrowing; a discussion profile implements nothing, so it contributes
        // no file-edit capability in the first place.
        for base in [
            CapabilitySet::from_slice(LlmCapability::ALL),
            CapabilitySet::interactive_repl(),
        ] {
            let narrowed = TurnIntent::IsolatedStep.narrow_capabilities(&base);
            assert!(narrowed.contains(LlmCapability::FileEdits));
            assert!(narrowed.contains(LlmCapability::Discovery));
        }
    }

    #[test]
    fn test_narrow_capabilities_discussion_owns_both_plan_permissions() {
        let advertised = CapabilitySet::from_slice(LlmCapability::ALL);
        let narrowed = TurnIntent::PlanDiscussion.narrow_capabilities(&advertised);
        assert!(narrowed.contains(LlmCapability::PlanCreation));
        assert!(narrowed.contains(LlmCapability::PlanUpdate));
    }

    #[test]
    fn test_narrow_capabilities_isolated_step_matches_the_step_profile() {
        // The narrowing is an intersection with the regime profile followed by
        // the single harness-owned removal. A base carrying the whole advertised
        // surface therefore reaches exactly the step profile minus toolchain
        // verification, while a narrower base reaches a subset of it.
        let expected = CapabilitySet::isolated_step().without(LlmCapability::ToolchainVerify);
        let advertised = CapabilitySet::from_slice(LlmCapability::ALL);
        let full = TurnIntent::IsolatedStep.narrow_capabilities(&advertised);
        assert_eq!(full.as_slice(), expected.as_slice());

        // A narrower base reaches a subset of the step profile.
        let repl = TurnIntent::IsolatedStep.narrow_capabilities(&CapabilitySet::interactive_repl());
        for capability in repl.iter() {
            assert!(expected.contains(*capability));
        }
        assert!(repl.contains(LlmCapability::FileEdits));
    }
    #[test]
    fn test_regime_profile_maps_every_intent() {
        assert_eq!(
            TurnIntent::InteractiveRepl.regime_profile(),
            CapabilityProfile::interactive_repl()
        );
        assert_eq!(
            TurnIntent::PlanDiscussion.regime_profile(),
            CapabilityProfile::discussion()
        );
        assert_eq!(
            TurnIntent::IsolatedStep.regime_profile(),
            CapabilityProfile::isolated_step()
        );
    }

    #[test]
    fn test_dialogue_contours_share_one_core_list() {
        // A review discussion runs through the interactive intent, so the shared
        // head is the interactive core list by construction, while the delta of
        // that contour stays empty.
        let interactive = TurnIntent::InteractiveRepl.regime_profile();
        assert!(interactive.delta().is_empty());
        assert_eq!(
            interactive.surface().as_slice(),
            interactive.core().as_slice()
        );

        // A plan discussion reuses the very same head and carries only its own
        // plan mutation in the delta.
        let discussion = TurnIntent::PlanDiscussion.regime_profile();
        assert_eq!(discussion.core().as_slice(), interactive.core().as_slice());
        assert_eq!(discussion.delta().as_slice(), &[LlmCapability::PlanUpdate]);
        assert!(!discussion.core().contains(LlmCapability::PlanUpdate));

        // The delta list is the sanctioned extension point: a review-specific
        // capability would join the review delta without touching the head, and
        // none is built today.
        assert_eq!(
            interactive.surface().as_slice(),
            CapabilitySet::from_slice(LlmCapability::ALL)
                .without(LlmCapability::PlanUpdate)
                .as_slice()
        );
    }
}
