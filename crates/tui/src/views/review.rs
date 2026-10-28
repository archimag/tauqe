//! Review view state: sessions, fold state, navigation, and the refine flow.
//!
//! The pure projection machinery — the `VisibleReviewRow` row model and the
//! group bucketing that collects findings under the group owning them — lives
//! in `views::review::projection`, and row rendering lives in
//! `views::review::tree`. This module keeps the interactive view state, the
//! embedded refine dialog, and the viewport behavior (Conventions §3.5).

use std::collections::{HashSet, VecDeque};

use tauqe_protocol::{
    DiscussionFocus, IssueStatus, ModelEntry, PlanItem, ReviewIssue, ReviewItem, ReviewSession,
};

use crate::components::model_selector::ModelSelector;
use crate::markdown::MarkdownBodyCache;
use crate::views::recenter::RecenterState;

pub mod input;
pub mod projection;
pub mod render;
pub mod tree;

pub use self::input::{handle_review_key, REVIEW_COMMAND_MAP};
pub use self::projection::VisibleReviewRow;
use self::projection::{collect_subplan_keys, group_key, subplan_all_expanded, subplan_key};
pub use self::render::render_review_view;
pub(crate) use self::tree::{
    compute_unified_review_lines, format_created_at, issue_status_label, issue_status_style,
    issue_type_badge, issue_type_style, location, model_badge, plan_status_style,
};
/// A finding targeted for Architect decomposition.
/// The Architect decomposes one finding per bounded call, so a batch refinement
/// keeps an ordered queue of the findings still awaiting decomposition and
/// drains it one response at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefineTarget {
    /// Review session the finding belongs to.
    pub review_id: String,
    /// Numeric finding id inside that session.
    pub item_id: u32,
}

/// Review-finding decomposition dialog state.
///
/// The Architect decomposes one finding per bounded call, so `r` opens this
/// dialog to choose the architect model and, optionally, to describe how the
/// finding should be decomposed. Submitting dispatches `review/refineItem` with
/// the selected model and directives, keeping the "what" decision explicit
/// instead of an implicit retry (Vision.md §1.2).
#[derive(Debug, Clone)]
pub struct ReviewRefineDialogState {
    pub review_id: String,
    pub review_title: String,
    pub item_id: u32,
    pub item_title: String,
    pub selector: ModelSelector,
    pub instructions_editor: crate::editor::InputEditor,
    /// Section that currently owns the keyboard.
    pub focus: crate::views::plans::RefineDialogFocus,
    pub confirm_button: crate::app::ConfirmDialogButton,
}

impl ReviewRefineDialogState {
    /// Builds the dialog over the configured `architect_entries`.
    ///
    /// The entries arrive from the role configuration exactly as the daemon
    /// reported them, so each one still carries the escalation marker the
    /// shared selector renders: a finding can be decomposed manually on an
    /// Architect the ladder never escalates to, and the dialog says so (c-4).
    pub fn new(
        review_id: String,
        review_title: String,
        item_id: u32,
        item_title: String,
        architect_entries: Vec<ModelEntry>,
    ) -> Self {
        Self {
            review_id,
            review_title,
            item_id,
            item_title,
            selector: ModelSelector::new(
                tauqe_protocol::ModelRole::Architect,
                architect_entries,
                0,
            ),
            instructions_editor: crate::editor::InputEditor::default(),
            focus: crate::views::plans::RefineDialogFocus::default(),
            confirm_button: crate::app::ConfirmDialogButton::Cancel,
        }
    }

    /// Currently selected architect entry, if any.
    pub fn selected_entry(&self) -> Option<&ModelEntry> {
        self.selector.selected_entry()
    }
}

/// Group whose isolated audit is currently streaming.
///
/// Mirrored verbatim from its `review/groupStarted` event, which carries no
/// ordinal and no review identity: the progress badge derives the group's
/// position from the supervisor's sequential traversal and its total from
/// the mirrored session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningReviewGroup {
    /// Identifier of the group inside its session.
    pub id: String,
    /// Human-readable title rendered beside the progress badge.
    pub title: String,
}

#[derive(Debug, Clone, Default)]
pub struct ReviewViewState {
    pub running: bool,
    /// Group whose isolated audit is currently streaming.
    ///
    /// Mirrored from `review/groupStarted` and closed method-authoritatively
    /// by `review/groupFinished`, which never depends on its payload.
    pub running_group: Option<RunningReviewGroup>,
    /// Groups of the current run that already finished, in supervisor order.
    ///
    /// The supervisor audits groups sequentially, so the running group's
    /// one-based position is `finished_groups + 1`.
    pub finished_groups: usize,
    /// Whether the Architect post-analysis layer is classifying the findings.
    pub analyzing: bool,
    /// Review id currently under Architect decomposition (`review/refineItem`),
    /// if any. Drives the in-flight footer indicator and blocks re-entry.
    pub refining: Option<String>,
    pub sessions: Vec<ReviewSession>,
    pub session: Option<ReviewSession>,
    pub selected_index: usize,
    pub scroll: u16,
    pub collapsed_sessions: HashSet<String>,
    pub collapsed_summaries: HashSet<String>,
    /// Folded groups, keyed by session and group through `group_key`.
    ///
    /// Groups are expanded by default and folding one is presentation state
    /// only, so the session is never mutated (#c-3).
    pub collapsed_groups: HashSet<String>,
    pub expanded_items: HashSet<String>,
    pub hide_closed: bool,
    pub error: Option<String>,
    pub view_height: u16,
    pub content_width: usize,
    pub rendered_lines: usize,
    pub theme_mode: crate::config::ThemeMode,
    /// Active discussion focus mirrored from the server, used to render the
    /// `[FOCUS]` indicator. Synced from `AppState::discussion_focus` on render.
    pub discussion_focus: DiscussionFocus,
    /// Findings awaiting Architect decomposition, in dispatch order.
    ///
    /// The Architect decomposes one finding per bounded call, so a batch
    /// refinement drains this queue one `review/refineItem` response at a time.
    pub refine_queue: VecDeque<RefineTarget>,
    /// Architect decomposition dialog for the selected finding, when open.
    pub refine_dialog: Option<ReviewRefineDialogState>,
    /// Rendered Markdown bodies of the expanded finding descriptions and
    /// subplan step details, keyed by the item that owns the description.
    ///
    /// The cache keeps a parsed body alive across frames, so moving the cursor
    /// or scrolling never re-parses the Markdown of the tree, and a changed
    /// description, indent, width, or palette drops only the body it
    /// invalidated (c-1, c-3).
    pub markdown_cache: MarkdownBodyCache,
    pub recenter_state: RecenterState,
}

