//! Pure projection machinery of the Review view.
//!
//! This module owns the read-only projection of stored review sessions into
//! renderable tree rows: the `VisibleReviewRow` row model, the expansion keys,
//! and the group bucketing that collects findings under the group owning them.
//! Everything here derives rows from session state; folding, selection, and
//! viewport behavior stay on `ReviewViewState`, and row rendering lives in
//! `views::review::tree` (Conventions §3.5).
//!
//! A session is a single layer: the post-analysis regroups it in place by
//! rewriting the verdict, replacing the groups, and repointing every finding,
//! so the tree always renders the current groups and never branches on a
//! perspective (concept #c-1, #c-3).

use std::collections::HashSet;

use tauqe_protocol::review::DEFAULT_REVIEW_GROUP_ID;
use tauqe_protocol::{
    ArchitectTier, DiscussionFocus, DiscussionTarget, IssueStatus, IssueType, ModelRef,
    ModelSelection, PlanItem, PlanItemStatus, ReviewGroupKind, ReviewIssue, ReviewItem,
    ReviewSession,
};

use crate::i18n::active_t;
use crate::views::implementation::leaf_progress;

use super::ReviewViewState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleReviewRow {
    SessionHeader {
        session_id: String,
        title: String,
        description: Option<String>,
        created_at: u64,
        model: ModelRef,
        total: usize,
        fixed: usize,
        todo: usize,
        in_progress: usize,
        discussion: usize,
        skipped: usize,
        rejected: usize,
        is_expanded: bool,
        /// Whether the active discussion focus anchors on this session.
        focused: bool,
    },
    SessionSummary {
        session_id: String,
        description: String,
        is_expanded: bool,
    },
    /// Header of one review group.
    ///
    /// A group is either a slice the run audited or a cluster the post-analysis
    /// derived from a shared root cause (concept #c-2, #c-4). The unified
    /// remediation plan of that cause owns a branch of its own, so the header
    /// announces it through `has_plan` instead of carrying it.
    GroupHeader {
        session_id: String,
        group_id: String,
        /// Class of the group, which decides how the group is executed.
        ///
        /// A simple group is executed finding by finding, while an
        /// architectural group is executed exclusively through the unified plan
        /// its `GroupPlanHeader` branch renders (concept #c-1).
        kind: ReviewGroupKind,
        title: String,
        /// Explanation of the root cause the findings of this group share.
        description: Option<String>,
        /// Reason the unified remediation plan of this group is blocked.
        ///
        /// An architectural group whose decomposition could not be synthesized,
        /// or whose remediation stalled on an unresolved decision, states the
        /// diagnosis here: the tree badges the header and opens the group body
        /// with the architectural fork instead of scattering the diagnosis
        /// across the notes of the findings (concept #c-2).
        blocked_reason: Option<String>,
        /// Architectural decision already recorded for this group.
        ///
        /// The decision outlives the blocker it resolved, so a later reader
        /// still sees why the plan of the group took the shape it took.
        resolution_notes: Option<String>,
        findings_count: usize,
        /// Whether the group owns a unified plan branch below its header.
        ///
        /// A group whose plan the Architect projects just-in-time carries no
        /// branch until the synthesis lands, so the fold icon of the header
        /// still reflects every body it can reveal.
        has_plan: bool,
        is_expanded: bool,
    },
    /// Header of the unified remediation plan a group owns.
    ///
    /// The plan is the execution mechanism of a root cause (`How`) while the
    /// findings are the symptoms that manifest it (`What`), so the plan owns an
    /// independent branch: its steps render strictly deeper than this header,
    /// and the findings close the group on their own band (concept #c-1,
    /// Conventions §4.8).
    GroupPlanHeader {
        session_id: String,
        /// Identifier of the group whose plan the branch renders.
        group_id: String,
        /// Display title of the branch.
        ///
        /// The localized label travels with the row, so the renderer resolves no
        /// literal of its own (Conventions §9).
        title: String,
        /// Leaf steps already done.
        done: usize,
        /// Executable leaf steps of the plan.
        total: usize,
        /// The Architect configuration that produced the plan.
        subplan_architect: Option<ArchitectTier>,
        /// Whether the branch is unfolded.
        is_expanded: bool,
    },
    ReviewItem {
        session_id: String,
        item_id: u32,
        title: String,
        /// Classified problem type driving the `[BUG]`/`[SEC]`/... badge.
        issue_type: IssueType,
        status: IssueStatus,
        /// Git commit that carried the fix of this finding, once it is fixed.
        commit: Option<String>,
        model: Option<ModelSelection>,
        files: Vec<String>,
        /// Immutable contracts, interfaces, and invariants the fix must rely on
        /// but must never modify.
        readonly_files: Vec<String>,
        /// Technical description of the defect, rendered as a Markdown block.
        description: String,
        /// Concrete, ordered remediation strategy, rendered as a Markdown list.
        recommendations: Vec<String>,
        note: Option<String>,
        is_expanded: bool,
        /// Whether the active discussion focus anchors on this finding.
        focused: bool,
    },
    /// One interactive row of a group's unified remediation plan.
    ///
    /// A root cause is fixed by one coherent change set, so the steps belong to
    /// the group and render once under its header instead of being repeated
    /// under every symptom. Subplan steps are full tree nodes, so the cursor can
    /// rest on a single step, fold its details and files, and execute exactly
    /// that step (Conventions §4.4, concept #c-5).
    SubplanItem {
        session_id: String,
        /// Identifier of the group whose unified plan owns the step.
        group_id: String,
        step_id: String,
        title: String,
        details: Option<String>,
        status: PlanItemStatus,
        /// Git commit the step recorded when it finished, if any.
        commit: Option<String>,
        /// Diagnostic report of the semantic blocker that stopped the step.
        ///
        /// Present only while the step is blocked, so the reason survives the
        /// interrupted turn on disk and in the tree instead of being lost with
        /// the transient session.
        blocked_reason: Option<String>,
        files: Vec<String>,
        readonly_files: Vec<String>,
        /// Identifiers of the project-declared `[[outer_docs]]` entries the step
        /// is implemented against (concept #c-review-subplan-docs-visibility).
        outer_docs: Vec<String>,
        depends_on: Vec<String>,
        depth: usize,
        /// Whether the step owns nested child steps.
        ///
        /// A container step delegates its target files to its leaves, so it
        /// never repeats their file lists and inflates the rendered height.
        has_children: bool,
        is_expandable: bool,
        is_expanded: bool,
    },
}

