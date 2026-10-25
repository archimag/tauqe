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

use serde::{Deserialize, Serialize};

use crate::command::{CapabilitySet, LlmCapability};

/// Mode of a bounded turn, declared by the caller that starts it.
///
/// The variants are mutually exclusive execution regimes:
///
/// - [`TurnIntent::InteractiveRepl`] is a free interactive dialogue started
///   from the REPL, including a dialogue anchored on a review session.
/// - [`TurnIntent::ReadonlyDiscussion`] is a read-only architectural
///   discussion of a plan.
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
    /// Read-only architectural discussion of a plan.
    ReadonlyDiscussion,
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

    /// Reports whether this is a read-only plan discussion.
    pub fn is_readonly_discussion(self) -> bool {
        matches!(self, Self::ReadonlyDiscussion)
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
    /// - [`TurnIntent::ReadonlyDiscussion`] keeps only the discussion profile.
    /// - [`TurnIntent::IsolatedStep`] keeps the isolated-step profile and drops
    ///   toolchain verification, because the harness owns verification for a
    ///   step deterministically.
    ///
    /// The narrowing always intersects with a regime profile, never with a
    /// wire-format profile, so both transports converge on the same capability
    /// surface for a given intent.
    pub fn narrow_capabilities(self, capabilities: &CapabilitySet) -> CapabilitySet {
        match self {
            Self::InteractiveRepl => capabilities.clone(),
            Self::ReadonlyDiscussion => Self::intersect(capabilities, &CapabilitySet::discussion()),
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
        TurnIntent::ReadonlyDiscussion,
        TurnIntent::IsolatedStep,
    ];

    #[test]
    fn test_predicates_are_mutually_exclusive() {
        for intent in ALL {
            let flags = [
                intent.is_interactive_repl(),
                intent.is_readonly_discussion(),
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
        assert!(TurnIntent::ReadonlyDiscussion.is_readonly_discussion());
        assert!(TurnIntent::IsolatedStep.is_isolated_step());
    }

    #[test]
    fn test_serde_snake_case_roundtrip() {
        let cases = [
            (TurnIntent::InteractiveRepl, "\"interactive_repl\""),
            (TurnIntent::ReadonlyDiscussion, "\"readonly_discussion\""),
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
    fn test_narrow_capabilities_makes_discussion_read_only() {
        let advertised = CapabilitySet::from_slice(LlmCapability::ALL);
        let narrowed = TurnIntent::ReadonlyDiscussion.narrow_capabilities(&advertised);

        assert!(narrowed.contains(LlmCapability::TextMessage));
        assert!(narrowed.contains(LlmCapability::Discovery));
        assert!(narrowed.contains(LlmCapability::PlanCreation));
        assert!(narrowed.contains(LlmCapability::PlanUpdate));
        assert!(narrowed.contains(LlmCapability::UserLanguage));
        assert!(!narrowed.contains(LlmCapability::FileEdits));
        assert!(!narrowed.contains(LlmCapability::ToolchainVerify));
        assert!(!narrowed.contains(LlmCapability::Scratchpad));
        assert!(!narrowed.contains(LlmCapability::Outcome));
        assert!(!narrowed.contains(LlmCapability::Blocked));
    }

    #[test]
    fn test_narrow_capabilities_isolated_step_drops_plan_and_verification() {
        let advertised = CapabilitySet::from_slice(LlmCapability::ALL);
        let narrowed = TurnIntent::IsolatedStep.narrow_capabilities(&advertised);

        assert!(narrowed.contains(LlmCapability::FileEdits));
        assert!(narrowed.contains(LlmCapability::Discovery));
        assert!(narrowed.contains(LlmCapability::Blocked));
        assert!(!narrowed.contains(LlmCapability::PlanCreation));
        assert!(!narrowed.contains(LlmCapability::PlanUpdate));
        assert!(!narrowed.contains(LlmCapability::ToolchainVerify));
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

        // A read-only discussion implements nothing, so the narrowing removes
        // the abort surface it never carried.
        let discussion = TurnIntent::ReadonlyDiscussion.narrow_capabilities(&advertised);
        assert!(!discussion.contains(LlmCapability::Abort));
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
        let narrowed = TurnIntent::ReadonlyDiscussion.narrow_capabilities(&advertised);
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
}