impl ReviewViewState {
    pub fn begin(&mut self) {
        self.running = true;
        self.error = None;
    }

    /// Clears the per-run telemetry of a grouped audit.
    ///
    /// Every run opens and settles through one authoritative point —
    /// `review/started` and the terminal review events — so the badges of a
    /// previous run never leak into the next one.
    pub fn reset_run_telemetry(&mut self) {
        self.running_group = None;
        self.finished_groups = 0;
        self.analyzing = false;
    }

    /// Mirrors the group whose isolated audit just started.
    ///
    /// The badge derives the group's position from the supervisor's
    /// sequential traversal, because the event carries no ordinal (c-4).
    pub fn begin_group(&mut self, id: String, title: String) {
        self.running_group = Some(RunningReviewGroup { id, title });
    }

    /// Closes the running group and counts it as finished.
    ///
    /// The event method is the authoritative lifecycle signal, so the group
    /// settles without reading its payload, mirroring the `model/finished`
    /// contract (c-2).
    pub fn finish_group(&mut self) {
        self.running_group = None;
        self.finished_groups += 1;
    }

    /// Total group count of the run, resolved against the mirrored session.
    ///
    /// The total is trustworthy only while the mirror holds the session the
    /// group belongs to: a session left over from an earlier run names its
    /// own groups, so an unknown group id degrades the badge to an ordinal
    /// without a denominator instead of inventing a total.
    pub fn running_group_total(&self) -> Option<usize> {
        let group = self.running_group.as_ref()?;
        let session = self.session.as_ref()?;
        let groups = session.effective_groups();
        groups
            .iter()
            .any(|g| g.id == group.id)
            .then_some(groups.len())
    }

    pub fn set_session(&mut self, session: ReviewSession) {
        self.running = false;
        let session_id = session.id.clone();
        self.update_session(session);
        // The freshly finished session's header (with its Executive Summary
        // block) must be visible immediately, so unfold the summary and reset
        // the viewport instead of leaving it where the streaming view left it.
        self.collapsed_summaries.remove(&session_id);
        self.error = None;
        self.selected_index = 0;
        self.scroll = 0;
    }

    pub fn set_sessions(&mut self, sessions: Vec<ReviewSession>, active_id: Option<String>) {
        self.sessions = sessions;
        let target_id = active_id.or_else(|| self.session.as_ref().map(|s| s.id.clone()));
        self.session = self
            .sessions
            .iter()
            .find(|s| Some(&s.id) == target_id.as_ref())
            .or_else(|| self.sessions.first())
            .cloned();
        self.clamp_selection();
    }

    pub fn update_session(&mut self, session: ReviewSession) {
        if let Some(pos) = self.sessions.iter().position(|s| s.id == session.id) {
            self.sessions[pos] = session.clone();
        } else {
            self.sessions.push(session.clone());
        }
        if self.session.as_ref().map(|s| &s.id) == Some(&session.id) || self.session.is_none() {
            self.session = Some(session);
        }
        self.clamp_selection();
    }

    /// Replaces a finding with the server-provided version.
    ///
    /// When `review_id` is set, only the session with that id is touched, so a
    /// finding with the same numeric id in another session is never modified.
    pub fn apply_item(&mut self, review_id: Option<&str>, item: ReviewIssue) {
        let in_scope = |session_id: &str| review_id.is_none_or(|id| id == session_id);
        // Session storage keeps findings in the durable `ReviewItem` form, so
        // an incoming `ReviewIssue` is projected onto that form on the way in.
        let stored: ReviewItem = item.clone().into();
        if let Some(session) = self.session.as_mut() {
            if in_scope(&session.id) {
                if let Some(existing) = session.items.iter_mut().find(|i| i.id == stored.id) {
                    *existing = stored.clone();
                }
            }
        }
        for session in &mut self.sessions {
            if in_scope(&session.id) {
                if let Some(existing) = session.items.iter_mut().find(|i| i.id == stored.id) {
                    *existing = stored.clone();
                    break;
                }
            }
        }
    }

    /// Single path for changing a finding status (optimistic update and
    /// rollback). Updates both the session list used for rendering and the
    /// active session copy, scoped by `review_id` when provided.
    pub fn set_item_status(&mut self, review_id: Option<&str>, item_id: u32, status: IssueStatus) {
        let in_scope = |session_id: &str| review_id.is_none_or(|id| id == session_id);
        if let Some(session) = self.session.as_mut() {
            if in_scope(&session.id) {
                if let Some(existing) = session.items.iter_mut().find(|i| i.id == item_id) {
                    existing.status = status;
                }
            }
        }
        for session in &mut self.sessions {
            if in_scope(&session.id) {
                if let Some(existing) = session.items.iter_mut().find(|i| i.id == item_id) {
                    existing.status = status;
                    break;
                }
            }
        }
    }

    pub fn is_session_expanded(&self, session_id: &str) -> bool {
        !self.collapsed_sessions.contains(session_id)
    }

    pub fn toggle_session_expanded(&mut self, session_id: &str) {
        if !self.collapsed_sessions.remove(session_id) {
            self.collapsed_sessions.insert(session_id.to_string());
        }
    }

    /// The Executive Summary block folds independently of the session's
    /// finding list; it defaults to expanded so a loaded session shows it.
    pub fn is_summary_expanded(&self, session_id: &str) -> bool {
        !self.collapsed_summaries.contains(session_id)
    }

    pub fn toggle_summary_expanded(&mut self, session_id: &str) {
        if !self.collapsed_summaries.remove(session_id) {
            self.collapsed_summaries.insert(session_id.to_string());
        }
    }

    /// Whether a group of the tree is open.
    pub fn is_group_expanded(&self, session_id: &str, group_id: &str) -> bool {
        !self
            .collapsed_groups
            .contains(&group_key(session_id, group_id))
    }