impl VisibleReviewRow {
    pub fn session_id(&self) -> &str {
        match self {
            Self::SessionHeader { session_id, .. } => session_id,
            Self::SessionSummary { session_id, .. } => session_id,
            Self::GroupHeader { session_id, .. } => session_id,
            Self::GroupPlanHeader { session_id, .. } => session_id,
            Self::ReviewItem { session_id, .. } => session_id,
            Self::SubplanItem { session_id, .. } => session_id,
        }
    }
}

/// Section key of one group header.
///
/// The `:g:` segment keeps a section key disjoint from the `{session}:{item}`
/// key of an expanded finding and the `{session}:{item}:s:{step}` key of an
/// expanded subplan step, so the expansion scopes never collide and clearing
/// one scope never drops another.
pub(super) fn group_key(session_id: &str, group_id: &str) -> String {
    format!("{}:g:{}", session_id, group_id)
}

/// Expansion key of the unified plan branch of one group.
///
/// The `:p:` segment keeps a branch key disjoint from the `{session}:g:{group}`
/// key of a group header, the `{session}:{item}` key of an expanded finding,
/// and the `{session}:g:{group}:s:{step}` key of an expanded plan step, so
/// folding the branch never touches the steps or the findings, and clearing one
/// scope never drops another.
pub(super) fn plan_branch_key(session_id: &str, group_id: &str) -> String {
    format!("{}:p:{}", session_id, group_id)
}

// The leaf progress of a group plan is the same horizon metric the Plans view
// reports, so the bucket resolves it through the shared projection (c-5).
/// Expansion key of one subplan step inside a group's unified plan.
///
/// The `:g:` segment keeps a step key disjoint from the `{session}:{item}` key
/// of an expanded finding and the `{session}:g:{group}` key of its group
/// header, so the expansion scopes never collide and clearing one scope never
/// drops another.
pub(super) fn subplan_key(session_id: &str, group_id: &str, step_id: &str) -> String {
    format!("{}:g:{}:s:{}", session_id, group_id, step_id)
}

/// Records every step of a group plan as expanded.
pub(super) fn collect_subplan_keys(
    session_id: &str,
    group_id: &str,
    steps: &[PlanItem],
    out: &mut HashSet<String>,
) {
    for step in steps {
        out.insert(subplan_key(session_id, group_id, &step.id));
        collect_subplan_keys(session_id, group_id, &step.children, out);
    }
}

/// Whether every step of a group plan is expanded.
pub(super) fn subplan_all_expanded(
    expanded: &HashSet<String>,
    session_id: &str,
    group_id: &str,
    steps: &[PlanItem],
) -> bool {
    steps.iter().all(|step| {
        expanded.contains(&subplan_key(session_id, group_id, &step.id))
            && subplan_all_expanded(expanded, session_id, group_id, &step.children)
    })
}

/// Whether a finding reached a terminal, non-actionable status.
///
/// Group headers count the rows they actually render, so the open-findings
/// filter must be applied identically by the row builder and the row counter.
fn is_closed(status: IssueStatus) -> bool {
    matches!(
        status,
        IssueStatus::Fixed | IssueStatus::Skipped | IssueStatus::Rejected
    )
}

/// Title of a protective bucket collecting findings of an unknown group.
pub(super) fn orphan_title(group_id: &str) -> String {
    if group_id == DEFAULT_REVIEW_GROUP_ID {
        "Default".to_string()
    } else {
        group_id.to_string()
    }
}

/// Normalizes a model-authored group diagnostic into a renderable value.
///
/// The blocker and the recorded decision are authored by the Architect inside a
/// protocol payload, so a blank or whitespace-only value carries no information
/// and is dropped before it reaches a row, while a surviving value loses its
/// surrounding whitespace (Conventions §4.3).
fn normalized_diagnostic(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Findings one group renders, in render order.
///
/// A declared group, the implicit `Default` group of a legacy session, and the
/// protective orphan bucket resolve their member list through this one filter,
/// so the buckets can never disagree about which finding belongs to which
/// header (concept #c-1, #c-3).
fn findings_of<'a>(visible: &[&'a ReviewItem], group_id: &str) -> Vec<&'a ReviewItem> {
    visible
        .iter()
        .copied()
        .filter(|item| item.group.as_deref().unwrap_or(DEFAULT_REVIEW_GROUP_ID) == group_id)
        .collect()
}

