//! Review groups and the post-analysis layer.
//!
//! A review run is decomposed into isolated [`ReviewGroup`]s, each auditing its
//! own slice of the repository. Findings are immutable: each one carries a
//! stable UUIDv7 identifier and the identifier of the group that produced it,
//! while the post-analysis layer classifies the findings by reference instead
//! of rewriting them. The group itself is the operational unit of remediation:
//! it owns the unified subplan synthesized for its root cause, so the symptoms
//! of one cause are fixed by one coherent change set.

use serde::{Deserialize, Serialize};

use crate::model::ArchitectTier;
use crate::plan::PlanItem;

/// Stable identifier of the implicit group a single-audit session audits.
pub const DEFAULT_REVIEW_GROUP_ID: &str = "default";

/// Title of the implicit single-audit group.
const DEFAULT_REVIEW_GROUP_TITLE: &str = "Default";

/// Stable identifier of the group collecting the findings that share no root
/// cause with another finding.
///
/// The post-analysis layer always materializes this group, even when it turns
/// out empty, so a classified session keeps one uniform shape: every finding
/// belongs to exactly one group, and the findings that need no architectural
/// remediation are collected here instead of floating outside the group list.
pub const LOCAL_REVIEW_GROUP_ID: &str = "local";

/// Title of the group collecting the findings that share no root cause.
pub const LOCAL_REVIEW_GROUP_TITLE: &str = "Local Issues";

/// Class of a review group, the typed dichotomy that governs its execution.
///
/// A simple group collects independent defects, so each of its findings is
/// addressed by its own isolated execution. An architectural group collects
/// manifestations of one shared root cause: its findings are symptoms, so the
/// group is executed exclusively through its unified `subplan`, and a finding
/// of such a group is never executed on its own.
///
/// The class is a stored field rather than a property derived from the shape
/// of a group `id`: a prefix is a guess, not a contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewGroupKind {
    /// Group of independent findings, executed finding by finding.
    #[default]
    Simple,
    /// Group of findings that share one root cause, executed as one plan.
    Architectural,
}

/// Whether and when the post-analysis layer is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostAnalysisMode {
    /// Classify the findings once every group has finished.
    Automatic,
    /// Classify only when the developer asks for it after the groups finish.
    Manual,
    /// Never classify: the findings stay a flat list.
    #[default]
    None,
}

/// One isolated slice of a review run.
///
/// A group owns the repository paths it audits and an optional local prompt.
/// Groups execute independently, so a failure or a cancellation inside one
/// group leaves the findings the other groups already produced intact.
///
/// The same structure also represents a group the post-analysis derived from a
/// shared architectural root cause: it then names that cause in `description`,
/// collects the findings that manifest it, and owns the single subplan that
/// resolves them all.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewGroup {
    /// Identifier of the group, unique within a session.
    #[serde(default)]
    pub id: String,
    /// Human-readable title rendered by clients.
    #[serde(default)]
    pub title: String,
    /// Repository-relative paths this group audits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Instructions that apply to this group only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Explanation of the root cause the findings of this group share.
    ///
    /// A group that only names a slice to audit keeps it empty and leaves its
    /// findings independent. A group the post-analysis derived from a shared
    /// root cause states that cause here, so the developer reads why exactly
    /// these findings were placed together instead of re-deriving it from the
    /// member list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Class of this group, which decides how the group is executed.
    ///
    /// The class is stored, never derived: a simple group is executed finding
    /// by finding, while an architectural group is executed exclusively
    /// through its unified `subplan`. A record written before the field
    /// existed carries no class and decodes as [`ReviewGroupKind::Simple`].
    #[serde(default)]
    pub kind: ReviewGroupKind,
    /// Unified remediation plan for the whole group.
    ///
    /// A root cause is fixed by one coherent change set, so the group owns a
    /// single subplan instead of one isolated plan per manifestation. The
    /// findings it collects stay untouched: the plan belongs to the group, and
    /// the findings merely describe what the group manifests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subplan: Vec<PlanItem>,
    /// The Architect configuration that produced `subplan`.
    ///
    /// Planning is the *What* role's output, so the group records which
    /// Architect model, at which reasoning depth, decomposed it. A later
    /// replanning wave reads this back instead of re-deriving a configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subplan_architect: Option<ArchitectTier>,
    /// Identifier of the sub-step whose horizon was most recently reprojected.
    ///
    /// Mirrors `Plan::last_replanned_step`, so a repeated failure of the same
    /// sub-step is recognized from persisted state and escalates the Architect
    /// exactly once per failed step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subplan_last_replanned_step: Option<String>,
}