    pub fn toggle_group_expanded(&mut self, session_id: &str, group_id: &str) {
        let key = group_key(session_id, group_id);
        if !self.collapsed_groups.remove(&key) {
            self.collapsed_groups.insert(key);
        }
    }

    pub fn is_item_expanded(&self, session_id: &str, item_id: u32) -> bool {
        self.expanded_items
            .contains(&format!("{}:{}", session_id, item_id))
    }

    pub fn toggle_item_expanded(&mut self, session_id: &str, item_id: u32) {
        let key = format!("{}:{}", session_id, item_id);
        if !self.expanded_items.remove(&key) {
            self.expanded_items.insert(key);
        }
    }

    /// Whether a single subplan step of a finding is expanded.
    ///
    /// A leaf step reveals its details and file lists; a container step reveals
    /// its nested child steps.
    pub fn is_subplan_item_expanded(&self, session_id: &str, item_id: u32, step_id: &str) -> bool {
        self.expanded_items
            .contains(&subplan_key(session_id, item_id, step_id))
    }

    pub fn toggle_subplan_item_expanded(&mut self, session_id: &str, item_id: u32, step_id: &str) {
        let key = subplan_key(session_id, item_id, step_id);
        if !self.expanded_items.remove(&key) {
            self.expanded_items.insert(key);
        }
    }

    pub fn expand_all(&mut self) {
        self.collapsed_sessions.clear();
        self.collapsed_summaries.clear();
        self.collapsed_groups.clear();
        let effective = self.effective_sessions();
        for s in &effective {
            for issue in s.issues() {
                self.expanded_items.insert(format!("{}:{}", s.id, issue.id));
                collect_subplan_keys(&s.id, issue.id, &issue.plan, &mut self.expanded_items);
            }
        }
    }
    pub fn collapse_all(&mut self) {
        self.expanded_items.clear();
        let effective = self.effective_sessions();
        for session in &effective {
            self.collapsed_sessions.insert(session.id.clone());
        }
        let keys: Vec<String> = effective
            .iter()
            .flat_map(|session| {
                self.group_buckets(session)
                    .into_iter()
                    .map(|bucket| group_key(&session.id, &bucket.id))
            })
            .collect();
        for key in keys {
            self.collapsed_groups.insert(key);
        }
    }

    /// True only when every session, section, and finding is expanded.
    ///
    /// Mirrors `PlansViewState::is_all_expanded` so that the global fold key
    /// (`a`) behaves as a clean binary toggle: it collapses the tree only when
    /// the whole tree is already fully expanded, otherwise it expands it.
    pub fn is_all_expanded(&self) -> bool {
        if !self.collapsed_sessions.is_empty() {
            return false;
        }
        let effective = self.effective_sessions();
        if effective.is_empty() {
            return false;
        }
        effective.iter().all(|session| {
            if !self.is_summary_expanded(&session.id) {
                return false;
            }
            let groups_open = self
                .group_buckets(session)
                .iter()
                .all(|bucket| self.is_group_expanded(&session.id, &bucket.id));
            groups_open
                && session.issues().iter().all(|issue| {
                    self.expanded_items
                        .contains(&format!("{}:{}", session.id, issue.id))
                        && subplan_all_expanded(
                            &self.expanded_items,
                            &session.id,
                            issue.id,
                            &issue.plan,
                        )
                })
        })
    }

    pub fn toggle_fold_all(&mut self) {
        if self.is_all_expanded() {
            self.collapse_all();
        } else {
            self.expand_all();
        }
    }

    pub fn toggle_hide_closed(&mut self) {
        self.hide_closed = !self.hide_closed;
        self.clamp_selection();
    }

    /// Findings still awaiting Architect decomposition, in session order.
    ///
    /// A finding in `REFINE` status has been classified by the Reviewer but not
    /// yet broken into executable steps, so this set is exactly what the batch
    /// decomposition (`R`) owes the developer.
    pub fn pending_refine_targets(&self) -> Vec<RefineTarget> {
        let mut targets = Vec::new();
        for session in self.effective_sessions() {
            for item in &session.items {
                if item.status == IssueStatus::Refine {
                    targets.push(RefineTarget {
                        review_id: session.id.clone(),
                        item_id: item.id,
                    });
                }
            }
        }
        targets
    }

    /// Replaces the refine queue with `targets` and returns the first finding to
    /// dispatch, or `None` when nothing is queued.
    pub fn begin_refine_batch(&mut self, targets: Vec<RefineTarget>) -> Option<RefineTarget> {
        self.refine_queue = targets.into();
        self.next_refine_target()
    }

    /// Pops the next finding to decompose and marks the view busy while the
    /// queue drains, clearing the marker once the queue is exhausted.
    pub fn next_refine_target(&mut self) -> Option<RefineTarget> {
        match self.refine_queue.pop_front() {
            Some(target) => {
                self.refining = Some(target.review_id.clone());
                Some(target)
            }
            None => {
                self.refining = None;
                None
            }
        }
    }

    pub(crate) fn effective_sessions(&self) -> Vec<ReviewSession> {
        if self.sessions.is_empty() {
            if let Some(ref s) = self.session {
                vec![s.clone()]
            } else {
                Vec::new()
            }
        } else {
            self.sessions.clone()
        }
    }

    pub fn clamp_selection(&mut self) {
        let count = self.row_count();
        if count == 0 {
            self.selected_index = 0;
        } else if self.selected_index >= count {
            self.selected_index = count - 1;
        }
    }

    /// Maximum valid viewport offset for the document last rendered.
    ///
    /// Paging and the wheel are free scrolling: they move the window without
    /// moving the cursor, so they need exactly the upper bound the renderer
    /// applies and must never re-anchor on the selected row (c-1). The bound is
    /// computed in `usize` and only then reduced to the `u16` viewport unit, so
    /// a document longer than 65535 lines saturates instead of wrapping around
    /// and trapping the viewport short of its end (c-2).
    pub fn max_scroll(&self) -> u16 {
        let max = self
            .rendered_lines
            .saturating_sub(self.view_height as usize);
        u16::try_from(max).unwrap_or(u16::MAX)
    }