/// One group of a session, ready to be rendered.
///
/// The bucket owns the group metadata it displays and borrows both the
/// findings it holds and the plan it renders, so the row builder and the row
/// counter walk one structure instead of re-deriving the grouping twice
/// (concept #c-1, #c-3). The plan is borrowed because the tree is rebuilt on
/// every frame: copying the step graph here would deep-clone every plan of
/// every group at the streaming redraw cadence.
pub(super) struct GroupBucket<'a> {
    pub(super) id: String,
    /// Class of the group, which decides how it is executed.
    kind: ReviewGroupKind,
    title: String,
    description: Option<String>,
    /// Reason the group currently cannot proceed.
    blocked_reason: Option<String>,
    /// Architectural decision already recorded for the group.
    resolution_notes: Option<String>,
    /// Executable leaf progress of the unified plan as `(total, done)`.
    ///
    /// The bucket resolves it through the shared implementation projection the
    /// moment it is built, so the branch header of the group and the header of
    /// a plan report one number for the same horizon (c-5).
    progress: (usize, usize),
    /// Unified plan of the group, borrowed from the session that stores it.
    pub(super) subplan: &'a [PlanItem],
    subplan_architect: Option<ArchitectTier>,
    pub(super) items: Vec<&'a ReviewItem>,
}

impl ReviewViewState {
    /// Whether the active discussion focus anchors on a whole review session.
    fn is_session_focused(&self, session_id: &str) -> bool {
        matches!(
            &self.discussion_focus,
            DiscussionFocus::Focused {
                target: DiscussionTarget::Review { id },
                focused_items,
            } if id.as_str() == session_id && focused_items.is_empty()
        )
    }

    /// Whether the active discussion focus anchors on a specific finding.
    fn is_item_focused(&self, session_id: &str, item_id: u32) -> bool {
        match &self.discussion_focus {
            DiscussionFocus::Focused {
                target: DiscussionTarget::Review { id },
                focused_items,
            } if id.as_str() == session_id => {
                let key = item_id.to_string();
                focused_items.iter().any(|fid| fid == &key)
            }
            _ => false,
        }
    }

    /// Collects the visible rows of the tree in render order.
    ///
    /// The projection is owned because the renderer draws every row. A path that
    /// needs one row or only the row count uses `selected_row` or `row_count`,
    /// so the navigation hot path never builds the whole projection (c-2).
    pub fn flatten_rows(&self) -> Vec<VisibleReviewRow> {
        let mut rows = Vec::new();
        self.walk_rows(&mut |_, row| {
            rows.push(row);
            true
        });
        rows
    }

    /// Walks the visible rows in render order, handing each row together with
    /// its index to `visit` and stopping as soon as `visit` returns `false`.
    ///
    /// This walk is the single source of row order: `flatten_rows` collects it,
    /// `selected_row` stops at the cursor, and the viewport measurement stops on
    /// the row it measures. None of them can drift from another, and none of
    /// them materializes a row it does not use (c-2).
    pub(super) fn walk_rows(&self, visit: &mut dyn FnMut(usize, VisibleReviewRow) -> bool) {
        let mut index = 0usize;
        for session in self.effective_sessions() {
            let is_expanded = self.is_session_expanded(&session.id);
            let header = self.session_header_row(session.as_ref(), is_expanded);
            if !Self::emit_row(&mut *visit, &mut index, header) {
                return;
            }
            if !is_expanded {
                continue;
            }
            if let Some(desc) = session.description.as_deref() {
                let trimmed = desc.trim();
                if !trimmed.is_empty() {
                    let summary = VisibleReviewRow::SessionSummary {
                        session_id: session.id.clone(),
                        description: trimmed.to_string(),
                        is_expanded: self.is_summary_expanded(&session.id),
                    };
                    if !Self::emit_row(&mut *visit, &mut index, summary) {
                        return;
                    }
                }
            }
            for bucket in self.group_buckets(session.as_ref()) {
                let group_expanded = self.is_group_expanded(&session.id, &bucket.id);
                let has_plan = !bucket.subplan.is_empty();
                let header = VisibleReviewRow::GroupHeader {
                    session_id: session.id.clone(),
                    group_id: bucket.id.clone(),
                    kind: bucket.kind,
                    title: bucket.title,
                    description: bucket.description,
                    blocked_reason: bucket.blocked_reason,
                    resolution_notes: bucket.resolution_notes,
                    findings_count: bucket.items.len(),
                    has_plan,
                    is_expanded: group_expanded,
                };
                if !Self::emit_row(&mut *visit, &mut index, header) {
                    return;
                }
                if !group_expanded {
                    continue;
                }
                // The unified plan of a group is the execution mechanism of its
                // root cause, so it owns a branch of its own: the branch renders
                // once under the header, its steps sit strictly deeper, and the
                // findings close the group on their own band instead of mixing
                // with the plan (concept #c-1).
                if has_plan {
                    let (total, done) = bucket.progress;
                    let branch_expanded = self.is_plan_branch_expanded(&session.id, &bucket.id);
                    let branch = VisibleReviewRow::GroupPlanHeader {
                        session_id: session.id.clone(),
                        group_id: bucket.id.clone(),
                        title: active_t("review.group_plan_branch").to_string(),
                        done,
                        total,
                        subplan_architect: bucket.subplan_architect,
                        is_expanded: branch_expanded,
                    };
                    if !Self::emit_row(&mut *visit, &mut index, branch) {
                        return;
                    }
                    if branch_expanded
                        && !self.walk_subplan_rows(
                            &session.id,
                            &bucket.id,
                            bucket.subplan,
                            0,
                            &mut index,
                            &mut *visit,
                        )
                    {
                        return;
                    }
                }
                for stored in bucket.items {
                    let issue = ReviewIssue::from(stored.clone());
                    if !self.emit_item_row(&session.id, &issue, &mut index, &mut *visit) {
                        return;
                    }
                }
            }
        }
    }

