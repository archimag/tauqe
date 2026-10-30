use serde::{Deserialize, Serialize};

use crate::model::{CancelMode, ModelSelection};

use super::item::{IssueStatus, ReviewIssue};
use super::session::ReviewSession;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewStartParams {
    /// Reviewer selection: a semantic role, an explicit model reference, or an
    /// explicit role-list rung carrying the reasoning depth of the configured
    /// entry the developer picked.
    ///
    /// The whole selection travels on the wire, so the server never re-derives
    /// the reasoning depth by matching the model against the configured reviewer
    /// entries. A legacy client that sends only a model reference keeps
    /// decoding: the untagged `ModelSelection` form maps it to
    /// `ModelSelection::Specific`, and an absent selection runs the base rung of
    /// the reviewer role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,
    /// Optional list of directory or glob patterns selecting the files to
    /// review.
    ///
    /// Coverage is the union of every entry: a repository file is audited when
    /// at least one pattern selects it, so several lines widen the audit
    /// instead of competing for it. Blank entries narrow nothing and are
    /// dropped before matching. Matched files are merged with the current User
    /// Context without mutating it: the developer keeps exclusive control of
    /// the user layer, so the selected paths are added alongside it rather
    /// than replacing it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_patterns: Vec<String>,
    /// Name of the configured review preset to launch.
    ///
    /// The server resolves the preset into the explicit groups the run audits
    /// and records the result as the session launch snapshot, so a later edit
    /// of the project configuration never rewrites a stored session. Absent for
    /// a legacy single-audit run and for a one-off custom launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
}

/// Parameters for `review/preview`.
///
/// The projection describes the audit window `review/start` would launch from
/// the same selection, so a client resolves consent against exactly the shape
/// the run would take instead of learning about a truncated window after the
/// fact. Only the fields that decide the composition of the window travel
/// here: the active context, the repository listing, and the configured
/// budgets are server-side facts, while the model and the user prompt shape
/// the audit itself and never its size.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPreviewParams {
    /// Optional list of directory or glob patterns selecting additional audit
    /// targets, interpreted exactly as `ReviewStartParams::path_patterns`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_patterns: Vec<String>,
    /// Name of the configured review preset whose groups would be audited.
    ///
    /// A preset launch never reads the interactive context window, so its
    /// projection describes the declared groups instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
}

/// Composition of one audit window before a review runs.
///
/// Every counter is a deterministic pre-computation: the projection applies
/// the same selection order and the same limits as the launch it describes, so
/// a window that previews clean is the window the run audits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPreviewWindow {
    /// Number of files the active context contributes to the window.
    #[serde(default)]
    pub context_files: usize,
    /// Estimated tokens the active context contributes.
    #[serde(default)]
    pub context_tokens: usize,
    /// Number of repository files the path patterns matched.
    ///
    /// A file the context already carries is counted here and is not selected
    /// a second time, so this number reports how much the patterns found
    /// rather than how much they added.
    #[serde(default)]
    pub matched_files: usize,
    /// Number of matched files the window accepted.
    #[serde(default)]
    pub selected_files: usize,
    /// Estimated tokens the selected files add.
    #[serde(default)]
    pub selected_tokens: usize,
    /// Number of matched files the file token budget dropped.
    ///
    /// The file token budget is the only knife the window applies, so this
    /// aggregate and [`Self::omitted_by_budget`] report the same files: no
    /// other limit exists to drop a candidate, and a count of files is never a
    /// limit of its own.
    #[serde(default)]
    pub omitted_files: usize,
    /// Matched files dropped because the file token budget was exhausted.
    #[serde(default)]
    pub omitted_by_budget: usize,
    /// File token budget the pattern expansion shares with discovery.
    #[serde(default)]
    pub files_token_budget: usize,
}

impl ReviewPreviewWindow {
    /// Whether the file token budget drops files the selection matched.
    ///
    /// This is the single predicate a client branches on before launching the
    /// audit: a window that drops nothing needs no consent step, because the
    /// run audits exactly what the developer selected. The verdict is read from
    /// the budget breakdown, never from a generic count of dropped files, so
    /// truncation keeps exactly one cause.
    pub fn is_truncated(&self) -> bool {
        self.omitted_by_budget > 0
    }
}

/// One group of a grouped audit projected by `review/preview`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPreviewGroup {
    /// Title of the configured group the window belongs to.
    ///
    /// A launch mints fresh group identifiers, so a projection names the
    /// configured title instead of an identity that does not exist yet.
    #[serde(default)]
    pub title: String,
    /// The window the group would audit.
    #[serde(default)]
    pub window: ReviewPreviewWindow,
}

/// Outcome of projecting a review audit window.
///
/// The aggregate window is the single audit itself or, for a preset launch,
/// the sum of the grouped windows, so a client renders one coherent picture of
/// the coverage instead of summing per-group numbers on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPreviewResult {
    /// Aggregate window of the launch the request describes.
    #[serde(default)]
    pub window: ReviewPreviewWindow,
    /// Per-group windows of a preset launch, in execution order.
    ///
    /// Empty for a plain single audit, which audits the context window without
    /// declaring explicit groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ReviewPreviewGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewListParams {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewListResult {
    pub reviews: Vec<ReviewSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewGetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewGetResult {
    pub session: Option<ReviewSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDeleteParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDeleteResult {
    pub success: bool,
}

/// Parameters for `review/cancel`.
///
/// The `mode` field is optional so clients that predate two-mode cancellation
/// keep sending an empty object and receive the default immediate abort.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCancelParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CancelMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewExecuteParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    /// Identifier of the review group whose unified subplan is executed.
    ///
    /// A group run addresses the group itself, never one of its findings:
    /// `step_id` then names a step of the group subplan. The field is a target
    /// of its own, so a request names either a group or findings, and the
    /// server rejects a request that mixes `group_id` with `item_id` or
    /// `item_ids`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Client-ordered finding ids limiting a session queue to the findings
    /// one rendered section shows.
    ///
    /// `None` runs every actionable finding of the session; the ids are
    /// resolved verbatim in the given order, and unknown or non-actionable
    /// ids are dropped instead of guessed at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_ids: Option<Vec<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewUpdateItemParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    pub item_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<IssueStatus>,
    /// Resolution note recorded on the finding.
    ///
    /// Serialized as `note`; the legacy `resolution_notes` key is still
    /// accepted on read so older clients keep updating findings unchanged.
    #[serde(
        default,
        alias = "resolution_notes",
        skip_serializing_if = "Option::is_none"
    )]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewUpdateItemResult {
    pub item: ReviewIssue,
}

/// Parameters for `review/analyze`.
///
/// Post-analysis classifies the findings of one stored session into independent
/// issues and architectural clusters. The session identifier is mandatory and
/// never defaulted: a run that addressed the active session implicitly would
/// rewrite the classification of whichever session happened to be focused, so
/// the caller always names its target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAnalyzeParams {
    /// Identifier of the stored session whose findings are classified.
    pub review_id: String,
}

/// Outcome of a completed post-analysis run.
///
/// Carries the whole session so the client renders the new layer without a
/// follow-up fetch. Only the classification layer is replaced; the findings it
/// references by identifier are immutable and are never rescanned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAnalyzeResult {
    /// The session carrying the freshly replaced post-analysis layer.
    pub session: ReviewSession,
}