    /// Scrolls the viewport just enough to reveal the selected row.
    ///
    /// This is the only place that moves the viewport because of the cursor:
    /// free scrolling (wheel, paging) belongs to the developer and is never
    /// re-anchored on a redraw (c-1). A row that fits on one screen is revealed
    /// in full from its top edge, and a row taller than the viewport keeps any
    /// position already resting inside it, so its lower lines stay reachable
    /// instead of being pinned to the first screenful (c-2).
    pub fn scroll_to_selected(&mut self) {
        let rows = self.flatten_rows();
        if rows.is_empty() || self.view_height == 0 {
            return;
        }
        let sel = self.selected_index.min(rows.len() - 1);
        self.selected_index = sel;

        let (lines, offsets) = compute_unified_review_lines(
            &rows,
            sel,
            self.content_width.max(10),
            self.theme_mode,
            &mut self.markdown_cache,
        );
        let height = self.view_height as usize;
        let start = offsets.get(sel).copied().unwrap_or(0);
        let end = offsets.get(sel + 1).copied().unwrap_or(lines.len());
        let scroll = self.scroll as usize;

        let new_scroll = if end.saturating_sub(start) <= height {
            // The row fits, so it is fully visible only while its first line
            // sits at or below the viewport top and its last line at or above
            // the viewport bottom. A row scrolled off above — what an upward
            // `p`, `Up`, `Home`, or `AccordionPrev` move produces — therefore
            // pulls the viewport up instead of staying invisible, and a row
            // going past the bottom is revealed from its top edge.
            if scroll <= start && scroll + height >= end {
                scroll
            } else {
                start
            }
        } else {
            // The row is taller than the viewport, so every offset in
            // `[start, end - height]` reveals a part of it. A viewport already
            // resting inside that range keeps its position, and one arriving
            // from above or below moves to the nearest edge instead of being
            // pinned to the row's first screenful (c-2).
            scroll.clamp(start, end.saturating_sub(height))
        };
        self.scroll = new_scroll as u16;
    }

    /// Positions the viewport around the selected review row using the three-beat
    /// Emacs `recenter-top-bottom` cycle (center -> top -> bottom).
    pub fn recenter_selected(&mut self) {
        let rows = self.flatten_rows();
        if rows.is_empty() || self.view_height == 0 {
            return;
        }
        let sel = self.selected_index.min(rows.len().saturating_sub(1));
        self.selected_index = sel;

        let (lines, offsets) = compute_unified_review_lines(
            &rows,
            sel,
            self.content_width.max(10),
            self.theme_mode,
            &mut self.markdown_cache,
        );
        let start = offsets.get(sel).copied().unwrap_or(0);
        let end = offsets.get(sel + 1).copied().unwrap_or(lines.len());
        self.scroll = self
            .recenter_state
            .recenter(sel, start, end, lines.len(), self.view_height);
    }