    /// Hands one row to the walk visitor and reports whether the walk continues.
    ///
    /// Every branch of the projection emits its rows through this single point,
    /// so the index a consumer sees always matches render order.
    fn emit_row(
        visit: &mut dyn FnMut(usize, VisibleReviewRow) -> bool,
        index: &mut usize,
        row: VisibleReviewRow,
    ) -> bool {
        let current = *index;
        *index += 1;
        visit(current, row)
    }
    /// Builds the header row of one session from its aggregate statistics.
    fn session_header_row(&self, session: &ReviewSession, is_expanded: bool) -> VisibleReviewRow {
        let count = |status: IssueStatus| {
            session
                .items
                .iter()
                .filter(|item| item.status == status)
                .count()
        };
        VisibleReviewRow::SessionHeader {
            session_id: session.id.clone(),
            title: if session.title.trim().is_empty() {
                session.id.clone()
            } else {
                session.title.clone()
            },
            description: session.description.clone(),
            created_at: session.created_at,
            model: session.model.clone(),
            total: session.items.len(),
            fixed: count(IssueStatus::Fixed),
            todo: count(IssueStatus::Todo),
            in_progress: count(IssueStatus::InProgress),
            discussion: count(IssueStatus::Discussion),
            skipped: count(IssueStatus::Skipped),
            rejected: count(IssueStatus::Rejected),
            is_expanded,
            focused: self.is_session_focused(&session.id),
        }
    }

    /// Emits one finding row of a group.
    ///
    /// The finding never owns subplan rows: the unified plan belongs to its
    /// group and is projected once under the group header (concept #c-5).
    fn emit_item_row(
        &self,
        session_id: &str,
        issue: &ReviewIssue,
        index: &mut usize,
        visit: &mut dyn FnMut(usize, VisibleReviewRow) -> bool,
    ) -> bool {
        let row = VisibleReviewRow::ReviewItem {
            session_id: session_id.to_string(),
            item_id: issue.id,
            title: issue.title.clone(),
            issue_type: issue.issue_type,
            status: issue.status,
            commit: issue.commit.clone(),
            model: issue.model.clone(),
            files: issue.files.clone(),
            readonly_files: issue.readonly_files.clone(),
            description: issue.description.clone(),
            recommendations: issue.recommendations.clone(),
            note: issue.note.clone(),
            is_expanded: self.is_item_expanded(session_id, issue.id),
            focused: self.is_item_focused(session_id, issue.id),
        };
        Self::emit_row(visit, index, row)
    }

