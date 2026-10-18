use std::collections::{HashSet, VecDeque};

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use tauqe_protocol::{
    DiscussionFocus, DiscussionTarget, IssueStatus, IssueType, ModelEntry, ModelRef,
    ModelSelection, PlanItem, PlanItemStatus, ReviewIssue, ReviewItem, ReviewSession,
};

use crate::components::model_selector::ModelSelector;
use crate::theme::engine::current_engine;
use crate::ui::wrap_lines;

pub mod input;
pub mod render;

pub use self::input::{handle_review_key, REVIEW_COMMAND_MAP};
pub use self::render::render_review_view;

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
        /// Embedded step plan decomposed by the Architect, rendered on expansion.
        plan: Vec<PlanItem>,
        is_expanded: bool,
        /// Whether the active discussion focus anchors on this finding.
        focused: bool,
    },
    /// One interactive row of an expanded finding's embedded subplan.
    ///
    /// Subplan steps are full tree nodes, so the cursor can rest on a single
    /// step, fold its details and files, and execute the owning finding from it
    /// (Conventions §4.4).
    SubplanItem {
        session_id: String,
        /// Numeric id of the owning finding inside the session.
        item_id: u32,
        step_id: String,
        title: String,
        details: Option<String>,
        status: PlanItemStatus,
        files: Vec<String>,
        readonly_files: Vec<String>,
        depends_on: Vec<String>,
        depth: usize,
        is_expandable: bool,
        is_expanded: bool,
    },
}

impl VisibleReviewRow {
    pub fn session_id(&self) -> &str {
        match self {
            Self::SessionHeader { session_id, .. } => session_id,
            Self::SessionSummary { session_id, .. } => session_id,
            Self::ReviewItem { session_id, .. } => session_id,
            Self::SubplanItem { session_id, .. } => session_id,
        }
    }
}

/// A finding queued for Architect decomposition.
///
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

/*
#[derive(Debug, Clone, Default)]
pub struct ReviewViewState {</search_6DFEFD4296DDA92B>
</edit>
<edit path="crates/tui/src/views/review.rs">
<search>    /// Findings awaiting Architect decomposition, in dispatch order.
    ///
    /// The Architect decomposes one finding per bounded call, so a batch
    /// refinement drains this queue one `review/refineItem` response at a time.
    pub refine_queue: VecDeque<RefineTarget>,
}</search>
<replace>    /// Findings awaiting Architect decomposition, in dispatch order.
    ///
    /// The Architect decomposes one finding per bounded call, so a batch
    /// refinement drains this queue one `review/refineItem` response at a time.
    pub refine_queue: VecDeque<RefineTarget>,
    /// Architect decomposition dialog for the selected finding, when open.
    pub refine_dialog: Option<ReviewRefineDialogState>,
}
    pub running: bool,
    /// Review id currently under Architect decomposition (`review/refineItem`),
    /// if any. Drives the in-flight footer indicator and blocks re-entry.
    pub refining: Option<String>,
    pub sessions: Vec<ReviewSession>,
    pub session: Option<ReviewSession>,
    pub selected_index: usize,
    pub scroll: u16,
    pub collapsed_sessions: HashSet<String>,
    pub collapsed_summaries: HashSet<String>,
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
}

*/

#[derive(Debug, Clone, Default)]
pub struct ReviewViewState {
    pub running: bool,
    /// Review id currently under Architect decomposition (`review/refineItem`),
    /// if any. Drives the in-flight footer indicator and blocks re-entry.
    pub refining: Option<String>,
    pub sessions: Vec<ReviewSession>,
    pub session: Option<ReviewSession>,
    pub selected_index: usize,
    pub scroll: u16,
    pub collapsed_sessions: HashSet<String>,
    pub collapsed_summaries: HashSet<String>,
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
}