    /// Accordion-style navigation: moves to the next or previous finding,
    /// session, or subplan step, closing previous items and opening the new
    /// target item.
    pub fn accordion_navigate(&mut self, forward: bool) {
        #[derive(Clone)]
        struct ReviewNodeRef {
            session_id: String,
            section_id: Option<String>,
            item_id: Option<u32>,
            step_id: Option<String>,
            ancestor_steps: Vec<String>,
        }

        fn collect_subplan_nodes(
            session_id: &str,
            item_id: u32,
            steps: &[PlanItem],
            ancestors: &mut Vec<String>,
            out: &mut Vec<ReviewNodeRef>,
        ) {
            for step in steps {
                out.push(ReviewNodeRef {
                    session_id: session_id.to_string(),
                    section_id: None,
                    item_id: Some(item_id),
                    step_id: Some(step.id.clone()),
                    ancestor_steps: ancestors.clone(),
                });
                ancestors.push(step.id.clone());
                collect_subplan_nodes(session_id, item_id, &step.children, ancestors, out);
                ancestors.pop();
            }
        }

        let effective = self.effective_sessions();
        let mut all_nodes = Vec::new();
        for session in &effective {
            all_nodes.push(ReviewNodeRef {
                session_id: session.id.clone(),
                section_id: None,
                item_id: None,
                step_id: None,
                ancestor_steps: Vec::new(),
            });
            let groups = self
                .group_buckets(session)
                .into_iter()
                .map(|bucket| (bucket.id, bucket.items))
                .collect::<Vec<_>>();
            for (group_id, items) in groups {
                all_nodes.push(ReviewNodeRef {
                    session_id: session.id.clone(),
                    section_id: Some(group_id),
                    item_id: None,
                    step_id: None,
                    ancestor_steps: Vec::new(),
                });
                for issue in items {
                    all_nodes.push(ReviewNodeRef {
                        session_id: session.id.clone(),
                        section_id: None,
                        item_id: Some(issue.id),
                        step_id: None,
                        ancestor_steps: Vec::new(),
                    });
                    let mut ancestors = Vec::new();
                    collect_subplan_nodes(
                        &session.id,
                        issue.id,
                        &issue.subplan,
                        &mut ancestors,
                        &mut all_nodes,
                    );
                }
            }
        }

        if all_nodes.is_empty() {
            return;
        }

        let sel_row = self.selected_row();
        let cur_idx = match sel_row {
            Some(VisibleReviewRow::SessionHeader { session_id, .. })
            | Some(VisibleReviewRow::SessionSummary { session_id, .. }) => all_nodes
                .iter()
                .position(|n| {
                    n.session_id == session_id && n.section_id.is_none() && n.item_id.is_none()
                })
                .unwrap_or(0),
            Some(VisibleReviewRow::GroupHeader {
                session_id,
                group_id,
                ..
            }) => all_nodes
                .iter()
                .position(|n| {
                    n.session_id == session_id && n.section_id.as_deref() == Some(group_id.as_str())
                })
                .unwrap_or(0),
            Some(VisibleReviewRow::ReviewItem {
                session_id,
                item_id,
                ..
            }) => all_nodes
                .iter()
                .position(|n| {
                    n.session_id == session_id && n.item_id == Some(item_id) && n.step_id.is_none()
                })
                .unwrap_or(0),
            Some(VisibleReviewRow::SubplanItem {
                session_id,
                item_id,
                step_id,
                ..
            }) => all_nodes
                .iter()
                .position(|n| {
                    n.session_id == session_id
                        && n.item_id == Some(item_id)
                        && n.step_id.as_deref() == Some(step_id.as_str())
                })
                .unwrap_or(0),
            None => 0,
        };

        let target_idx = if forward {
            if cur_idx + 1 < all_nodes.len() {
                cur_idx + 1
            } else {
                return;
            }
        } else if cur_idx > 0 {
            cur_idx - 1
        } else {
            return;
        };

        let target = all_nodes[target_idx].clone();

        for session in &effective {
            if session.id == target.session_id
                && (target.item_id.is_some() || target.section_id.is_some())
            {
                self.collapsed_sessions.remove(&target.session_id);
            } else {
                self.collapsed_sessions.insert(session.id.clone());
            }
        }

        // Reveal the target path: its section and every container step in the
        // way must be unfolded, so the cursor can rest on the row it walked to.
        let session_prefix = format!("{}:g:", target.session_id);
        if let Some(section_id) = target.section_id.as_deref() {
            self.collapsed_groups
                .remove(&group_key(&target.session_id, section_id));
        } else if target.item_id.is_some() {
            self.collapsed_groups
                .retain(|key| !key.starts_with(&session_prefix));
        }

        self.expanded_items.clear();
        if let Some(item_id) = target.item_id {
            self.expanded_items
                .insert(format!("{}:{}", target.session_id, item_id));
            for ancestor in &target.ancestor_steps {
                self.expanded_items
                    .insert(subplan_key(&target.session_id, item_id, ancestor));
            }
            if let Some(step_id) = target.step_id.as_deref() {
                self.expanded_items
                    .insert(subplan_key(&target.session_id, item_id, step_id));
            }
        }

        let rows = self.flatten_rows();
        if let Some(pos) = rows.iter().position(|row| {
            if row.session_id() != target.session_id.as_str() {
                return false;
            }
            match (row, &target.section_id, target.item_id, &target.step_id) {
                (VisibleReviewRow::SessionHeader { .. }, None, None, None) => true,
                (VisibleReviewRow::SessionSummary { .. }, None, None, None) => true,
                (VisibleReviewRow::GroupHeader { group_id, .. }, Some(section), None, None) => {
                    group_id == section
                }
                (VisibleReviewRow::ReviewItem { item_id, .. }, None, Some(tid), None) => {
                    *item_id == tid
                }
                (
                    VisibleReviewRow::SubplanItem {
                        item_id, step_id, ..
                    },
                    None,
                    Some(tid),
                    Some(sid),
                ) => *item_id == tid && step_id == sid,
                _ => false,
            }
        }) {
            self.selected_index = pos;
        }

        self.clamp_selection();
        self.scroll_to_selected();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{
        IssueStatus, IssueType, ModelRef, PlanItemStatus, ReviewIssue, ReviewSession,
    };
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
            plan: vec![],
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

    #[test]
    fn test_clamp_selection_clamps_against_the_row_count() {
        let mut view = ReviewViewState {
            sessions: vec![make_session("rev-clamp", &[1, 2])],
            ..Default::default()
        };

        // A header, a group header, and two findings: a cursor past the last
        // row clamps to it.
        assert_eq!(view.row_count(), 4);
        view.selected_index = 9;
        view.clamp_selection();
        assert_eq!(view.selected_index, 3);

        // Collapsing the session leaves its header as the only row.
        view.toggle_session_expanded("rev-clamp");
        assert_eq!(view.row_count(), 1);
        view.selected_index = 2;
        view.clamp_selection();
        assert_eq!(view.selected_index, 0);
    }

    #[test]
    fn test_status_updates_are_scoped_by_review_id() {
        let mut view = ReviewViewState {
            sessions: vec![make_session("rev-1", &[1]), make_session("rev-2", &[1])],
            ..Default::default()
        };
        view.session = view.sessions.first().cloned();

        // Updating rev-2 must not touch the same-numbered finding in rev-1.
        view.set_item_status(Some("rev-2"), 1, IssueStatus::Fixed);
        assert_eq!(view.sessions[0].items[0].status, IssueStatus::Todo);
        assert_eq!(view.sessions[1].items[0].status, IssueStatus::Fixed);
        assert_eq!(
            view.session.as_ref().map(|s| s.items[0].status),
            Some(IssueStatus::Todo)
        );

        // Updating the active session keeps both copies in sync.
        view.set_item_status(Some("rev-1"), 1, IssueStatus::InProgress);
        assert_eq!(view.sessions[0].items[0].status, IssueStatus::InProgress);
        assert_eq!(
            view.session.as_ref().map(|s| s.items[0].status),
            Some(IssueStatus::InProgress)
        );
        assert_eq!(view.sessions[1].items[0].status, IssueStatus::Fixed);

        // apply_item is scoped the same way.
        let mut replacement = make_item(1);
        replacement.status = IssueStatus::Rejected;
        view.apply_item(Some("rev-2"), replacement);
        assert_eq!(view.sessions[0].items[0].status, IssueStatus::InProgress);
        assert_eq!(view.sessions[1].items[0].status, IssueStatus::Rejected);
    }

    #[test]
    fn test_fold_all_is_a_reversible_binary_toggle() {
        let mut view = ReviewViewState {
            sessions: vec![make_session("rev-1", &[1, 2]), make_session("rev-2", &[3])],
            ..Default::default()
        };

        // Default state: sessions expanded, findings collapsed. This is neither
        // fully expanded nor fully collapsed, so the first press expands all.
        assert!(!view.is_all_expanded());

        // First press: expand every session and finding.
        view.toggle_fold_all();
        assert!(view.is_all_expanded());
        assert!(view.is_item_expanded("rev-1", 1));
        assert!(view.is_item_expanded("rev-1", 2));
        assert!(view.is_item_expanded("rev-2", 3));
        assert!(view.is_session_expanded("rev-1"));
        assert!(view.is_session_expanded("rev-2"));

        // Second press: collapse everything back into the dense overview.
        view.toggle_fold_all();
        assert!(!view.is_all_expanded());
        assert!(!view.is_item_expanded("rev-1", 1));
        assert!(!view.is_session_expanded("rev-1"));
        assert!(!view.is_session_expanded("rev-2"));

        // Third press: the cycle is reversible and returns to fully expanded.
        view.toggle_fold_all();
        assert!(view.is_all_expanded());
        assert!(view.is_item_expanded("rev-1", 1));
        assert!(view.is_session_expanded("rev-2"));
    }

    #[test]
    fn test_scroll_to_selected_keeps_free_scroll_inside_a_tall_row() {
        let long_description = (0..120)
            .map(|line| format!("Detail line {line}."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut session = make_session("rev-tall", &[1, 2]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            description: long_description,
            ..make_item(1)
        });
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.view_height = 10;
        view.content_width = 80;
        view.toggle_item_expanded("rev-tall", 1);

        // The first row is revealed normally: the unconditional reset that used
        // to exist for row zero is gone, but a row at the top needs no scroll.
        view.selected_index = 0;
        view.scroll_to_selected();
        assert_eq!(view.scroll, 0);

        // Moving down into the tall finding reveals its top edge.
        view.selected_index = 2;
        view.scroll_to_selected();
        let start = view.scroll as usize;
        assert!(start > 0, "the tall finding must be scrolled into view");

        let rows = view.flatten_rows();
        let (lines, offsets) =
            compute_unified_review_lines(&rows, 2, 80, view.theme_mode, &mut view.markdown_cache);
        let end = offsets.get(3).copied().unwrap_or(lines.len());
        let height = view.view_height as usize;
        let max_start = end - height;
        assert!(
            max_start > start + 20,
            "the finding must be taller than the viewport: {start}..{end}"
        );

        // A viewport already resting inside the tall row keeps its position
        // instead of snapping back to the first screenful (c-2).
        view.scroll = (start + 20) as u16;
        view.scroll_to_selected();
        assert_eq!(view.scroll as usize, start + 20);

        // Arriving from below reveals the bottom of the same row.
        view.selected_index = 3;
        view.scroll_to_selected();
        assert_eq!(
            view.scroll as usize, end,
            "the next row must be revealed at the top of the viewport"
        );

        view.selected_index = 2;
        view.scroll_to_selected();
        assert_eq!(
            view.scroll as usize, max_start,
            "moving up into a tall row must reveal its bottom edge"
        );
    }

    fn subplan_step(id: &str, title: &str, status: PlanItemStatus) -> PlanItem {
        PlanItem {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..Default::default()
        }
    }

    #[test]
    fn test_accordion_navigation_walks_subplan_steps() {
        let mut session = make_session("rev-acc", &[1]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            plan: vec![
                PlanItem {
                    children: vec![subplan_step("1.1", "Leaf one", PlanItemStatus::Todo)],
                    ..subplan_step("1", "Container", PlanItemStatus::Todo)
                },
                subplan_step("2", "Second top step", PlanItemStatus::Todo),
            ],
            ..make_item(1)
        });
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        // Row 0 is the session header, row 1 is the default group header, and
        // row 2 is the finding.
        view.selected_index = 2;

        view.accordion_navigate(true);
        assert!(matches!(
            view.selected_row(),
            Some(VisibleReviewRow::SubplanItem { ref step_id, .. }) if step_id == "1"
        ));

        view.accordion_navigate(true);
        assert!(matches!(
            view.selected_row(),
            Some(VisibleReviewRow::SubplanItem { ref step_id, .. }) if step_id == "1.1"
        ));

        view.accordion_navigate(true);
        assert!(matches!(
            view.selected_row(),
            Some(VisibleReviewRow::SubplanItem { ref step_id, .. }) if step_id == "2"
        ));

        // Backward navigation crosses the folded container into its last leaf.
        view.accordion_navigate(false);
        assert!(matches!(
            view.selected_row(),
            Some(VisibleReviewRow::SubplanItem { ref step_id, .. }) if step_id == "1.1"
        ));
    }

    #[test]
    fn test_max_scroll_bounds_the_viewport_without_wrapping() {
        let mut view = ReviewViewState {
            rendered_lines: 500,
            view_height: 20,
            ..Default::default()
        };
        assert_eq!(
            view.max_scroll(),
            480,
            "the bound is the last full screenful of the document"
        );

        // A document shorter than the viewport has no scroll range at all.
        view.rendered_lines = 10;
        assert_eq!(view.max_scroll(), 0);

        // A document longer than the u16 viewport unit saturates at the largest
        // representable offset instead of wrapping around and trapping the
        // viewport before its end (c-2).
        view.rendered_lines = 70_000;
        view.view_height = 10;
        assert_eq!(view.max_scroll(), u16::MAX);
    }

    /// Expected `Center`, `Top`, and `Bottom` viewport offsets for a row that
    /// spans `start..end` inside a `height`-row viewport (Conventions §5.2).
    fn recenter_beats(start: usize, end: usize, total: usize, height: u16) -> [u16; 3] {
        let height = height as usize;
        let max_scroll = total.saturating_sub(height);
        let clamp = |value: usize| u16::try_from(value.min(max_scroll)).unwrap_or(u16::MAX);
        [
            clamp(
                start
                    .saturating_add(end.saturating_sub(start) / 2)
                    .saturating_sub(height / 2),
            ),
            clamp(start),
            clamp(end.saturating_sub(height)),
        ]
    }

    /// Expected beats for the row at `index`, derived from the same layout
    /// offsets the view uses to position its viewport.
    fn review_recenter_beats(view: &ReviewViewState, index: usize) -> [u16; 3] {
        let rows = view.flatten_rows();
        // A scratch cache is enough here: the beats depend on the rendered line
        // count, never on cache reuse.
        let mut cache = MarkdownBodyCache::new();
        let (lines, offsets) = compute_unified_review_lines(
            &rows,
            index,
            view.content_width.max(10),
            view.theme_mode,
            &mut cache,
        );
        let start = offsets.get(index).copied().unwrap_or(0);
        let end = offsets.get(index + 1).copied().unwrap_or(lines.len());
        recenter_beats(start, end, lines.len(), view.view_height)
    }

    fn recenter_review_fixture(selected: usize) -> ReviewViewState {
        ReviewViewState {
            sessions: (1..=12)
                .map(|i| make_session(&format!("rev-{i}"), &[i as u32]))
                .collect(),
            selected_index: selected,
            view_height: 8,
            content_width: 80,
            ..Default::default()
        }
    }

    #[test]
    fn test_recenter_selected_walks_the_center_top_bottom_cycle() {
        let mut view = recenter_review_fixture(16);

        let [center, top, bottom] = review_recenter_beats(&view, view.selected_index);
        assert_ne!(center, top, "the center and top beats must differ");
        assert_ne!(top, bottom, "the top and bottom beats must differ");
        assert_ne!(center, bottom, "the center and bottom beats must differ");

        view.recenter_selected();
        assert_eq!(view.scroll, center, "the first press centers the row");
        view.recenter_selected();
        assert_eq!(
            view.scroll, top,
            "the second press aligns the row to the top"
        );
        view.recenter_selected();
        assert_eq!(
            view.scroll, bottom,
            "the third press aligns the row to the bottom"
        );
        view.recenter_selected();
        assert_eq!(
            view.scroll, center,
            "the fourth press wraps back to the center"
        );
    }

    #[test]
    fn test_recenter_selected_restarts_the_cycle_on_cursor_move() {
        let mut view = recenter_review_fixture(16);

        // Walk to the top beat of the first row, so a continuation would land on
        // the bottom beat instead of starting over.
        view.recenter_selected();
        view.recenter_selected();

        view.selected_index = 6;
        let [center, top, bottom] = review_recenter_beats(&view, view.selected_index);
        assert_ne!(center, bottom, "the first and third beats must differ");

        view.recenter_selected();
        assert_eq!(
            view.scroll, center,
            "a new cursor position starts over on the center beat"
        );
        view.recenter_selected();
        assert_eq!(view.scroll, top, "the new row keeps its own cycle");
        view.recenter_selected();
        assert_eq!(view.scroll, bottom);
    }

    /// The recenter command is declared on both the abstract primary chord and
    /// the raw `Ctrl+L` chord, and no other binding claims the same key event
    /// under either modifier configuration (c-2).
    #[test]
    fn test_recenter_chord_is_declared_and_unambiguous() {
        use super::input::ReviewAction;
        use crate::config::{InputConfig, MetaModifier, PrimaryModifier};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        // The abstract `C-l` chord and the raw `Ctrl+L` chord deliberately name
        // the same intent, so under the default primary assignment both of them
        // match a single key event. The resolved intents are deduplicated, since
        // the invariant under test is which action the key reaches, not how many
        // spellings declare it (c-2).
        fn claims(
            input: &InputConfig,
            code: KeyCode,
            modifiers: KeyModifiers,
        ) -> Vec<ReviewAction> {
            let mut actions = Vec::new();
            for spec in REVIEW_COMMAND_MAP.specs() {
                let matched = spec
                    .chord
                    .matches(KeyEvent::new(code, modifiers), input, false);
                if matched && !actions.contains(&spec.action) {
                    actions.push(spec.action);
                }
            }
            actions
        }

        let ctrl_primary = InputConfig {
            primary_modifier: PrimaryModifier::Ctrl,
            meta_modifier: MetaModifier::Alt,
            ..InputConfig::default()
        };
        let alt_primary = InputConfig {
            primary_modifier: PrimaryModifier::Alt,
            meta_modifier: MetaModifier::Ctrl,
            ..InputConfig::default()
        };

        assert_eq!(
            claims(&ctrl_primary, KeyCode::Char('l'), KeyModifiers::CONTROL),
            vec![ReviewAction::Recenter],
            "C-l must recenter under the default primary modifier"
        );
        assert_eq!(
            claims(&alt_primary, KeyCode::Char('l'), KeyModifiers::ALT),
            vec![ReviewAction::Recenter],
            "the abstract C-l chord must follow a remapped primary modifier"
        );
        assert_eq!(
            claims(&alt_primary, KeyCode::Char('l'), KeyModifiers::CONTROL),
            vec![ReviewAction::Recenter],
            "a raw Ctrl+L must still recenter when the primary modifier is Alt"
        );
    }

    /// An expanded subplan step taller than the viewport must be readable down
    /// to its last line: selecting it reveals its top edge, and free scrolling
    /// then reaches the bottom without snapping back to the first screenful
    /// (c-2).
    #[test]
    fn test_tall_expanded_subplan_step_scrolls_to_its_final_line() {
        let long_details = (0..80)
            .map(|line| format!("Invariant line {line}."))
            .collect::<Vec<_>>()
            .join("\n");
        let mut session = make_session("rev-tall-step", &[1]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            plan: vec![PlanItem {
                details: Some(long_details),
                ..subplan_step("1.1", "Tall step", PlanItemStatus::Todo)
            }],
            ..make_item(1)
        });
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.view_height = 20;
        view.content_width = 80;
        view.toggle_item_expanded("rev-tall-step", 1);
        view.toggle_subplan_item_expanded("rev-tall-step", 1, "1.1");

        let rows = view.flatten_rows();
        let step_row = rows
            .iter()
            .position(|row| {
                matches!(
                    row,
                    VisibleReviewRow::SubplanItem { step_id, .. } if step_id == "1.1"
                )
            })
            .expect("the tall step row must be visible once expanded");
        let (lines, offsets) = compute_unified_review_lines(
            &rows,
            usize::MAX,
            80,
            view.theme_mode,
            &mut view.markdown_cache,
        );
        view.rendered_lines = lines.len();

        let start = offsets[step_row];
        let end = offsets.get(step_row + 1).copied().unwrap_or(lines.len());
        let height = view.view_height as usize;
        assert!(
            end.saturating_sub(start) > height,
            "the step must be taller than the viewport: {start}..{end}"
        );

        // Arriving from above reveals the top edge of the tall step.
        view.scroll = 0;
        view.selected_index = step_row;
        view.scroll_to_selected();
        assert_eq!(view.scroll as usize, start);

        // Free scrolling walks down to the last full screenful, where the final
        // invariant line is on screen, and never snaps back to the top edge.
        let max = view.max_scroll() as usize;
        assert_eq!(max, lines.len() - height);
        assert!(
            max > start,
            "the viewport must be able to leave the step's top edge"
        );
        view.scroll = u16::try_from(max).expect("the viewport offset fits in u16");
        view.scroll_to_selected();
        assert_eq!(
            view.scroll as usize, max,
            "free scrolling down must never snap back to the step's first screenful"
        );
        let last = lines
            .last()
            .map(|line| line.to_string())
            .unwrap_or_default();
        assert!(
            last.contains("Invariant line 79."),
            "the last line of the tall step must be reachable: {last}"
        );
    }

