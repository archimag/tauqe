use std::collections::{HashSet, VecDeque};

use tauqe_protocol::{
    ArchitectTier, DiscussionFocus, DiscussionTarget, IssueStatus, IssueType, ModelEntry, ModelRef,
    ModelSelection, PlanItem, PlanItemStatus, ReviewIssue, ReviewItem, ReviewSession,
};

use crate::components::model_selector::ModelSelector;

pub mod input;
pub mod render;
pub mod tree;

pub use self::input::{handle_review_key, REVIEW_COMMAND_MAP};
pub use self::render::render_review_view;
use self::tree::{collect_subplan_keys, subplan_all_expanded, subplan_key};
pub(crate) use self::tree::{
    compute_unified_review_lines, format_created_at, issue_status_label, issue_status_style,
    issue_type_badge, issue_type_style, location, model_badge, plan_status_style,
};

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
        /// The Architect configuration that produced `plan`, rendered beside
        /// the step progress so the decomposition's provenance stays visible.
        subplan_architect: Option<ArchitectTier>,
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
                        && subplan_all_expanded(&self.expanded_items, &s.id, issue.id, &issue.plan)
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
                for stored in &session.items {
                    // The durable item carries the subplan's Architect
                    // provenance, which the issue view drops, so the row reads
                    // the stored item and projects it onto the view vocabulary
                    // exactly once.
                    let issue = ReviewIssue::from(stored.clone());
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
                        subplan_architect: stored.subplan_architect.clone(),
                        is_expanded: item_expanded,
                        focused,
                    });

                    // An expanded finding exposes its decomposed steps as
                    // first-class interactive rows, so the cursor can rest on a
                    // single step and fold its own details (Conventions §4.4).
                    if item_expanded {
                        self.flatten_subplan_rows(&session.id, issue.id, &issue.plan, 0, &mut rows);
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
                self.expanded_items
                    .insert(subplan_key(&target.session_id, item_id, ancestor));
            }
            if let Some(step_id) = target.step_id.as_deref() {
                self.expanded_items
                    .insert(subplan_key(&target.session_id, item_id, step_id));
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
                (
                    VisibleReviewRow::ReviewItem {
                        session_id,
                        item_id,
                        ..
                    },
                    Some(tid),
                    None,
                ) => session_id == &target.session_id && *item_id == tid,
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