impl ReviewViewState {
    pub fn begin(&mut self) {
        self.running = true;
        self.error = None;
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
    pub fn is_subplan_item_expanded(
        &self,
        session_id: &str,
        item_id: u32,
        step_id: &str,
    ) -> bool {
        self.expanded_items
            .contains(&subplan_key(session_id, item_id, step_id))
    }

    pub fn toggle_subplan_item_expanded(
        &mut self,
        session_id: &str,
        item_id: u32,
        step_id: &str,
    ) {
        let key = subplan_key(session_id, item_id, step_id);
        if !self.expanded_items.remove(&key) {
            self.expanded_items.insert(key);
        }
    }

    pub fn expand_all(&mut self) {
        self.collapsed_sessions.clear();
        self.collapsed_summaries.clear();
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
        for s in &effective {
            self.collapsed_sessions.insert(s.id.clone());
        }
    }

    /// True only when every session and every finding is expanded.
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
        effective.iter().all(|s| {
            self.is_summary_expanded(&s.id)
                && s.issues().iter().all(|issue| {
                    self.expanded_items
                        .contains(&format!("{}:{}", s.id, issue.id))
                        && subplan_all_expanded(
                            &self.expanded_items,
                            &s.id,
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
        let effective = self.effective_sessions();

        for session in &effective {
            let is_expanded = self.is_session_expanded(&session.id);
            let total = session.items.len();
            let fixed = session
                .items
                .iter()
                .filter(|i| i.status == IssueStatus::Fixed)
                .count();
            let todo = session
                .items
                .iter()
                .filter(|i| i.status == IssueStatus::Todo)
                .count();
            let in_progress = session
                .items
                .iter()
                .filter(|i| i.status == IssueStatus::InProgress)
                .count();
            let discussion = session
                .items
                .iter()
                .filter(|i| i.status == IssueStatus::Discussion)
                .count();
            let skipped = session
                .items
                .iter()
                .filter(|i| i.status == IssueStatus::Skipped)
                .count();
            let rejected = session
                .items
                .iter()
                .filter(|i| i.status == IssueStatus::Rejected)
                .count();

            rows.push(VisibleReviewRow::SessionHeader {
                session_id: session.id.clone(),
                title: if session.title.trim().is_empty() {
                    session.id.clone()
                } else {
                    session.title.clone()
                },
                description: session.description.clone(),
                created_at: session.created_at,
                model: session.model.clone(),
                total,
                fixed,
                todo,
                in_progress,
                discussion,
                skipped,
                rejected,
                is_expanded,
                focused: self.is_session_focused(&session.id),
            });

            if is_expanded {
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
                // Findings are stored as durable `ReviewItem` values and are
                // projected onto the richer `ReviewIssue` view vocabulary, so
                // the row exposes the classified problem type, the split
                // description/recommendations, the protected readonly
                // contracts, and the embedded plan through one explicit field
                // mapping.
                for issue in session.issues() {
                    if self.hide_closed
                        && matches!(
                            issue.status,
                            IssueStatus::Fixed | IssueStatus::Skipped | IssueStatus::Rejected
                        )
                    {
                        continue;
                    }
                    let item_expanded = self.is_item_expanded(&session.id, issue.id);
                    let focused = self.is_item_focused(&session.id, issue.id);
                    rows.push(VisibleReviewRow::ReviewItem {
                        session_id: session.id.clone(),
                        item_id: issue.id,
                        title: issue.title,
                        issue_type: issue.issue_type,
                        status: issue.status,
                        model: issue.model,
                        files: issue.files,
                        readonly_files: issue.readonly_files,
                        description: issue.description,
                        recommendations: issue.recommendations,
                        note: issue.note,
                        plan: issue.plan.clone(),
                        is_expanded: item_expanded,
                        focused,
                    });

                    // An expanded finding exposes its decomposed steps as
                    // first-class interactive rows, so the cursor can rest on a
                    // single step and fold its own details (Conventions §4.4).
                    if item_expanded {
                        self.flatten_subplan_rows(
                            &session.id,
                            issue.id,
                            &issue.plan,
                            0,
                            &mut rows,
                        );
                    }
                }
            }
        }
        rows
    }

    /// Flattens an embedded subplan into interactive step rows.
    ///
    /// Top-level steps always follow an expanded finding; nested steps appear
    /// only after their container step is expanded, so the tree stays dense
    /// until the developer drills into a step.
    fn flatten_subplan_rows(
        &self,
        session_id: &str,
        item_id: u32,
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
            let is_expanded = self.is_subplan_item_expanded(session_id, item_id, &step.id);

            out.push(VisibleReviewRow::SubplanItem {
                session_id: session_id.to_string(),
                item_id,
                step_id: step.id.clone(),
                title: step.title.clone(),
                details: step.details.clone(),
                status: step.status,
                files: step.files.clone(),
                readonly_files: step.readonly_files.clone(),
                depends_on: step.depends_on.clone(),
                depth,
                is_expandable: has_children || has_details || has_meta,
                is_expanded,
            });

            if has_children && is_expanded {
                self.flatten_subplan_rows(session_id, item_id, &step.children, depth + 1, out);
            }
        }
    }

    pub fn selected_row(&self) -> Option<VisibleReviewRow> {
        let rows = self.flatten_rows();
        rows.get(self.selected_index).cloned()
    }

    pub fn clamp_selection(&mut self) {
        let count = self.flatten_rows().len();
        if count == 0 {
            self.selected_index = 0;
        } else if self.selected_index >= count {
            self.selected_index = count - 1;
        }
    }

    pub fn scroll_to_selected(&mut self) {
        let rows = self.flatten_rows();
        if rows.is_empty() || self.view_height == 0 {
            return;
        }
        let sel = self.selected_index.min(rows.len() - 1);
        self.selected_index = sel;

        let (lines, offsets) =
            compute_unified_review_lines(&rows, sel, self.content_width.max(10), self.theme_mode);
        let height = self.view_height as usize;
        let start = offsets.get(sel).copied().unwrap_or(0);
        let end = offsets.get(sel + 1).copied().unwrap_or(lines.len());
        let scroll = self.scroll as usize;

        let new_scroll = if sel == 0 {
            0
        } else if start < scroll {
            start
        } else if end > scroll + height {
            end.saturating_sub(height).min(start)
        } else {
            scroll
        };
        self.scroll = new_scroll as u16;
    }

    /// Accordion-style navigation: moves to the next or previous finding,
    /// session, or subplan step, closing previous items and opening the new
    /// target item.
    pub fn accordion_navigate(&mut self, forward: bool) {
        #[derive(Clone)]
        struct ReviewNodeRef {
            session_id: String,
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
                item_id: None,
                step_id: None,
                ancestor_steps: Vec::new(),
            });
            for issue in session.issues() {
                if self.hide_closed
                    && (issue.status == IssueStatus::Fixed
                        || issue.status == IssueStatus::Skipped
                        || issue.status == IssueStatus::Rejected)
                {
                    continue;
                }
                all_nodes.push(ReviewNodeRef {
                    session_id: session.id.clone(),
                    item_id: Some(issue.id),
                    step_id: None,
                    ancestor_steps: Vec::new(),
                });
                let mut ancestors = Vec::new();
                collect_subplan_nodes(
                    &session.id,
                    issue.id,
                    &issue.plan,
                    &mut ancestors,
                    &mut all_nodes,
                );
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
                .position(|n| n.session_id == session_id && n.item_id.is_none())
                .unwrap_or(0),
            Some(VisibleReviewRow::ReviewItem { session_id, item_id, .. }) => all_nodes
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
            if session.id == target.session_id && target.item_id.is_some() {
                self.collapsed_sessions.remove(&target.session_id);
            } else {
                self.collapsed_sessions.insert(session.id.clone());
            }
        }

        self.expanded_items.clear();
        if let Some(item_id) = target.item_id {
            self.expanded_items
                .insert(format!("{}:{}", target.session_id, item_id));
            for ancestor in &target.ancestor_steps {
                self.expanded_items.insert(subplan_key(
                    &target.session_id,
                    item_id,
                    ancestor,
                ));
            }
            if let Some(step_id) = target.step_id.as_deref() {
                self.expanded_items.insert(subplan_key(
                    &target.session_id,
                    item_id,
                    step_id,
                ));
            }
        }

        let rows = self.flatten_rows();
        if let Some(pos) = rows
            .iter()
            .position(|r| match (r, target.item_id, &target.step_id) {
                (VisibleReviewRow::SessionHeader { session_id, .. }, None, None) => {
                    session_id == &target.session_id
                }
                (VisibleReviewRow::SessionSummary { session_id, .. }, None, None) => {
                    session_id == &target.session_id
                }
                (VisibleReviewRow::ReviewItem { session_id, item_id, .. }, Some(tid), None) => {
                    session_id == &target.session_id && *item_id == tid
                }
                (
                    VisibleReviewRow::SubplanItem {
                        session_id,
                        item_id,
                        step_id,
                        ..
                    },
                    Some(tid),
                    Some(sid),
                ) => session_id == &target.session_id && *item_id == tid && step_id == sid,
                _ => false,
            })
        {
            self.selected_index = pos;
        }

        self.clamp_selection();
        self.scroll_to_selected();
    }
}

/// Expansion key of one subplan step inside a finding's embedded plan.
///
/// The `s:` segment keeps step keys disjoint from the `session:item` keys that
/// mark an expanded finding, so the two expansion scopes never collide.
fn subplan_key(session_id: &str, item_id: u32, step_id: &str) -> String {
    format!("{}:{}:s:{}", session_id, item_id, step_id)
}

/// Records every subplan step of a finding as expanded.
fn collect_subplan_keys(
    session_id: &str,
    item_id: u32,
    steps: &[PlanItem],
    out: &mut HashSet<String>,
) {
    for step in steps {
        out.insert(subplan_key(session_id, item_id, &step.id));
        collect_subplan_keys(session_id, item_id, &step.children, out);
    }
}

/// Whether every subplan step of a finding is expanded.
fn subplan_all_expanded(
    expanded: &HashSet<String>,
    session_id: &str,
    item_id: u32,
    steps: &[PlanItem],
) -> bool {
    steps.iter().all(|step| {
        expanded.contains(&subplan_key(session_id, item_id, &step.id))
            && subplan_all_expanded(expanded, session_id, item_id, &step.children)
    })
}

/// Short badge text for a classified problem type.
///
/// The `quality` category maps to the domain `Style` variant, so it renders as
/// `[QUAL]` rather than the raw enum name.
fn issue_type_badge(issue_type: IssueType) -> &'static str {
    match issue_type {
        IssueType::Bug => "BUG",
        IssueType::Security => "SEC",
        IssueType::Architecture => "ARCH",
        IssueType::Performance => "PERF",
        IssueType::Quality => "QUAL",
    }
}

/// Emphasized color for a finding's problem type badge.
fn issue_type_style(issue_type: IssueType) -> Style {
    let engine = current_engine();
    let palette = engine.palette();
    match issue_type {
        IssueType::Bug | IssueType::Security => engine.fg(palette.status_error).bold(),
        IssueType::Architecture | IssueType::Performance => {
            engine.fg(palette.status_warning).bold()
        }
        IssueType::Quality => engine.fg(palette.status_info).bold(),
    }
}

/// Filled badge style for a finding's lifecycle status, including `Refine`.
fn issue_status_style(status: IssueStatus) -> Style {
    let engine = current_engine();
    let palette = engine.palette();
    let inverse = engine.color(palette.text_inverse);
    match status {
        IssueStatus::Discussion => engine.bg(palette.button_active_bg).fg(inverse).bold(),
        IssueStatus::Refine => engine.bg(palette.status_error).fg(inverse).bold(),
        IssueStatus::Todo => engine.bg(palette.status_warning).fg(inverse).bold(),
        IssueStatus::InProgress => engine.bg(palette.status_info).fg(inverse).bold(),
        IssueStatus::Fixed => engine.bg(palette.status_success).fg(inverse).bold(),
        IssueStatus::Skipped | IssueStatus::Rejected => {
            engine.bg(palette.text_muted).fg(inverse).bold()
        }
    }
}

/// Formats a Unix timestamp (seconds) as a compact UTC calendar instant
/// (`YYYY-MM-DD HH:MM UTC`) for the session header.
fn format_created_at(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rem = seconds % 86_400;
    let (hour, minute) = (rem / 3_600, (rem % 3_600) / 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 {
        yoe + era * 400 + 1
    } else {
        yoe + era * 400
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        year, month, day, hour, minute
    )
}

/// Compact indicator marking the row anchored by the active discussion focus.
fn focus_badge() -> Span<'static> {
    let engine = current_engine();
    let palette = engine.palette();
    Span::styled(
        " [FOCUS]",
        engine
            .bg(palette.status_success)
            .fg(engine.color(palette.text_inverse))
            .bold(),
    )
}

/// Status badge style for a subplan step, mirroring the Plans view vocabulary so
/// a decomposed finding reads exactly like an implementation step.
fn plan_status_style(status: PlanItemStatus) -> Style {
    let engine = current_engine();
    let palette = engine.palette();
    let inverse = engine.color(palette.text_inverse);
    match status {
        PlanItemStatus::Discussion => engine.bg(palette.button_active_bg).fg(inverse).bold(),
        PlanItemStatus::Todo => engine.bg(palette.status_warning).fg(inverse).bold(),
        PlanItemStatus::InProgress => engine.bg(palette.status_info).fg(inverse).bold(),
        PlanItemStatus::Done => engine.bg(palette.status_success).fg(inverse).bold(),
        PlanItemStatus::Blocked => engine.bg(palette.status_error).fg(inverse).bold(),
        PlanItemStatus::Cancelled => engine.bg(palette.text_muted).fg(inverse).bold(),
    }
}

/// Counts executable leaves in an embedded subplan as `(completed, total)`.
///
/// Mirrors `ReviewItem::subplan_stats`: only leaves carry an explicit status,
/// cancelled leaves are excluded from the total, and `completed` counts the
/// done leaves.
fn subplan_progress(items: &[PlanItem]) -> (usize, usize) {
    fn walk(items: &[PlanItem], total: &mut usize, completed: &mut usize) {
        for item in items {
            if item.is_leaf() {
                if item.status == PlanItemStatus::Cancelled {
                    continue;
                }
                *total += 1;
                if item.status == PlanItemStatus::Done {
                    *completed += 1;
                }
            } else {
                walk(&item.children, total, completed);
            }
        }
    }
    let mut total = 0;
    let mut completed = 0;
    walk(items, &mut total, &mut completed);
    (completed, total)
}

fn model_badge(selection: &ModelSelection) -> Span<'static> {
    let engine = current_engine();
    let palette = engine.palette();
    match selection {
        ModelSelection::Role(role) => match role {
            tauqe_protocol::ModelRole::Scribe => {
                Span::styled(" [Scr]", engine.fg(palette.status_success))
            }
            tauqe_protocol::ModelRole::Coder => {
                Span::styled(" [Cod]", engine.fg(palette.status_warning))
            }
            tauqe_protocol::ModelRole::Architect => {
                Span::styled(" [Arc]", engine.fg(palette.status_error))
            }
            tauqe_protocol::ModelRole::Primary => {
                Span::styled(" [Pri]", engine.fg(palette.status_info))
            }
            tauqe_protocol::ModelRole::Reviewer => {
                Span::styled(" [Rev]", engine.fg(palette.button_active_bg))
            }
        },
        ModelSelection::Specific(model_ref) => Span::styled(
            format!(" [{}]", model_ref.name),
            engine.fg(palette.status_info),
        ),
    }
}

fn issue_status_label(status: IssueStatus) -> &'static str {
    match status {
        IssueStatus::Discussion => "DISCUSSION",
        IssueStatus::Todo => "TODO",
        IssueStatus::InProgress => "IN_PROGRESS",
        IssueStatus::Refine => "REFINE",
        IssueStatus::Fixed => "FIXED",
        IssueStatus::Skipped => "SKIP",
        IssueStatus::Rejected => "REJECTED",
    }
}