    /// Selecting an earlier row (`p`, `Up`, `Home`, `AccordionPrev`) must pull
    /// the viewport up to it. A row left above the viewport top is invisible
    /// even though the cursor rests on it (c-scroll-visibility).
    #[test]
    fn test_scroll_to_selected_pulls_the_viewport_up_for_an_earlier_row() {
        let item_ids: Vec<u32> = (1..=60).collect();
        let mut view = ReviewViewState {
            sessions: vec![make_session("rev-up", &item_ids)],
            view_height: 10,
            content_width: 80,
            ..Default::default()
        };

        let last = view.row_count() - 1;
        view.selected_index = last;
        view.scroll_to_selected();
        assert!(
            view.scroll > 0,
            "the last row must push the viewport away from the top"
        );

        // Moving up to the middle of the list must scroll the viewport up with
        // the cursor instead of leaving the row above its top edge.
        let target = last / 2;
        view.selected_index = target;
        view.scroll_to_selected();
        assert_selected_row_visible(&mut view, target);

        // `Home` must reach the very first row the same way.
        view.selected_index = 0;
        view.scroll_to_selected();
        assert_eq!(view.scroll, 0, "the first row must reveal the document top");
        assert_selected_row_visible(&mut view, 0);
    }

    /// Asserts that the row at `index` lies entirely inside the viewport.
    fn assert_selected_row_visible(view: &mut ReviewViewState, index: usize) {
        let rows = view.flatten_rows();
        let (lines, offsets) = compute_unified_review_lines(
            &rows,
            index,
            view.content_width.max(10),
            view.theme_mode,
            &mut view.markdown_cache,
        );
        let start = offsets.get(index).copied().unwrap_or(0);
        let end = offsets.get(index + 1).copied().unwrap_or(lines.len());
        let scroll = view.scroll as usize;
        let height = view.view_height as usize;
        assert!(
            scroll <= start && scroll + height >= end,
            "the selected row {start}..{end} must be fully visible in {scroll}..{}",
            scroll + height
        );
    }