impl ReviewGroup {
    /// The implicit group a single-audit session audits.
    ///
    /// A session created before grouping existed, or one launched as a single
    /// audit, carries no explicit groups. It is the degenerate case of one
    /// `Default` group covering every session target file, so consumers resolve
    /// it through `ReviewSession::effective_groups` instead of branching on
    /// "legacy or grouped".
    pub fn single_default(paths: Vec<String>) -> Self {
        Self {
            id: DEFAULT_REVIEW_GROUP_ID.to_string(),
            title: DEFAULT_REVIEW_GROUP_TITLE.to_string(),
            paths,
            prompt: None,
            description: None,
            kind: ReviewGroupKind::Simple,
            subplan: Vec::new(),
            subplan_architect: None,
            subplan_last_replanned_step: None,
        }
    }
}

/// A cluster of findings that share one architectural root cause.
///
/// The cluster is the operational unit of remediation: one root cause is fixed
/// by one coherent change set, so the group owns a unified subplan instead of
/// leaving every symptom to be fixed in isolation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ArchitecturalIssueGroup {
    /// Identifier of the cluster, unique within one post-analysis layer.
    #[serde(default)]
    pub id: String,
    /// Title of the shared root cause.
    #[serde(default)]
    pub title: String,
    /// Explanation of why these findings belong together.
    #[serde(default)]
    pub description: String,
    /// Identifiers of the findings in this cluster.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issue_ids: Vec<String>,
    /// Unified remediation plan for every finding of the cluster.
    ///
    /// A root cause is fixed by one coherent change set, so the group owns a
    /// single subplan instead of one isolated plan per symptom. The findings it
    /// references stay untouched: the plan belongs to the cluster, and the
    /// findings merely describe what the cluster manifests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subplan: Vec<PlanItem>,
    /// The Architect configuration that produced `subplan`.
    ///
    /// Planning is the *What* role's output, so the group records which
    /// Architect model, at which reasoning depth, decomposed it. A later
    /// replanning wave reads this back instead of re-deriving a configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subplan_architect: Option<ArchitectTier>,
    /// Identifier of the sub-step whose horizon was most recently reprojected.
    ///
    /// Mirrors `Plan::last_replanned_step`, so a repeated failure of the same
    /// sub-step is recognized from persisted state and escalates the Architect
    /// exactly once per failed step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subplan_last_replanned_step: Option<String>,
}

/// The post-analysis layer of a session.
///
/// The Architect role classifies the findings of every group at once, splitting
/// them into findings that stand on their own and clusters that share one root
/// cause. The layer references findings by identifier and never rewrites them,
/// so replacing it discards the previous classification — including the
/// remediation horizons its groups carry — and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewPostAnalysis {
    /// Unix epoch seconds of the analysis that produced this layer.
    pub created_at: u64,
    /// Global verdict over the whole session.
    #[serde(default)]
    pub summary: String,
    /// Identifiers of the findings that stand on their own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub independent_issues: Vec<String>,
    /// Clusters of findings that share an architectural root cause.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub architectural_groups: Vec<ArchitecturalIssueGroup>,
}

/// Immutable snapshot of the configuration that launched a session.
///
/// The snapshot isolates a stored session from later edits of the project
/// configuration: a session always describes the groups and the post-analysis
/// mode it was actually launched with.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewLaunchConfig {
    /// Name of the configured preset the session was launched from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Post-analysis mode the session was launched with.
    #[serde(default)]
    pub mode: PostAnalysisMode,
    /// Groups the session audited, copied verbatim from that configuration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ReviewGroup>,
}