fn location(files: &[String]) -> Option<String> {
    if files.is_empty() {
        None
    } else {
        Some(files.join(", "))
    }
}

fn split_title_and_file_path(title: &str) -> (&str, Option<&str>) {
    let trimmed = title.trim();
    if let Some(idx) = trimmed.rfind(" (") {
        if trimmed.ends_with(')') {
            let inside = &trimmed[idx + 2..trimmed.len() - 1];
            if inside.contains('/') || inside.contains('\\') || inside.contains(':') || inside.contains('.') {
                return (trimmed[..idx].trim(), Some(inside));
            }
        }
    }
    (trimmed, None)
}

fn pad_line_to_width(mut line: Line<'static>, width: usize, fill_style: Style) -> Line<'static> {
    let current_width: usize = line.spans.iter().map(|s| s.width()).sum();
    if current_width < width {
        line.spans.push(Span::styled(" ".repeat(width - current_width), fill_style));
    }
    line
}

fn compute_unified_review_lines(
    rows: &[VisibleReviewRow],
    selected_idx: usize,
    width: usize,
    theme_mode: crate::config::ThemeMode,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let engine = current_engine();
    let palette = engine.palette();
    let mut all_lines = Vec::new();
    let mut offsets = Vec::with_capacity(rows.len());

    for (idx, row) in rows.iter().enumerate() {
        let is_selected = idx == selected_idx;
        offsets.push(all_lines.len());

        let row_style = if is_selected {
            engine.bg(palette.overlay)
        } else {
            Style::default()
        };

        let gutter = if is_selected {
            Span::styled("▌", engine.fg(palette.status_info).bold())
        } else {
            Span::raw(" ")
        };

        match row {
            VisibleReviewRow::SessionHeader {
                session_id,
                title,
                model,
                created_at,
                total,
                fixed,
                todo,
                in_progress,
                discussion,
                skipped,
                rejected,
                is_expanded,
                focused,
                ..
            } => {
                if idx > 0 {
                    all_lines.push(Line::raw(""));
                }

                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let stats_str = format!(
                    "{}/{} fixed ({} todo, {} prog, {} disc, {} skip, {} rej)",
                    fixed, total, todo, in_progress, discussion, skipped, rejected
                );

                let mut spans = vec![
                    gutter,
                    Span::raw(" "),
                    Span::styled(fold_icon, engine.fg(palette.status_warning).bold()),
                    Span::styled(
                        format!("[{}] ", session_id),
                        engine.fg(palette.status_info).bold(),
                    ),
                    Span::styled(title.clone(), engine.fg(palette.text_primary).bold()),
                    Span::styled(
                        format!(" ({}) ", model),
                        engine.fg(palette.text_muted),
                    ),
                    Span::styled(
                        format!("{}  ", format_created_at(*created_at)),
                        engine.fg(palette.text_muted),
                    ),
                    Span::styled(
                        format!("({})", stats_str),
                        if is_selected {
                            engine.fg(palette.text_primary)
                        } else {
                            engine.fg(palette.text_muted)
                        },
                    ),
                ];
                if *focused {
                    spans.push(focus_badge());
                }

                let mut header_line = Line::from(spans).style(row_style);
                if is_selected {
                    header_line = pad_line_to_width(header_line, width, row_style);
                }
                all_lines.extend(wrap_lines(vec![header_line], width));
            }
            VisibleReviewRow::SessionSummary {
                description,
                is_expanded,
                ..
            } => {
                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let spans = vec![
                    gutter,
                    Span::raw("  "),
                    Span::styled(fold_icon, engine.fg(palette.status_warning).bold()),
                    Span::styled(
                        "Executive Summary:",
                        engine.fg(palette.status_success).bold(),
                    ),
                ];

                let mut summary_line = Line::from(spans).style(row_style);
                if is_selected {
                    summary_line = pad_line_to_width(summary_line, width, row_style);
                }
                let mut row_lines = vec![summary_line];

                if *is_expanded {
                    let trimmed = description.trim();
                    if !trimmed.is_empty() {
                        let mut theme =
                            crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                        theme.text_style = engine.fg(palette.text_secondary);
                        theme.hard_breaks = true;
                        theme.compact = true;
                        theme.line_prefix = Some(Span::styled(
                            "    │ ",
                            engine.fg(palette.text_muted),
                        ));
                        let md = crate::markdown::render_markdown(trimmed, &theme);
                        if !md.is_empty() {
                            row_lines.extend(md);
                        } else {
                            for line in trimmed.lines() {
                                row_lines.push(Line::from(vec![
                                    Span::styled(
                                        "    │ ",
                                        engine.fg(palette.text_muted),
                                    ),
                                    Span::styled(
                                        line.to_string(),
                                        engine.fg(palette.text_secondary),
                                    ),
                                ]));
                            }
                        }
                    }
                }

                all_lines.extend(wrap_lines(row_lines, width));
            }
            VisibleReviewRow::ReviewItem {
                item_id,
                title,
                issue_type,
                status,
                model,
                files,
                readonly_files,
                description,
                recommendations,
                note,
                plan,
                is_expanded,
                focused,
                ..
            } => {
                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let label = issue_status_label(*status);

                let title_style = if is_selected {
                    engine.fg(palette.text_primary).bold()
                } else if *status == IssueStatus::Fixed
                    || *status == IssueStatus::Skipped
                    || *status == IssueStatus::Rejected
                {
                    engine.fg(palette.text_secondary)
                } else {
                    Style::default().bold()
                };

                let raw_title = title.replace(['\r', '\n'], " ");
                let (display_title, extra_path) = split_title_and_file_path(&raw_title);

                let mut spans = vec![
                    gutter,
                    Span::raw("    "),
                    Span::styled(fold_icon, engine.fg(palette.status_warning).bold()),
                    Span::styled(
                        format!(" {} ", issue_type_badge(*issue_type)),
                        issue_type_style(*issue_type),
                    ),
                    Span::raw(" "),
                    Span::styled(format!(" {} ", label), issue_status_style(*status)),
                    Span::raw(" "),
                    Span::styled(format!("#{} {}", item_id, display_title), title_style),
                ];
                if let Some(selection) = model {
                    spans.push(model_badge(selection));
                }
                if !files.is_empty() {
                    spans.push(Span::styled(
                        format!(
                            " [{} file{}]",
                            files.len(),
                            if files.len() == 1 { "" } else { "s" }
                        ),
                        engine.fg(palette.text_muted),
                    ));
                }
                if !readonly_files.is_empty() {
                    spans.push(Span::styled(
                        format!(" [{} readonly]", readonly_files.len()),
                        engine.fg(palette.text_muted),
                    ));
                }
                if !plan.is_empty() {
                    let (done, total) = subplan_progress(plan.as_slice());
                    if total > 0 {
                        spans.push(Span::styled(
                            format!(" [{}/{} steps]", done, total),
                            engine.fg(palette.status_info),
                        ));
                    }
                }
                if *focused {
                    spans.push(focus_badge());
                }

                let mut item_line = Line::from(spans).style(row_style);
                if is_selected {
                    item_line = pad_line_to_width(item_line, width, row_style);
                }
                let mut row_lines = vec![item_line];

                if *is_expanded {
                    let loc_str = location(files)
                        .or_else(|| extra_path.map(|s| s.to_string()));
                    if let Some(loc) = loc_str {
                        let clean_loc = loc.trim();
                        if !clean_loc.is_empty() {
                            row_lines.push(Line::from(vec![
                                Span::styled(
                                    "        Files: ",
                                    engine.fg(palette.status_info).bold(),
                                ),
                                Span::styled(
                                    clean_loc.to_string(),
                                    engine.fg(palette.text_primary),
                                ),
                            ]));
                        }
                    }

                    if !readonly_files.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(
                                "        Readonly (do not modify): ",
                                engine.fg(palette.status_warning).bold(),
                            ),
                            Span::styled(
                                readonly_files.join(", "),
                                engine.fg(palette.text_primary),
                            ),
                        ]));
                    }

                    if let Some(notes) = note.as_deref() {
                        let clean_notes = notes.trim();
                        if !clean_notes.is_empty() {
                            let note_header = if *status == IssueStatus::Skipped {
                                "        Skip Reason: "
                            } else {
                                "        Note: "
                            };
                            let mut note_lines = clean_notes.lines();
                            if let Some(first) = note_lines.next() {
                                row_lines.push(Line::from(vec![
                                    Span::styled(
                                        note_header,
                                        engine.fg(palette.status_warning).bold(),
                                    ),
                                    Span::styled(
                                        first.to_string(),
                                        engine.fg(palette.text_primary),
                                    ),
                                ]));
                                for line in note_lines {
                                    row_lines.push(Line::from(vec![
                                        Span::styled(
                                            "        │ ",
                                            engine.fg(palette.text_muted),
                                        ),
                                        Span::styled(
                                            line.to_string(),
                                            engine.fg(palette.text_primary),
                                        ),
                                    ]));
                                }
                            }
                        }
                    }

                    if !description.trim().is_empty() {
                        row_lines.push(Line::from(Span::styled(
                            "        Description:",
                            engine.fg(palette.status_info).bold(),
                        )));
                        let mut theme =
                            crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                        theme.text_style = engine.fg(palette.text_secondary);
                        theme.hard_breaks = true;
                        theme.compact = true;
                        theme.line_prefix = Some(Span::styled(
                            "        │ ",
                            engine.fg(palette.text_muted),
                        ));
                        let md = crate::markdown::render_markdown(description.trim(), &theme);
                        if !md.is_empty() {
                            row_lines.extend(md);
                        }
                    }

                    if !recommendations.is_empty() {
                        row_lines.push(Line::from(Span::styled(
                            "        Recommendations:",
                            engine.fg(palette.status_success).bold(),
                        )));
                        for rec in recommendations {
                            row_lines.push(Line::from(vec![
                                Span::styled(
                                    "        │ • ",
                                    engine.fg(palette.text_muted),
                                ),
                                Span::styled(
                                    rec.clone(),
                                    engine.fg(palette.text_primary),
                                ),
                            ]));
                        }
                    }

                }

                all_lines.extend(wrap_lines(row_lines, width));
            }
            VisibleReviewRow::SubplanItem {
                step_id,
                title,
                details,
                status,
                files,
                readonly_files,
                depends_on,
                depth,
                is_expandable,
                is_expanded,
                ..
            } => {
                let indent = format!("    {}", "  ".repeat(*depth));
                let fold_icon = if *is_expandable {
                    if *is_expanded {
                        "▼ "
                    } else {
                        "▶ "
                    }
                } else if is_selected {
                    "▶ "
                } else {
                    "• "
                };
                let fold_style = if *is_expandable {
                    engine.fg(palette.status_warning).bold()
                } else if is_selected {
                    engine.fg(palette.status_info).bold()
                } else {
                    engine.fg(palette.text_muted)
                };
                let title_style = if is_selected {
                    engine.fg(palette.text_primary).bold()
                } else if *status == PlanItemStatus::Done
                    || *status == PlanItemStatus::Cancelled
                {
                    engine.fg(palette.text_secondary)
                } else {
                    Style::default().bold()
                };

                let clean_title = title.replace(['\r', '\n'], " ").trim().to_string();

                let mut spans = vec![
                    gutter,
                    Span::raw(indent.clone()),
                    Span::styled(fold_icon, fold_style),
                    Span::styled(format!(" {} ", status), plan_status_style(*status)),
                    Span::raw(" "),
                    Span::styled(format!("#{} {}", step_id, clean_title), title_style),
                ];
                if !files.is_empty() || !readonly_files.is_empty() {
                    let mut parts = Vec::new();
                    if !files.is_empty() {
                        parts.push(format!(
                            "{} file{}",
                            files.len(),
                            if files.len() == 1 { "" } else { "s" }
                        ));
                    }
                    if !readonly_files.is_empty() {
                        parts.push(format!("{} ref", readonly_files.len()));
                    }
                    spans.push(Span::styled(
                        format!(" [{}]", parts.join(" + ")),
                        engine.fg(palette.text_muted),
                    ));
                }

                let mut row_line = Line::from(spans).style(row_style);
                if is_selected {
                    row_line = pad_line_to_width(row_line, width, row_style);
                }
                let mut row_lines = vec![row_line];

                if *is_expanded {
                    let meta_prefix = format!("{}    │ ", indent);
                    if !files.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(meta_prefix.clone(), engine.fg(palette.text_muted)),
                            Span::styled("files: ", engine.fg(palette.text_muted)),
                            Span::styled(files.join(", "), engine.fg(palette.status_info)),
                        ]));
                    }
                    if !readonly_files.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(meta_prefix.clone(), engine.fg(palette.text_muted)),
                            Span::styled("ref: ", engine.fg(palette.text_muted)),
                            Span::styled(
                                readonly_files.join(", "),
                                engine.fg(palette.status_info),
                            ),
                        ]));
                    }
                    if !depends_on.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(meta_prefix.clone(), engine.fg(palette.text_muted)),
                            Span::styled("depends_on: ", engine.fg(palette.text_muted)),
                            Span::styled(depends_on.join(", "), engine.fg(palette.status_warning)),
                        ]));
                    }
                    if let Some(det) = details {
                        let trimmed = det.trim();
                        if !trimmed.is_empty() {
                            let mut theme =
                                crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                            theme.text_style = engine.fg(palette.text_secondary);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix =
                                Some(Span::styled(meta_prefix, engine.fg(palette.text_muted)));
                            let md = crate::markdown::render_markdown(trimmed, &theme);
                            if !md.is_empty() {
                                row_lines.extend(md);
                            }
                        }
                    }
                }

                all_lines.extend(wrap_lines(row_lines, width));
            }
        }
    }

    (all_lines, offsets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{IssueStatus, IssueType, ReviewIssue, ReviewSession};

    fn make_item(id: u32) -> ReviewIssue {
        ReviewIssue {
            id,
            title: format!("Finding {}", id),
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
        }
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

    fn make_session_with_description(
        id: &str,
        description: &str,
        item_ids: &[u32],
    ) -> ReviewSession {
        let mut session = make_session(id, item_ids);
        session.description = Some(description.to_string());
        session
    }

    #[test]
    fn test_expanded_session_header_renders_executive_summary_block() {
        let session = make_session_with_description(
            "rev-summary",
            "## Overview\n\nThe architecture is sound.",
            &[1],
        );
        let view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        let rows = view.flatten_rows();
        let (lines, _offsets) =
            compute_unified_review_lines(&rows, 0, 80, crate::config::ThemeMode::Dark);
        let texts: Vec<String> = lines.iter().map(|line| line.to_string()).collect();

        assert!(
            texts.iter().any(|t| t.contains("Executive Summary:")),
            "expected the Executive Summary header, got: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("The architecture is sound.")),
            "expected the rendered summary body, got: {texts:?}"
        );
    }

    #[test]
    fn test_collapsed_session_header_hides_executive_summary() {
        let session = make_session_with_description(
            "rev-collapsed",
            "## Overview\n\nThe architecture is sound.",
            &[1],
        );
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.collapsed_sessions.insert("rev-collapsed".to_string());
        let rows = view.flatten_rows();
        let (lines, _offsets) =
            compute_unified_review_lines(&rows, 0, 80, crate::config::ThemeMode::Dark);
        let texts: Vec<String> = lines.iter().map(|line| line.to_string()).collect();

        assert!(
            !texts.iter().any(|t| t.contains("Executive Summary:")),
            "a collapsed session must not render the Executive Summary block, got: {texts:?}"
        );
    }

    #[test]
    fn test_summary_fold_is_independent_of_findings() {
        let session = make_session_with_description(
            "rev-summary",
            "## Overview\n\nThe architecture is sound.",
            &[1],
        );
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        // Expanded session exposes an expanded summary row by default.
        let rows = view.flatten_rows();
        assert!(matches!(
            rows.get(1),
            Some(VisibleReviewRow::SessionSummary {
                is_expanded: true,
                ..
            })
        ));

        let (lines, _) =
            compute_unified_review_lines(&rows, 0, 80, crate::config::ThemeMode::Dark);
        let texts: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        assert!(texts.iter().any(|t| t.contains("Executive Summary:")));
        assert!(texts.iter().any(|t| t.contains("The architecture is sound.")));

        // Folding the summary hides only its body; findings stay visible.
        view.toggle_summary_expanded("rev-summary");
        assert!(!view.is_summary_expanded("rev-summary"));

        let rows = view.flatten_rows();
        assert!(matches!(
            rows.get(1),
            Some(VisibleReviewRow::SessionSummary {
                is_expanded: false,
                ..
            })
        ));
        let (lines, _) =
            compute_unified_review_lines(&rows, 0, 80, crate::config::ThemeMode::Dark);
        let texts: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        assert!(texts.iter().any(|t| t.contains("Executive Summary:")));
        assert!(
            !texts.iter().any(|t| t.contains("The architecture is sound.")),
            "a folded summary must hide its body, got: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("#1")),
            "findings must stay visible when the summary is folded, got: {texts:?}"
        );
    }

    #[test]
    fn test_toggle_fold_all_is_bound_to_summary_state() {
        let session = make_session_with_description("rev-summary", "Summary body.", &[1]);
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        // A folded summary makes the tree "not fully expanded".
        view.collapsed_summaries.insert("rev-summary".to_string());
        assert!(!view.is_all_expanded());

        // Expanding all also unfolds summaries.
        view.toggle_fold_all();
        assert!(view.is_summary_expanded("rev-summary"));
        assert!(view.is_all_expanded());

        // Collapsing all hides the session and its summary again.
        view.toggle_fold_all();
        assert!(!view.is_all_expanded());
    }

    #[test]
    fn test_review_item_multiple_files_badge_and_expanded_rendering() {
        let mut session = make_session("rev-files", &[1]);
        session.items[0].files = vec![
            "crates/core/src/review.rs".to_string(),
            "crates/protocol/src/review.rs".to_string(),
        ];
        let view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        // Collapsed item shows [2 files] badge
        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, 1, 100, crate::config::ThemeMode::Dark);
        let texts: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(
            texts.iter().any(|t| t.contains("[2 files]")),
            "collapsed finding should display file count badge, got: {texts:?}"
        );

        // Expanded item renders full files line
        let mut view_expanded = view;
        view_expanded.toggle_item_expanded("rev-files", 1);
        let rows_exp = view_expanded.flatten_rows();
        let (lines_exp, _) =
            compute_unified_review_lines(&rows_exp, 1, 100, crate::config::ThemeMode::Dark);
        let texts_exp: Vec<String> = lines_exp.iter().map(|l| l.to_string()).collect();
        assert!(
            texts_exp.iter().any(|t| t.contains("Files: ")
                && t.contains("crates/core/src/review.rs, crates/protocol/src/review.rs")),
            "expanded finding should render full file paths, got: {texts_exp:?}"
        );
    }

    #[test]
    fn test_review_item_skipped_badge_and_skip_reason_notes() {
        let mut session = make_session("rev-skip", &[1, 2]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            status: IssueStatus::Skipped,
            note: Some(
                "Already addressed in previous turn\nVerified no code change needed".to_string(),
            ),
            ..make_item(1)
        });
        session.items[1] = ReviewItem::from(ReviewIssue {
            status: IssueStatus::Fixed,
            note: Some("Fixed race condition".to_string()),
            ..make_item(2)
        });

        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.toggle_item_expanded("rev-skip", 1);
        view.toggle_item_expanded("rev-skip", 2);

        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, 1, 100, crate::config::ThemeMode::Dark);
        let texts: Vec<String> = lines.iter().map(|l| l.to_string()).collect();

        // 1. Badge displays compact SKIP
        assert!(
            texts.iter().any(|t| t.contains(" SKIP ")),
            "expected compact SKIP status badge, got: {texts:?}"
        );

        // 2. Multiline Skip Reason renders with vertical prefix for subsequent lines
        assert!(
            texts.iter().any(|t| t.contains("Skip Reason: ")
                && t.contains("Already addressed in previous turn")),
            "expected Skip Reason header and first line, got: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("│ Verified no code change needed")),
            "expected indented subsequent line with vertical bar prefix, got: {texts:?}"
        );

        // 3. Regular non-skipped item renders Note: header
        assert!(
            texts.iter().any(|t| t.contains("Note: Fixed race condition")),
            "expected regular Note: header for fixed item, got: {texts:?}"
        );
    }

    #[test]
    fn test_session_header_statistics_with_skipped() {
        let mut session = make_session("rev-stats", &[1, 2, 3, 4, 5, 6, 7]);
        session.items[0].status = IssueStatus::Fixed;
        session.items[1].status = IssueStatus::Todo;
        session.items[2].status = IssueStatus::InProgress;
        session.items[3].status = IssueStatus::Discussion;
        session.items[4].status = IssueStatus::Skipped;
        session.items[5].status = IssueStatus::Skipped;
        session.items[6].status = IssueStatus::Rejected;

        let view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        let rows = view.flatten_rows();

        if let VisibleReviewRow::SessionHeader {
            total,
            fixed,
            todo,
            in_progress,
            discussion,
            skipped,
            rejected,
            ..
        } = &rows[0]
        {
            assert_eq!(*total, 7);
            assert_eq!(*fixed, 1);
            assert_eq!(*todo, 1);
            assert_eq!(*in_progress, 1);
            assert_eq!(*discussion, 1);
            assert_eq!(*skipped, 2);
            assert_eq!(*rejected, 1);
        } else {
            panic!("expected SessionHeader row");
        }

        let (lines, _) =
            compute_unified_review_lines(&rows, 0, 200, crate::config::ThemeMode::Dark);
        let header_text = lines[0].to_string();
        assert!(
            header_text.contains("1/7 fixed (1 todo, 1 prog, 1 disc, 2 skip, 1 rej)"),
            "expected session statistics formatted with skip count, got: {header_text}"
        );
    }

    #[test]
    fn test_issue_type_badges_and_status_labels() {
        assert_eq!(issue_type_badge(IssueType::Bug), "BUG");
        assert_eq!(issue_type_badge(IssueType::Security), "SEC");
        assert_eq!(issue_type_badge(IssueType::Architecture), "ARCH");
        assert_eq!(issue_type_badge(IssueType::Performance), "PERF");
        assert_eq!(issue_type_badge(IssueType::Quality), "QUAL");

        assert_eq!(issue_status_label(IssueStatus::Refine), "REFINE");
        assert_eq!(issue_status_label(IssueStatus::Skipped), "SKIP");
        assert_eq!(issue_status_label(IssueStatus::InProgress), "IN_PROGRESS");
    }

    #[test]
    fn test_format_created_at_is_utc_and_stable() {
        assert_eq!(format_created_at(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_created_at(1_700_000_000), "2023-11-14 22:13 UTC");
    }

    #[test]
    fn test_session_header_renders_created_date() {
        let session = make_session("rev-date", &[1]);
        let view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, 0, 120, crate::config::ThemeMode::Dark);
        let header_text = lines[0].to_string();
        assert!(
            header_text.contains("1970-01-01 00:00 UTC"),
            "expected the formatted creation date in the header, got: {header_text}"
        );
    }

    #[test]
    fn test_review_item_renders_issue_type_and_refine_status() {
        let mut session = make_session("rev-refine-badge", &[1]);
        session.items[0].issue_type = IssueType::Bug;
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.toggle_item_expanded("rev-refine-badge", 1);

        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, 1, 120, crate::config::ThemeMode::Dark);
        let text: String = lines.iter().map(|line| line.to_string()).collect();

        // The authoritative category drives the badge directly.
        assert!(text.contains(" BUG "), "expected the issue-type badge: {text}");
    }

    #[test]
    fn test_review_focus_badge_and_subplan_rendering() {
        let mut session = make_session("rev-focus", &[1]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            plan: vec![
                PlanItem {
                    id: "1.1".to_string(),
                    title: "First subtask".to_string(),
                    status: PlanItemStatus::Done,
                    ..Default::default()
                },
                PlanItem {
                    id: "1.2".to_string(),
                    title: "Second subtask".to_string(),
                    status: PlanItemStatus::InProgress,
                    ..Default::default()
                },
            ],
            ..make_item(1)
        });
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.discussion_focus =
            tauqe_protocol::DiscussionFocus::review("rev-focus", vec!["1".to_string()]);
        view.toggle_item_expanded("rev-focus", 1);

        let rows = view.flatten_rows();
        assert!(matches!(
            &rows[0],
            VisibleReviewRow::SessionHeader { focused: false, .. }
        ));
        assert!(rows.iter().any(|r| matches!(
            r,
            VisibleReviewRow::ReviewItem {
                item_id: 1,
                focused: true,
                ..
            }
        )));

        let (lines, _) =
            compute_unified_review_lines(&rows, 1, 120, crate::config::ThemeMode::Dark);
        let mut text = String::new();
        for line in &lines {
            for span in &line.spans {
                text.push_str(&span.content);
            }
        }
        assert!(text.contains("[FOCUS]"), "focus badge expected: {text}");
        assert!(text.contains("[1/2 steps]"), "subplan badge expected: {text}");
        assert!(text.contains("#1.1 First subtask"), "subtask expected: {text}");
        assert!(text.contains("#1.2 Second subtask"), "subtask expected: {text}");
    }

    fn subplan_step(id: &str, title: &str, status: PlanItemStatus) -> PlanItem {
        PlanItem {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..Default::default()
        }
    }

    fn step_rows(rows: &[VisibleReviewRow]) -> Vec<(String, usize)> {
        rows.iter()
            .filter_map(|row| match row {
                VisibleReviewRow::SubplanItem { step_id, depth, .. } => {
                    Some((step_id.clone(), *depth))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_subplan_steps_flatten_into_interactive_rows() {
        let mut session = make_session("rev-sub", &[1]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            plan: vec![PlanItem {
                children: vec![
                    subplan_step("1.1", "First leaf", PlanItemStatus::Todo),
                    subplan_step("1.2", "Second leaf", PlanItemStatus::Done),
                ],
                ..subplan_step("1", "Parent step", PlanItemStatus::InProgress)
            }],
            ..make_item(1)
        });
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        // A collapsed finding exposes no subplan rows at all.
        assert!(step_rows(&view.flatten_rows()).is_empty());

        // An expanded finding exposes its top-level step only; children stay
        // folded until the container step is expanded.
        view.toggle_item_expanded("rev-sub", 1);
        assert_eq!(
            step_rows(&view.flatten_rows()),
            vec![("1".to_string(), 0)]
        );

        view.toggle_subplan_item_expanded("rev-sub", 1, "1");
        assert_eq!(
            step_rows(&view.flatten_rows()),
            vec![
                ("1".to_string(), 0),
                ("1.1".to_string(), 1),
                ("1.2".to_string(), 1),
            ]
        );

        // A row carries the owning finding, the step status, and its path.
        assert!(view.flatten_rows().iter().any(|row| matches!(
            row,
            VisibleReviewRow::SubplanItem {
                item_id: 1,
                step_id,
                status: PlanItemStatus::Done,
                ..
            } if step_id == "1.2"
        )));
    }

    #[test]
    fn test_subplan_step_details_and_files_render_when_expanded() {
        let mut session = make_session("rev-sub", &[1]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            plan: vec![PlanItem {
                id: "1.1".to_string(),
                title: "Wire the auth guard".to_string(),
                details: Some("Guard must reject anonymous callers.".to_string()),
                status: PlanItemStatus::InProgress,
                files: vec!["src/auth.rs".to_string()],
                readonly_files: vec!["contracts/api.md".to_string()],
                depends_on: vec!["1".to_string()],
                ..Default::default()
            }],
            ..make_item(1)
        });
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.toggle_item_expanded("rev-sub", 1);

        // Folded step shows only its title and the file badge.
        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, 2, 160, crate::config::ThemeMode::Dark);
        let folded: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(folded.contains("#1.1 Wire the auth guard"), "{folded}");
        assert!(folded.contains("[1 file + 1 ref]"), "{folded}");
        assert!(!folded.contains("depends_on: 1"), "{folded}");

        // Expanded step reveals target files, references, dependencies, and the
        // formatted description block.
        view.toggle_subplan_item_expanded("rev-sub", 1, "1.1");
        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, 2, 160, crate::config::ThemeMode::Dark);
        let expanded: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(expanded.contains("files: src/auth.rs"), "{expanded}");
        assert!(expanded.contains("ref: contracts/api.md"), "{expanded}");
        assert!(expanded.contains("depends_on: 1"), "{expanded}");
        assert!(
            expanded.contains("Guard must reject anonymous callers."),
            "{expanded}"
        );
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
        // Row 0 is the session header, row 1 is the finding.
        view.selected_index = 1;

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
}