    /// Selecting a row below the viewport (`n`, `Down`, `End`) must scroll the
    /// viewport down to it, and selecting the first row (`Home`) must reveal the
    /// very top of the document (c-scroll-visibility).
    #[test]
    fn test_scroll_to_selected_reveals_a_row_below_and_the_document_top() {
        let item_ids: Vec<u32> = (1..=60).collect();
        let mut view = ReviewViewState {
            sessions: vec![make_session("rev-nav", &item_ids)],
            view_height: 10,
            content_width: 80,
            ..Default::default()
        };

        // `End`: the last row pushes the viewport away from the top and stays
        // inside it.
        let last = view.row_count() - 1;
        view.selected_index = last;
        view.scroll_to_selected();
        assert!(
            view.scroll > 0,
            "the last row must scroll the viewport down"
        );
        assert_selected_row_visible(&mut view, last);

        // `n` / `Down`: a row in the middle is revealed instead of staying
        // above the viewport top.
        let middle = last / 2;
        view.selected_index = middle;
        view.scroll_to_selected();
        assert_selected_row_visible(&mut view, middle);

        // `Home`: the first row reveals the document top.
        view.selected_index = 0;
        view.scroll_to_selected();
        assert_eq!(view.scroll, 0, "the first row must reveal the document top");
        assert_selected_row_visible(&mut view, 0);
    }