    /// Groups of one session in render order.
    ///
    /// Declared groups render in configuration order, including a group whose
    /// visible findings are all filtered out; a finding whose group identifier
    /// is not declared renders last in a protective orphan bucket, in the order
    /// the identifiers first appear. The open-findings filter is applied before
    /// the buckets are built, so a header count always equals the rows rendered
    /// under it (concept #c-1, #c-3).
    pub(super) fn group_buckets<'a>(&self, session: &'a ReviewSession) -> Vec<GroupBucket<'a>> {
        let visible: Vec<&ReviewItem> = session
            .items
            .iter()
            .filter(|item| !self.hide_closed || !is_closed(item.status))
            .collect();
        // The stored groups are walked in place. Resolving the effective groups
        // would clone every group of the session — its whole plan tree included
        // — on every call, while the projection runs once per rendered frame
        // (concept #c-1, #c-3).
        let mut buckets: Vec<GroupBucket<'a>> = Vec::new();
        if session.groups.is_empty() {
            // A session written before grouping existed, or one launched as a
            // single audit, is the degenerate case of one implicit `Default`
            // group: it owns no stored plan, so the bucket borrows an empty step
            // slice, and an empty implicit group renders no header at all.
            let items = findings_of(&visible, DEFAULT_REVIEW_GROUP_ID);
            if !items.is_empty() {
                buckets.push(GroupBucket {
                    id: DEFAULT_REVIEW_GROUP_ID.to_string(),
                    kind: ReviewGroupKind::Simple,
                    title: orphan_title(DEFAULT_REVIEW_GROUP_ID),
                    description: None,
                    blocked_reason: None,
                    resolution_notes: None,
                    progress: (0, 0),
                    subplan: &[],
                    subplan_architect: None,
                    items,
                });
            }
        } else {
            for group in &session.groups {
                buckets.push(GroupBucket {
                    id: group.id.clone(),
                    kind: group.kind,
                    title: if group.title.trim().is_empty() {
                        group.id.clone()
                    } else {
                        group.title.clone()
                    },
                    description: normalized_diagnostic(group.description.as_deref()),
                    blocked_reason: normalized_diagnostic(group.blocked_reason.as_deref()),
                    resolution_notes: normalized_diagnostic(group.resolution_notes.as_deref()),
                    progress: leaf_progress(&group.implementation),
                    subplan: &group.implementation.items,
                    subplan_architect: group.implementation.architect.clone(),
                    items: findings_of(&visible, &group.id),
                });
            }
        }

        let mut orphan_ids: Vec<String> = Vec::new();
        for item in &visible {
            let group_id = item.group.as_deref().unwrap_or(DEFAULT_REVIEW_GROUP_ID);
            let declared = if session.groups.is_empty() {
                group_id == DEFAULT_REVIEW_GROUP_ID
            } else {
                session.groups.iter().any(|group| group.id == group_id)
            };
            if declared {
                continue;
            }
            if !orphan_ids.iter().any(|known| known == group_id) {
                orphan_ids.push(group_id.to_string());
            }
        }
        for group_id in orphan_ids {
            buckets.push(GroupBucket {
                kind: ReviewGroupKind::Simple,
                title: orphan_title(&group_id),
                description: None,
                blocked_reason: None,
                resolution_notes: None,
                progress: (0, 0),
                subplan: &[],
                subplan_architect: None,
                items: findings_of(&visible, &group_id),
                id: group_id,
            });
        }
        buckets
    }

    /// Findings one group renders, in render order.
    ///
    /// The group resolves through the same bucketing the tree renders, so the
    /// returned set is exactly the rows rendered under the group header: the
    /// open-findings filter applies, and a finding whose group identifier is
    /// unknown resolves through its protective bucket.
    pub(super) fn group_findings(
        &self,
        session: &ReviewSession,
        group_id: &str,
    ) -> Vec<ReviewItem> {
        self.group_buckets(session)
            .into_iter()
            .find(|bucket| bucket.id == group_id)
            .map(|bucket| bucket.items)
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }

    /// Walks the unified plan of a group into interactive step rows.
    ///
    /// Top-level steps always follow an expanded group header; nested steps
    /// appear only after their container step is expanded, so the tree stays
    /// dense until the developer drills into a step (concept #c-5).
    fn walk_subplan_rows(
        &self,
        session_id: &str,
        group_id: &str,
        steps: &[PlanItem],
        depth: usize,
        index: &mut usize,
        visit: &mut dyn FnMut(usize, VisibleReviewRow) -> bool,
    ) -> bool {
        for step in steps {
            let has_children = !step.children.is_empty();
            let has_details = step
                .details
                .as_deref()
                .map(|details| !details.trim().is_empty())
                .unwrap_or(false);
            let has_meta = !step.files.is_empty()
                || !step.readonly_files.is_empty()
                || !step.outer_docs.is_empty()
                || !step.depends_on.is_empty();
            let has_blocked_reason = step
                .blocked_reason
                .as_deref()
                .map(|reason| !reason.trim().is_empty())
                .unwrap_or(false);
            let is_expanded = self.is_subplan_item_expanded(session_id, group_id, &step.id);

            let row = VisibleReviewRow::SubplanItem {
                session_id: session_id.to_string(),
                group_id: group_id.to_string(),
                step_id: step.id.clone(),
                title: step.title.clone(),
                details: step.details.clone(),
                status: step.status,
                commit: step.commit.clone(),
                blocked_reason: step.blocked_reason.clone(),
                files: step.files.clone(),
                readonly_files: step.readonly_files.clone(),
                outer_docs: step.outer_docs.clone(),
                depends_on: step.depends_on.clone(),
                depth,
                has_children,
                is_expandable: has_children || has_details || has_meta || has_blocked_reason,
                is_expanded,
            };
            if !Self::emit_row(&mut *visit, index, row) {
                return false;
            }

            if has_children
                && is_expanded
                && !self.walk_subplan_rows(
                    session_id,
                    group_id,
                    &step.children,
                    depth + 1,
                    index,
                    &mut *visit,
                )
            {
                return false;
            }
        }
        true
    }

    /// Number of subplan step rows a group plan contributes to the tree.
    ///
    /// Counts every step plus, while a container step is expanded, the steps of
    /// its children, mirroring `walk_subplan_rows` branch for branch.
    fn count_subplan_rows(&self, session_id: &str, group_id: &str, steps: &[PlanItem]) -> usize {
        let mut count = 0;
        for step in steps {
            count += 1;
            if !step.children.is_empty()
                && self.is_subplan_item_expanded(session_id, group_id, &step.id)
            {
                count += self.count_subplan_rows(session_id, group_id, &step.children);
            }
        }
        count
    }

    /// Number of rows one session contributes at its current fold state.
    ///
    /// Mirrors the per-session branch of `flatten_rows` branch for branch, so
    /// selection and viewport bounds never build the owned row projection just
    /// to learn its length.
    fn count_session_rows(&self, session: &ReviewSession) -> usize {
        let mut count = 1; // The session header.
        if !self.is_session_expanded(&session.id) {
            return count;
        }
        let has_summary = session
            .description
            .as_deref()
            .map(|description| !description.trim().is_empty())
            .unwrap_or(false);
        if has_summary {
            count += 1; // The Executive Summary row.
        }
        for bucket in self.group_buckets(session) {
            count += 1; // The group header.
            if !self.is_group_expanded(&session.id, &bucket.id) {
                continue;
            }
            if !bucket.subplan.is_empty() {
                count += 1; // The plan branch header.
                if self.is_plan_branch_expanded(&session.id, &bucket.id) {
                    count += self.count_subplan_rows(&session.id, &bucket.id, bucket.subplan);
                }
            }
            count += bucket.items.len(); // One finding row per rendered finding.
        }
        count
    }

    /// Number of rows the tree currently exposes, computed without allocating
    /// the row list.
    ///
    /// Selection and viewport bounds only need the count, so they read it here
    /// instead of building every owned row projection just to call `len()`. The
    /// walk mirrors `flatten_rows` branch for branch, and the regression tests
    /// pin the two to the same value in every fold state (c-2).
    pub fn row_count(&self) -> usize {
        if self.sessions.is_empty() {
            match self.session.as_ref() {
                Some(session) => self.count_session_rows(session),
                None => 0,
            }
        } else {
            self.sessions
                .iter()
                .map(|session| self.count_session_rows(session))
                .sum()
        }
    }

    /// The row under the cursor, cloned on its own.
    ///
    /// The walk stops at the cursor instead of collecting the whole tree, so
    /// reading the selected row costs one row rather than every visible one
    /// (c-2).
    pub fn selected_row(&self) -> Option<VisibleReviewRow> {
        let target = self.selected_index;
        let mut selected = None;
        self.walk_rows(&mut |index, row| {
            if index == target {
                selected = Some(row);
                false
            } else {
                true
            }
        });
        selected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{PlanImplementation, ReviewGroup, ReviewSession};

    fn make_item(id: u32) -> ReviewIssue {
        ReviewIssue {
            id,
            title: format!("Finding {}", id),
            uuid: None,
            commit: None,
            group: None,
            model: None,
            issue_type: IssueType::Architecture,
            status: IssueStatus::Todo,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            note: None,
        }
    }

    fn make_session(id: &str, item_ids: &[u32]) -> ReviewSession {
        ReviewSession {
            id: id.to_string(),
            title: format!("Session {}", id),
            created_at: 0,
            model: ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: item_ids
                .iter()
                .map(|&i| ReviewItem::from(make_item(i)))
                .collect(),
            raw_markdown: String::new(),
            groups: Vec::new(),
            post_analysis: None,
            launch_config: None,
        }
    }

    fn subplan_step(id: &str, title: &str, status: PlanItemStatus) -> PlanItem {
        PlanItem {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..Default::default()
        }
    }

    /// A session whose summary, findings, and subplan steps all carry expansion
    /// state, so every branch of the row walk is exercised.
    fn row_count_fixture() -> ReviewViewState {
        let mut session = make_session("rev-count", &[1, 2]);
        session.description = Some("Executive summary body.".to_string());
        session.groups = vec![ReviewGroup {
            id: "arch".to_string(),
            title: "Shared cause".to_string(),
            kind: ReviewGroupKind::Architectural,
            implementation: PlanImplementation {
                items: vec![PlanItem {
                    children: vec![subplan_step("1.1", "Leaf step", PlanItemStatus::Todo)],
                    ..subplan_step("1", "Container step", PlanItemStatus::Todo)
                }],
                ..Default::default()
            },
            ..Default::default()
        }];
        session.items[0].group = Some("arch".to_string());
        session.items[1] = ReviewItem::from(ReviewIssue {
            status: IssueStatus::Fixed,
            ..make_item(2)
        });
        ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        }
    }

    fn assert_row_count_matches(view: &ReviewViewState, state: &str) {
        assert_eq!(
            view.row_count(),
            view.flatten_rows().len(),
            "row_count must match the flattened rows while {state}"
        );
    }

    /// A session with two declared groups, an implicit default finding, and an
    /// orphan finding whose group id no longer exists in the configuration.
    fn grouped_session() -> ReviewSession {
        let mut session = make_session("rev-groups", &[1, 2, 3]);
        session.groups = vec![
            ReviewGroup {
                id: "alpha".to_string(),
                title: "Alpha slice".to_string(),
                paths: vec!["src/alpha/".to_string()],
                ..Default::default()
            },
            ReviewGroup {
                id: "beta".to_string(),
                title: "Beta slice".to_string(),
                paths: vec!["src/beta/".to_string()],
                ..Default::default()
            },
        ];
        session.items[0].group = Some("alpha".to_string());
        session.items[2].group = Some("orphan".to_string());
        session
    }

    #[test]
    fn test_group_layout_orders_declared_groups_and_orphans() {
        let mut view = ReviewViewState {
            sessions: vec![grouped_session()],
            ..Default::default()
        };

        let rows = view.flatten_rows();
        assert!(matches!(rows[0], VisibleReviewRow::SessionHeader { .. }));
        assert!(matches!(
            &rows[1],
            VisibleReviewRow::GroupHeader { group_id, findings_count: 1, .. }
                if group_id.as_str() == "alpha"
        ));
        assert!(matches!(
            &rows[2],
            VisibleReviewRow::ReviewItem { item_id: 1, .. }
        ));
        assert!(matches!(
            &rows[3],
            VisibleReviewRow::GroupHeader { group_id, findings_count: 0, .. }
                if group_id.as_str() == "beta"
        ));
        assert!(matches!(
            &rows[4],
            VisibleReviewRow::GroupHeader { group_id, findings_count: 1, .. }
                if group_id.as_str() == tauqe_protocol::review::DEFAULT_REVIEW_GROUP_ID
        ));
        assert!(matches!(
            &rows[5],
            VisibleReviewRow::ReviewItem { item_id: 2, .. }
        ));
        assert!(matches!(
            &rows[6],
            VisibleReviewRow::GroupHeader { group_id, findings_count: 1, .. }
                if group_id.as_str() == "orphan"
        ));
        assert!(matches!(
            &rows[7],
            VisibleReviewRow::ReviewItem { item_id: 3, .. }
        ));
        assert_eq!(view.row_count(), rows.len());

        // Folding one group hides only its own findings.
        view.toggle_group_expanded("rev-groups", "alpha");
        let rows = view.flatten_rows();
        assert!(matches!(
            &rows[1],
            VisibleReviewRow::GroupHeader {
                is_expanded: false,
                ..
            }
        ));
        assert!(!rows
            .iter()
            .any(|row| matches!(row, VisibleReviewRow::ReviewItem { item_id: 1, .. })));
        assert_eq!(view.row_count(), rows.len());

        // The open-findings filter is reflected by the header counters.
        view.toggle_group_expanded("rev-groups", "alpha");
        view.sessions[0].items[0].status = IssueStatus::Fixed;
        view.toggle_hide_closed();
        let rows = view.flatten_rows();
        assert!(matches!(
            &rows[1],
            VisibleReviewRow::GroupHeader { group_id, findings_count: 0, .. }
                if group_id.as_str() == "alpha"
        ));
        assert_eq!(view.row_count(), rows.len());
    }

    #[test]
    fn test_group_plan_owns_a_branch_below_its_group_header() {
        let mut session = make_session("rev-plan", &[1]);
        session.groups = vec![ReviewGroup {
            id: "g1".to_string(),
            title: "Root cause".to_string(),
            description: Some("One cause behind both symptoms.".to_string()),
            kind: ReviewGroupKind::Architectural,
            implementation: PlanImplementation {
                items: vec![PlanItem {
                    children: vec![subplan_step("1.1", "Leaf", PlanItemStatus::Done)],
                    ..subplan_step("1", "Unify the guard", PlanItemStatus::Todo)
                }],
                architect: Some(ArchitectTier::new(
                    ModelRef::openrouter("planner/model"),
                    None,
                )),
                ..Default::default()
            },
            ..Default::default()
        }];
        session.items[0].group = Some("g1".to_string());

        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        match &view.flatten_rows()[1] {
            VisibleReviewRow::GroupHeader {
                kind,
                group_id,
                description,
                findings_count,
                has_plan,
                is_expanded,
                ..
            } => {
                assert_eq!(*kind, ReviewGroupKind::Architectural);
                assert_eq!(group_id, "g1");
                assert_eq!(
                    description.as_deref(),
                    Some("One cause behind both symptoms.")
                );
                assert_eq!(*findings_count, 1);
                assert!(*has_plan);
                assert!(*is_expanded);
            }
            other => panic!("expected a group header, got {other:?}"),
        }
        assert_eq!(view.row_count(), view.flatten_rows().len());

        // The plan owns a branch directly under the group header, and its steps
        // render strictly deeper than the branch (concept #c-1).
        let rows = view.flatten_rows();
        assert!(matches!(
            &rows[2],
            VisibleReviewRow::GroupPlanHeader {
                group_id,
                done,
                total,
                subplan_architect,
                is_expanded,
                ..
            } if group_id == "g1"
                && *done == 1
                && *total == 1
                && subplan_architect.is_some()
                && *is_expanded
        ));
        assert!(matches!(
            &rows[3],
            VisibleReviewRow::SubplanItem { group_id, step_id, depth: 0, .. }
                if group_id == "g1" && step_id == "1"
        ));
        assert!(matches!(
            &rows[4],
            VisibleReviewRow::ReviewItem { item_id: 1, .. }
        ));
        assert!(!rows.iter().any(
            |row| matches!(row, VisibleReviewRow::SubplanItem { step_id, .. } if step_id == "1.1")
        ));

        // Folding the branch hides its steps but never the findings of the
        // group, and the counters mirror the rendered rows (concept #c-4).
        view.toggle_plan_branch_expanded("rev-plan", "g1");
        assert_eq!(view.row_count(), view.flatten_rows().len());
        let rows = view.flatten_rows();
        assert!(!rows
            .iter()
            .any(|row| matches!(row, VisibleReviewRow::SubplanItem { .. })));
        assert!(rows
            .iter()
            .any(|row| matches!(row, VisibleReviewRow::ReviewItem { item_id: 1, .. })));
        assert_eq!(
            view.row_count(),
            4,
            "a session header, its group, the folded branch, and the finding"
        );
        view.toggle_plan_branch_expanded("rev-plan", "g1");
        assert_eq!(view.row_count(), view.flatten_rows().len());

        // Folding the group hides its whole plan, so no step survives as an
        // independent row and the tree shrinks back to the header pair.
        view.toggle_group_expanded("rev-plan", "g1");
        assert_eq!(view.row_count(), view.flatten_rows().len());
        assert_eq!(view.row_count(), 2);
    }

    #[test]
    fn test_row_count_matches_flattened_rows_across_group_states() {
        let mut session = grouped_session();
        session.items[0].status = IssueStatus::Fixed;
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        assert_row_count_matches(&view, "the groups are expanded");
        view.toggle_group_expanded("rev-groups", "alpha");
        assert_row_count_matches(&view, "one group is folded");
        view.toggle_hide_closed();
        assert_row_count_matches(&view, "closed findings are hidden");
        view.collapse_all();
        assert_row_count_matches(&view, "every row is collapsed");
        view.expand_all();
        assert_row_count_matches(&view, "every row is expanded again");
    }

    #[test]
    fn test_row_count_matches_flattened_rows_across_fold_states() {
        let mut view = row_count_fixture();
        assert_row_count_matches(&view, "the session is expanded");

        view.toggle_plan_branch_expanded("rev-count", "arch");
        assert_row_count_matches(&view, "the plan branch is folded");
        view.toggle_plan_branch_expanded("rev-count", "arch");
        assert_row_count_matches(&view, "the plan branch is expanded again");
        view.toggle_item_expanded("rev-count", 1);
        assert_row_count_matches(&view, "the finding is expanded");

        view.toggle_subplan_item_expanded("rev-count", "arch", "1");
        assert_row_count_matches(&view, "the container step is expanded");
        view.toggle_subplan_item_expanded("rev-count", "arch", "1");
        assert_row_count_matches(&view, "the container step is folded");

        view.toggle_summary_expanded("rev-count");
        assert_row_count_matches(&view, "the Executive Summary is folded");

        view.toggle_hide_closed();
        assert_row_count_matches(&view, "closed findings are filtered out");

        view.toggle_session_expanded("rev-count");
        assert_row_count_matches(&view, "the session is collapsed");

        view.expand_all();
        assert_row_count_matches(&view, "every row is expanded");

        view.collapse_all();
        assert_row_count_matches(&view, "every row is collapsed");
    }

    #[test]
    fn test_row_count_matches_flattened_rows_for_the_single_active_session() {
        let mut session = make_session("rev-active", &[1]);
        session.description = Some("Summary body.".to_string());
        let view = ReviewViewState {
            session: Some(session),
            ..Default::default()
        };

        assert_eq!(view.row_count(), view.flatten_rows().len());
        assert_eq!(
            view.row_count(),
            4,
            "a header, its summary, one group header, and one finding"
        );
    }
    /// A blocked group states its diagnosis and its recorded decision on the
    /// header row, so the tree can badge the header and open the architectural
    /// fork as the first section of the group body (concept #c-2).
    #[test]
    fn test_group_header_carries_the_blocker_and_the_recorded_decision() {
        fn diagnostic_session(
            id: &str,
            blocked: Option<&str>,
            resolution: Option<&str>,
        ) -> ReviewSession {
            let mut session = make_session(id, &[1]);
            session.groups = vec![ReviewGroup {
                id: "arch".to_string(),
                title: "Shared cause".to_string(),
                kind: ReviewGroupKind::Architectural,
                blocked_reason: blocked.map(str::to_string),
                resolution_notes: resolution.map(str::to_string),
                ..Default::default()
            }];
            session.items[0].group = Some("arch".to_string());
            session
        }

        let blocked = ReviewViewState {
            sessions: vec![diagnostic_session(
                "rev-blocked",
                Some("  Two invariants contradict each other.  "),
                Some("\nReuse the existing policy object.\n"),
            )],
            ..Default::default()
        };
        match &blocked.flatten_rows()[1] {
            VisibleReviewRow::GroupHeader {
                blocked_reason,
                resolution_notes,
                ..
            } => {
                assert_eq!(
                    blocked_reason.as_deref(),
                    Some("Two invariants contradict each other.")
                );
                assert_eq!(
                    resolution_notes.as_deref(),
                    Some("Reuse the existing policy object.")
                );
            }
            other => panic!("expected a group header, got {other:?}"),
        }

        // A whitespace-only payload carries no information, so a blank
        // diagnostic never badges a healthy group.
        let healthy = ReviewViewState {
            sessions: vec![diagnostic_session("rev-healthy", Some("   "), None)],
            ..Default::default()
        };
        match &healthy.flatten_rows()[1] {
            VisibleReviewRow::GroupHeader {
                blocked_reason,
                resolution_notes,
                ..
            } => {
                assert!(blocked_reason.is_none());
                assert!(resolution_notes.is_none());
            }
            other => panic!("expected a group header, got {other:?}"),
        }
    }

    /// The bucket borrows the plan the session stores instead of copying it:
    /// the tree is rebuilt on every frame, so a deep clone of every group plan
    /// per frame is exactly the cost this projection must not pay (c-1, c-5).
    #[test]
    fn test_group_bucket_borrows_the_stored_plan() {
        let mut session = make_session("rev-borrow", &[1]);
        session.groups = vec![ReviewGroup {
            id: "arch".to_string(),
            title: "Shared cause".to_string(),
            kind: ReviewGroupKind::Architectural,
            implementation: PlanImplementation {
                items: vec![subplan_step("1", "Unify the guard", PlanItemStatus::Todo)],
                ..Default::default()
            },
            ..Default::default()
        }];
        session.items[0].group = Some("arch".to_string());
        let view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        let buckets = view.group_buckets(&view.sessions[0]);
        let bucket = buckets
            .iter()
            .find(|bucket| bucket.id == "arch")
            .expect("the declared group must be bucketed");
        assert_eq!(
            bucket.subplan.as_ptr(),
            view.sessions[0].groups[0].implementation.items.as_ptr(),
            "the bucket must borrow the stored plan instead of cloning it"
        );
        assert_eq!(bucket.subplan.len(), 1);
    }
}
