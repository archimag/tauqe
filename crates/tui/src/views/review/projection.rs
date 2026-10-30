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
        files: Vec<String>,
        readonly_files: Vec<String>,
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

/// Executable leaf progress of a unified plan as `(done, total)`.
///
/// Mirrors `Plan::count_stats`: only leaves carry an explicit status, cancelled
/// leaves are excluded from the total, and `done` counts the finished leaves.
/// The branch header and the renderer therefore report one number.
fn subplan_progress(items: &[PlanItem]) -> (usize, usize) {
    fn walk(items: &[PlanItem], total: &mut usize, done: &mut usize) {
        for item in items {
            if item.is_leaf() {
                if item.status == PlanItemStatus::Cancelled {
                    continue;
                }
                *total += 1;
                if item.status == PlanItemStatus::Done {
                    *done += 1;
                }
            } else {
                walk(&item.children, total, done);
            }
        }
    }

    let mut total = 0;
    let mut done = 0;
    walk(items, &mut total, &mut done);
    (done, total)
}
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

/// One group of a session, ready to be rendered.
///
/// The bucket owns the group metadata it displays and borrows the findings it
/// holds, so the row builder and the row counter walk one structure instead of
/// re-deriving the grouping twice (concept #c-1, #c-3).
pub(super) struct GroupBucket<'a> {
    pub(super) id: String,
    /// Class of the group, which decides how it is executed.
    kind: ReviewGroupKind,
    title: String,
    description: Option<String>,
    pub(super) subplan: Vec<PlanItem>,
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

    pub fn flatten_rows(&self) -> Vec<VisibleReviewRow> {
        let mut rows = Vec::new();
        for session in &self.effective_sessions() {
            let is_expanded = self.is_session_expanded(&session.id);
            rows.push(self.session_header_row(session, is_expanded));
            if !is_expanded {
                continue;
            }
            if let Some(desc) = session.description.as_deref() {
                let trimmed = desc.trim();
                if !trimmed.is_empty() {
                    rows.push(VisibleReviewRow::SessionSummary {
                        session_id: session.id.clone(),
                        description: trimmed.to_string(),
                        is_expanded: self.is_summary_expanded(&session.id),
                    });
                }
            }
            for bucket in self.group_buckets(session) {
                let group_expanded = self.is_group_expanded(&session.id, &bucket.id);
                let has_plan = !bucket.subplan.is_empty();
                rows.push(VisibleReviewRow::GroupHeader {
                    session_id: session.id.clone(),
                    group_id: bucket.id.clone(),
                    kind: bucket.kind,
                    title: bucket.title,
                    description: bucket.description,
                    findings_count: bucket.items.len(),
                    has_plan,
                    is_expanded: group_expanded,
                });
                if !group_expanded {
                    continue;
                }
                // The unified plan of a group is the execution mechanism of its
                // root cause, so it owns a branch of its own: the branch renders
                // once under the header, its steps sit strictly deeper, and the
                // findings close the group on their own band instead of mixing
                // with the plan (concept #c-1).
                if has_plan {
                    let (done, total) = subplan_progress(&bucket.subplan);
                    let branch_expanded = self.is_plan_branch_expanded(&session.id, &bucket.id);
                    rows.push(VisibleReviewRow::GroupPlanHeader {
                        session_id: session.id.clone(),
                        group_id: bucket.id.clone(),
                        title: active_t("review.group_plan_branch").to_string(),
                        done,
                        total,
                        subplan_architect: bucket.subplan_architect,
                        is_expanded: branch_expanded,
                    });
                    if branch_expanded {
                        self.flatten_subplan_rows(
                            &session.id,
                            &bucket.id,
                            &bucket.subplan,
                            0,
                            &mut rows,
                        );
                    }
                }
                for stored in bucket.items {
                    let issue = ReviewIssue::from(stored.clone());
                    self.push_item_rows(&session.id, &issue, &mut rows);
                }
            }
        }
        rows
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

    /// Appends one finding row of a group.
    ///
    /// The finding never owns subplan rows: the unified plan belongs to its
    /// group and is projected once under the group header (concept #c-5).
    fn push_item_rows(
        &self,
        session_id: &str,
        issue: &ReviewIssue,
        rows: &mut Vec<VisibleReviewRow>,
    ) {
        rows.push(VisibleReviewRow::ReviewItem {
            session_id: session_id.to_string(),
            item_id: issue.id,
            title: issue.title.clone(),
            issue_type: issue.issue_type,
            status: issue.status,
            model: issue.model.clone(),
            files: issue.files.clone(),
            readonly_files: issue.readonly_files.clone(),
            description: issue.description.clone(),
            recommendations: issue.recommendations.clone(),
            note: issue.note.clone(),
            is_expanded: self.is_item_expanded(session_id, issue.id),
            focused: self.is_item_focused(session_id, issue.id),
        });
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
        let declared = session.effective_groups();
        let implicit_default = session.groups.is_empty();

        let mut buckets: Vec<GroupBucket<'a>> = Vec::new();
        for group in &declared {
            let items: Vec<&ReviewItem> = visible
                .iter()
                .copied()
                .filter(|item| {
                    item.group.as_deref().unwrap_or(DEFAULT_REVIEW_GROUP_ID) == group.id.as_str()
                })
                .collect();
            // A legacy session synthesizes one group from its target files; an
            // empty implicit group would render a header over nothing.
            if implicit_default && items.is_empty() {
                continue;
            }
            buckets.push(GroupBucket {
                id: group.id.clone(),
                kind: group.kind,
                title: if group.title.trim().is_empty() {
                    group.id.clone()
                } else {
                    group.title.clone()
                },
                description: group
                    .description
                    .as_deref()
                    .map(str::trim)
                    .filter(|description| !description.is_empty())
                    .map(str::to_string),
                subplan: group.subplan.clone(),
                subplan_architect: group.subplan_architect.clone(),
                items,
            });
        }

        let mut orphan_ids: Vec<String> = Vec::new();
        for item in &visible {
            let group_id = item.group.as_deref().unwrap_or(DEFAULT_REVIEW_GROUP_ID);
            if declared.iter().any(|group| group.id == group_id) {
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
                subplan: Vec::new(),
                subplan_architect: None,
                items: visible
                    .iter()
                    .copied()
                    .filter(|item| {
                        item.group.as_deref().unwrap_or(DEFAULT_REVIEW_GROUP_ID)
                            == group_id.as_str()
                    })
                    .collect(),
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

    /// Flattens the unified plan of a group into interactive step rows.
    ///
    /// Top-level steps always follow an expanded group header; nested steps
    /// appear only after their container step is expanded, so the tree stays
    /// dense until the developer drills into a step (concept #c-5).
    fn flatten_subplan_rows(
        &self,
        session_id: &str,
        group_id: &str,
        steps: &[PlanItem],
        depth: usize,
        out: &mut Vec<VisibleReviewRow>,
    ) {
        for step in steps {
            let has_children = !step.children.is_empty();
            let has_details = step
                .details
                .as_deref()
                .map(|details| !details.trim().is_empty())
                .unwrap_or(false);
            let has_meta = !step.files.is_empty()
                || !step.readonly_files.is_empty()
                || !step.depends_on.is_empty();
            let is_expanded = self.is_subplan_item_expanded(session_id, group_id, &step.id);

            out.push(VisibleReviewRow::SubplanItem {
                session_id: session_id.to_string(),
                group_id: group_id.to_string(),
                step_id: step.id.clone(),
                title: step.title.clone(),
                details: step.details.clone(),
                status: step.status,
                files: step.files.clone(),
                readonly_files: step.readonly_files.clone(),
                depends_on: step.depends_on.clone(),
                depth,
                has_children,
                is_expandable: has_children || has_details || has_meta,
                is_expanded,
            });

            if has_children && is_expanded {
                self.flatten_subplan_rows(session_id, group_id, &step.children, depth + 1, out);
            }
        }
    }

    /// Number of subplan step rows a group plan contributes to the tree.
    ///
    /// Counts every step plus, while a container step is expanded, the steps of
    /// its children, mirroring `flatten_subplan_rows` branch for branch.
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
                    count += self.count_subplan_rows(&session.id, &bucket.id, &bucket.subplan);
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

    pub fn selected_row(&self) -> Option<VisibleReviewRow> {
        let rows = self.flatten_rows();
        rows.get(self.selected_index).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ReviewGroup, ReviewSession};

    fn make_item(id: u32) -> ReviewIssue {
        ReviewIssue {
            id,
            title: format!("Finding {}", id),
            uuid: None,
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
            subplan: vec![PlanItem {
                children: vec![subplan_step("1.1", "Leaf step", PlanItemStatus::Todo)],
                ..subplan_step("1", "Container step", PlanItemStatus::Todo)
            }],
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
            subplan: vec![PlanItem {
                children: vec![subplan_step("1.1", "Leaf", PlanItemStatus::Done)],
                ..subplan_step("1", "Unify the guard", PlanItemStatus::Todo)
            }],
            subplan_architect: Some(ArchitectTier::new(
                ModelRef::openrouter("planner/model"),
                None,
            )),
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
}