    #[test]
    fn test_group_telemetry_tracks_the_supervisors_sequential_walk() {
        let mut view = ReviewViewState::default();

        view.begin_group("grp-alpha".to_string(), "Alpha".to_string());
        assert!(view.running_group.is_some());
        view.finish_group();
        assert!(view.running_group.is_none());
        assert_eq!(view.finished_groups, 1);

        view.begin_group("grp-beta".to_string(), "Beta".to_string());
        view.finish_group();
        assert_eq!(view.finished_groups, 2);

        // The reset returns the run to a clean slate.
        view.begin_group("grp-gamma".to_string(), "Gamma".to_string());
        view.analyzing = true;
        view.reset_run_telemetry();
        assert!(view.running_group.is_none());
        assert_eq!(view.finished_groups, 0);
        assert!(!view.analyzing);
    }

    #[test]
    fn test_running_group_total_requires_the_running_groups_session() {
        let mut view = ReviewViewState::default();

        // Without a running group there is no total to name.
        assert_eq!(view.running_group_total(), None);

        // Without a mirrored session the badge cannot know the total.
        view.begin_group("grp-beta".to_string(), "Beta".to_string());
        assert_eq!(view.running_group_total(), None);

        // A session whose groups hold the running id names the total.
        let grouped = ReviewSession {
            id: "rev-grouped".to_string(),
            groups: vec![
                tauqe_protocol::ReviewGroup {
                    id: "grp-alpha".to_string(),
                    title: "Alpha".to_string(),
                    paths: Vec::new(),
                    prompt: None,
                    ..Default::default()
                },
                tauqe_protocol::ReviewGroup {
                    id: "grp-beta".to_string(),
                    title: "Beta".to_string(),
                    paths: Vec::new(),
                    prompt: None,
                    ..Default::default()
                },
            ],
            ..make_session("rev-grouped", &[])
        };
        view.session = Some(grouped);
        assert_eq!(view.running_group_total(), Some(2));

        // A mirrored session from another run never invents a total.
        let foreign = ReviewSession {
            id: "rev-foreign".to_string(),
            groups: vec![tauqe_protocol::ReviewGroup {
                id: "grp-legacy".to_string(),
                title: "Legacy".to_string(),
                paths: Vec::new(),
                prompt: None,
                ..Default::default()
            }],
            ..make_session("rev-foreign", &[1])
        };
        view.session = Some(foreign);
        assert_eq!(view.running_group_total(), None);
    }
}
