//! Pure Architect synthesis response interpretation and transitional outcome types.

use crate::control::outcome::AbortReport;
use crate::llm::CapabilityTree;
use crate::llm::LlmCapability;
use crate::models::ArchitectTier;
use crate::plan::prompt::{parse_plan_refinement_output, PlanRefinementOutput};
use crate::prompt::refine::unsupported_plan_update_reason;
/// Composes the capability surface of an Architect horizon synthesis.
///
/// A synthesis projects a horizon instead of patching a tree, so file edits and
/// toolchain verification stay excluded. The abort escape hatch stays included,
/// because a synthesis runs unattended exactly as an isolated step does: a
/// host-environment impossibility only the developer can repair is reported
/// through `abort` instead of an improvised surrogate or a misreported blocker.
pub fn architect_synthesis_capability_expr() -> CapabilityTree {
    CapabilityTree::from_leaves([
        LlmCapability::TextMessage,
        LlmCapability::ContextRequest,
        LlmCapability::RepoSearch,
        LlmCapability::RepoDirs,
        LlmCapability::RepoMap,
        LlmCapability::DirList,
        LlmCapability::DocRequest,
        LlmCapability::OuterDocRequest,
        LlmCapability::PlanCreation,
        LlmCapability::Scratchpad,
        LlmCapability::CommitMessage,
        LlmCapability::TurnError,
        LlmCapability::TurnSkip,
        LlmCapability::Blocked,
        LlmCapability::Abort,
        LlmCapability::PlanUpdate,
    ])
}

/// Typed terminal status of one structured synthesis run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructuredRefineStatus {
    /// The Architect finalized the horizon.
    Synthesized,
    /// The Architect demanded more context than `files_token_budget` allows.
    BudgetExceeded {
        /// Deterministic report the caller surfaces.
        diagnostic: String,
    },
    /// The synthesis could not finalize within the approved design.
    Blocked {
        /// Deterministic reason the caller reports.
        reason: String,
    },
    /// The synthesis needs an action only the developer can perform on the host.
    Aborted(AbortReport),
    /// The turn was cancelled before the Architect finalized.
    Cancelled,
}

/// Final outcome of one structured synthesis run.
#[derive(Debug, Clone)]
pub struct StructuredRefineOutcome {
    /// Parsed output of the final round, when one was produced.
    pub output: Option<PlanRefinementOutput>,
    /// Tier the synthesis concluded on.
    pub tier: ArchitectTier,
    /// Typed terminal status of the run.
    pub status: StructuredRefineStatus,
}

/// Accepted terminal status of one synthesis round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundStatus {
    /// The Architect finalized the horizon.
    Success,
    /// The Architect refused to plan and stated its reason.
    Blocked,
    /// The Architect aborted: only the developer can unblock the host.
    Aborted,
    /// The round emitted a plan mutation this contour does not accept.
    PlanUpdateRefused(String),
    /// The Architect named the evidence it still needs.
    NeedsContext,
}
/// Pure decoding of a synthesis answer; the scenario interprets its status.
pub fn parse_round(raw_text: &str) -> Result<(PlanRefinementOutput, RoundStatus), String> {
    let output = parse_plan_refinement_output(raw_text).map_err(|err| err.to_string())?;
    if declares_abort(&output) {
        return Ok((output, RoundStatus::Aborted));
    }
    if let Some(reason) = unsupported_plan_update_reason(raw_text) {
        return Ok((output, RoundStatus::PlanUpdateRefused(reason)));
    }
    let status = match output.status.trim().to_ascii_lowercase().as_str() {
        "success" => RoundStatus::Success,
        "blocked" => RoundStatus::Blocked,
        "needs_context" => RoundStatus::NeedsContext,
        other => return Err(format!("Unsupported plan refinement status '{other}'")),
    };
    Ok((output, status))
}

fn declares_abort(output: &PlanRefinementOutput) -> bool {
    output.abort.as_ref().is_some_and(|abort| abort.is_aborted)
}

pub fn abort_report(output: &PlanRefinementOutput) -> AbortReport {
    let abort = output.abort.as_ref();
    let reason = abort
        .map(|abort| abort.reason.trim())
        .filter(|reason| !reason.is_empty())
        .unwrap_or("Execution aborted");
    let recommendation = abort
        .map(|abort| abort.recommendation.trim())
        .filter(|recommendation| !recommendation.is_empty())
        .unwrap_or("No recommendation was stated.");
    AbortReport {
        reason: reason.to_string(),
        recommendation: recommendation.to_string(),
    }
}

pub fn blocked_reason(output: &PlanRefinementOutput) -> String {
    output
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let explanation = output.explanation.trim();
            (!explanation.is_empty()).then(|| explanation.to_string())
        })
        .unwrap_or_else(|| "The Architect reported a blocker without stating a reason.".to_string())
}
