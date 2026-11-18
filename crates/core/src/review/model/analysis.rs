//! Post-analysis payload of a code review session.
//!
//! Post-analysis is the Architect role's answer over the immutable findings of a
//! whole review session: it reads what every isolated review group reported and
//! names the systemic root causes no single group could observe. It rewrites no
//! finding and produces no code, so the payload travels through the Structured
//! Output contour (`crates/core/Conventions.md`, section 1.2) rather than through
//! a code-modification channel.
//!
//! The payload is the wire contract of that model output, so it lives beside the
//! review domain model it describes. Findings are referenced by the UUIDv7
//! identity they were minted with; the strict JSON Schema, the tolerant decoding,
//! and the validation against the identities a session actually holds belong to
//! the engine, because they are part of the model-facing contour.

use serde::Deserialize;

use super::item::IssueStatus;

/// Initial routing of one finding decided by the post-analysis.
///
/// The post-analysis holds the whole session in view, so it is also the moment to
/// route every finding: a defect with no open fork can be executed immediately,
/// while a finding whose resolution depends on a decision only the engineer can
/// make must be discussed first. Recording the routing here keeps the session
/// actionable straight after the analysis instead of leaving the engineer to
/// triage every finding by hand.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct PostAnalysisTriageOutput {
    /// Identity of the finding this entry routes.
    #[serde(default)]
    pub uuid: String,
    /// Status the analysis routed this finding to.
    ///
    /// Only two statuses are meaningful and the enforced schema admits no other:
    /// [`IssueStatus::Todo`] for a defect that must be executed without any open
    /// decision, and [`IssueStatus::Discussion`] for a finding that needs one. An
    /// absent field decodes as [`IssueStatus::Discussion`], the safe choice,
    /// because a finding the analysis failed to route stays with the engineer
    /// instead of silently entering execution.
    #[serde(default)]
    pub status: IssueStatus,
    /// The open question the engineer must answer, stated for a finding routed to
    /// `discussion` and absent for a finding routed to `todo`.
    #[serde(default)]
    pub discussion_topic: Option<String>,
}

/// One model-emitted cluster of findings that share a root cause.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct PostAnalysisGroupOutput {
    /// Title of the shared root cause.
    #[serde(default)]
    pub title: String,
    /// Explanation of why exactly these findings belong together.
    #[serde(default)]
    pub description: String,
    /// Identities of the findings that manifest this root cause.
    #[serde(default)]
    pub issue_ids: Vec<String>,
}

/// Model-emitted post-analysis payload.
///
/// `summary` carries the Architect's global verdict, `triage` routes every
/// finding, `independent_issues` carries the findings that stand on their own,
/// and `architectural_groups` carries the clusters that share one root cause.
/// Every collection defaults to empty, so a partial payload still decodes and the
/// caller decides whether the classification is complete enough to persist.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct PostAnalysisOutput {
    /// Global verdict over the whole session.
    #[serde(default)]
    pub summary: String,
    /// Initial routing of every finding of the session.
    #[serde(default)]
    pub triage: Vec<PostAnalysisTriageOutput>,
    /// Identities of the findings that stand on their own.
    #[serde(default)]
    pub independent_issues: Vec<String>,
    /// Clusters of findings that share one architectural root cause.
    #[serde(default)]
    pub architectural_groups: Vec<PostAnalysisGroupOutput>,
}
