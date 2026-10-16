use std::collections::HashSet;

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use tauqe_protocol::{
    DiscussionFocus, DiscussionTarget, ModelRef, ModelSelection, PlanItem, PlanItemStatus,
    ReviewItem, ReviewItemStatus, ReviewSession, ReviewSeverity,
};

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
        body: String,
        severity: ReviewSeverity,
        status: ReviewItemStatus,
        model: Option<ModelSelection>,
        files: Vec<String>,
        resolution_notes: Option<String>,
        /// Embedded step plan decomposed by the Architect, rendered on expansion.
        subplan: Vec<PlanItem>,
        is_expanded: bool,
        /// Whether the active discussion focus anchors on this finding.
        focused: bool,
    },
}

impl VisibleReviewRow {
    pub fn session_id(&self) -> &str {
        match self {
            Self::SessionHeader { session_id, .. } => session_id,
            Self::SessionSummary { session_id, .. } => session_id,
            Self::ReviewItem { session_id, .. } => session_id,
        }
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
    pub fn apply_item(&mut self, review_id: Option<&str>, item: ReviewItem) {
        let in_scope = |session_id: &str| review_id.is_none_or(|id| id == session_id);
        if let Some(session) = self.session.as_mut() {
            if in_scope(&session.id) {
                if let Some(existing) = session.items.iter_mut().find(|i| i.id == item.id) {
                    *existing = item.clone();
                }
            }
        }
        for session in &mut self.sessions {
            if in_scope(&session.id) {
                if let Some(existing) = session.items.iter_mut().find(|i| i.id == item.id) {
                    *existing = item.clone();
                    break;
                }
            }
        }
    }

    /// Single path for changing a finding status (optimistic update and
    /// rollback). Updates both the session list used for rendering and the
    /// active session copy, scoped by `review_id` when provided.
    pub fn set_item_status(&mut self, review_id: Option<&str>, item_id: u32, status: ReviewItemStatus) {
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

    pub fn expand_all(&mut self) {
        self.collapsed_sessions.clear();
        self.collapsed_summaries.clear();
        let effective = self.effective_sessions();
        for s in &effective {
            for item in &s.items {
                self.expanded_items.insert(format!("{}:{}", s.id, item.id));
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
                && s.items.iter().all(|item| {
                    self.expanded_items
                        .contains(&format!("{}:{}", s.id, item.id))
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
                .filter(|i| i.status == ReviewItemStatus::Fixed)
                .count();
            let todo = session
                .items
                .iter()
                .filter(|i| i.status == ReviewItemStatus::Todo)
                .count();
            let in_progress = session
                .items
                .iter()
                .filter(|i| i.status == ReviewItemStatus::InProgress)
                .count();
            let discussion = session
                .items
                .iter()
                .filter(|i| i.status == ReviewItemStatus::Discussion)
                .count();
            let skipped = session
                .items
                .iter()
                .filter(|i| i.status == ReviewItemStatus::Skipped)
                .count();
            let rejected = session
                .items
                .iter()
                .filter(|i| i.status == ReviewItemStatus::Rejected)
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
                for item in &session.items {
                    if self.hide_closed
                        && (item.status == ReviewItemStatus::Fixed
                            || item.status == ReviewItemStatus::Skipped
                            || item.status == ReviewItemStatus::Rejected)
                    {
                        continue;
                    }
                    let item_expanded = self.is_item_expanded(&session.id, item.id);
                    rows.push(VisibleReviewRow::ReviewItem {
                        session_id: session.id.clone(),
                        item_id: item.id,
                        title: item.title.clone(),
                        body: item.body.clone(),
                        severity: item.severity,
                        status: item.status,
                        model: item.model.clone(),
                        files: item.files.clone(),
                        resolution_notes: item.resolution_notes.clone(),
                        subplan: item.subplan.clone(),
                        is_expanded: item_expanded,
                        focused: self.is_item_focused(&session.id, item.id),
                    });
                }
            }
        }
        rows
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

    /// Accordion-style navigation: moves to the next or previous finding/session,
    /// closing previous items and opening the new target item.
    pub fn accordion_navigate(&mut self, forward: bool) {
        #[derive(Clone)]
        struct ReviewNodeRef {
            session_id: String,
            item_id: Option<u32>,
        }

        let effective = self.effective_sessions();
        let mut all_nodes = Vec::new();
        for session in &effective {
            all_nodes.push(ReviewNodeRef {
                session_id: session.id.clone(),
                item_id: None,
            });
            for item in &session.items {
                if self.hide_closed
                    && (item.status == ReviewItemStatus::Fixed
                        || item.status == ReviewItemStatus::Skipped
                        || item.status == ReviewItemStatus::Rejected)
                {
                    continue;
                }
                all_nodes.push(ReviewNodeRef {
                    session_id: session.id.clone(),
                    item_id: Some(item.id),
                });
            }
        }

        if all_nodes.is_empty() {
            return;
        }

        let sel_row = self.selected_row();
        let cur_idx = match sel_row {
            Some(VisibleReviewRow::SessionHeader { session_id, .. }) => {
                all_nodes.iter().position(|n| n.session_id == session_id && n.item_id.is_none()).unwrap_or(0)
            }
            Some(VisibleReviewRow::SessionSummary { session_id, .. }) => {
                all_nodes.iter().position(|n| n.session_id == session_id && n.item_id.is_none()).unwrap_or(0)
            }
            Some(VisibleReviewRow::ReviewItem { session_id, item_id, .. }) => {
                all_nodes.iter().position(|n| n.session_id == session_id && n.item_id == Some(item_id)).unwrap_or(0)
            }
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
            if session.id == target.session_id {
                if target.item_id.is_some() {
                    self.collapsed_sessions.remove(&target.session_id);
                } else {
                    self.collapsed_sessions.insert(target.session_id.clone());
                }
            } else {
                self.collapsed_sessions.insert(session.id.clone());
            }
        }

        self.expanded_items.clear();
        if let Some(item_id) = target.item_id {
            self.expanded_items.insert(format!("{}:{}", target.session_id, item_id));
        }

        let rows = self.flatten_rows();
        if let Some(pos) = rows.iter().position(|r| match (r, target.item_id) {
            (VisibleReviewRow::SessionHeader { session_id, .. }, None) => session_id == &target.session_id,
            (VisibleReviewRow::ReviewItem { session_id, item_id, .. }, Some(tid)) => {
                session_id == &target.session_id && *item_id == tid
            }
            _ => false,
        }) {
            self.selected_index = pos;
        }

        self.clamp_selection();
        self.scroll_to_selected();
    }
}

fn severity_style(severity: ReviewSeverity) -> Style {
    let engine = current_engine();
    let palette = engine.palette();
    match severity {
        ReviewSeverity::Critical => engine.fg(palette.status_error).bold(),
        ReviewSeverity::Warning => engine.fg(palette.status_warning).bold(),
        ReviewSeverity::Suggestion => engine.fg(palette.status_info).bold(),
        ReviewSeverity::Info => engine.fg(palette.text_secondary).bold(),
    }
}

fn status_style(status: ReviewItemStatus) -> Style {
    let engine = current_engine();
    let palette = engine.palette();
    let inverse = engine.color(palette.text_inverse);
    match status {
        ReviewItemStatus::Discussion => engine.bg(palette.button_active_bg).fg(inverse).bold(),
        ReviewItemStatus::Todo => engine.bg(palette.status_warning).fg(inverse).bold(),
        ReviewItemStatus::InProgress => engine.bg(palette.status_info).fg(inverse).bold(),
        ReviewItemStatus::Fixed => engine.bg(palette.status_success).fg(inverse).bold(),
        ReviewItemStatus::Skipped => engine.bg(palette.text_muted).fg(inverse).bold(),
        ReviewItemStatus::Rejected => engine.bg(palette.text_muted).fg(inverse).bold(),
    }
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

/// Renders the embedded subplan tree as indented, status-badged rows shown when
/// a finding is expanded.
fn subplan_lines(items: &[PlanItem], depth: usize) -> Vec<Line<'static>> {
    let engine = current_engine();
    let palette = engine.palette();
    let mut out = Vec::new();
    for item in items {
        let mut prefix = String::from("        │ ");
        prefix.push_str(&"  ".repeat(depth));
        out.push(Line::from(vec![
            Span::styled(prefix, engine.fg(palette.text_muted)),
            Span::styled(format!(" {} ", item.status), plan_status_style(item.status)),
            Span::raw(" "),
            Span::styled(
                format!("#{} {}", item.id, item.title),
                engine.fg(palette.text_primary),
            ),
        ]));
        if !item.children.is_empty() {
            out.extend(subplan_lines(&item.children, depth + 1));
        }
    }
    out
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

fn status_label(status: ReviewItemStatus) -> &'static str {
    match status {
        ReviewItemStatus::Discussion => "DISCUSSION",
        ReviewItemStatus::Todo => "TODO",
        ReviewItemStatus::InProgress => "IN_PROGRESS",
        ReviewItemStatus::Fixed => "FIXED",
        ReviewItemStatus::Skipped => "SKIP",
        ReviewItemStatus::Rejected => "REJECTED",
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
    let current_width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
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
                body,
                severity,
                status,
                model,
                files,
                resolution_notes,
                subplan,
                is_expanded,
                focused,
                ..
            } => {
                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let label = status_label(*status);

                let title_style = if is_selected {
                    engine.fg(palette.text_primary).bold()
                } else if *status == ReviewItemStatus::Fixed
                    || *status == ReviewItemStatus::Skipped
                    || *status == ReviewItemStatus::Rejected
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
                    Span::styled(format!(" {} ", label), status_style(*status)),
                    Span::raw(" "),
                    Span::styled(format!("[{}]", severity), severity_style(*severity)),
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
                if !subplan.is_empty() {
                    let (done, total) = subplan_progress(subplan.as_slice());
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

                    if let Some(notes) = resolution_notes.as_deref() {
                        let clean_notes = notes.trim();
                        if !clean_notes.is_empty() {
                            let note_header = if *status == ReviewItemStatus::Skipped {
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

                    let clean_body = body.trim();
                    if !clean_body.is_empty() {
                        let mut theme =
                            crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                        theme.text_style = engine.fg(palette.text_secondary);
                        theme.hard_breaks = true;
                        theme.compact = true;
                        theme.line_prefix = Some(Span::styled(
                            "        │ ",
                            engine.fg(palette.text_muted),
                        ));
                        let md = crate::markdown::render_markdown(clean_body, &theme);
                        if !md.is_empty() {
                            row_lines.extend(md);
                        }
                    }

                    if !subplan.is_empty() {
                        row_lines.push(Line::from(Span::styled(
                            "        Subtasks:",
                            engine.fg(palette.status_info).bold(),
                        )));
                        row_lines.extend(subplan_lines(subplan.as_slice(), 0));
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
    use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSeverity, ReviewSession};

    fn make_item(id: u32) -> ReviewItem {
        ReviewItem {
            id,
            title: format!("Finding {}", id),
            model: None,
            severity: ReviewSeverity::Warning,
            status: ReviewItemStatus::Todo,
            files: Vec::new(),
            subplan: vec![],
            resolution_notes: None,
            body: String::new(),
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
            items: item_ids.iter().map(|&i| make_item(i)).collect(),
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
        view.set_item_status(Some("rev-2"), 1, ReviewItemStatus::Fixed);
        assert_eq!(view.sessions[0].items[0].status, ReviewItemStatus::Todo);
        assert_eq!(view.sessions[1].items[0].status, ReviewItemStatus::Fixed);
        assert_eq!(
            view.session.as_ref().map(|s| s.items[0].status),
            Some(ReviewItemStatus::Todo)
        );

        // Updating the active session keeps both copies in sync.
        view.set_item_status(Some("rev-1"), 1, ReviewItemStatus::InProgress);
        assert_eq!(view.sessions[0].items[0].status, ReviewItemStatus::InProgress);
        assert_eq!(
            view.session.as_ref().map(|s| s.items[0].status),
            Some(ReviewItemStatus::InProgress)
        );
        assert_eq!(view.sessions[1].items[0].status, ReviewItemStatus::Fixed);

        // apply_item is scoped the same way.
        let mut replacement = make_item(1);
        replacement.status = ReviewItemStatus::Rejected;
        view.apply_item(Some("rev-2"), replacement);
        assert_eq!(view.sessions[0].items[0].status, ReviewItemStatus::InProgress);
        assert_eq!(view.sessions[1].items[0].status, ReviewItemStatus::Rejected);
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
        session.items[0].status = ReviewItemStatus::Skipped;
        session.items[0].resolution_notes = Some(
            "Already addressed in previous turn\nVerified no code change needed".to_string(),
        );
        session.items[1].status = ReviewItemStatus::Fixed;
        session.items[1].resolution_notes = Some("Fixed race condition".to_string());

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
        session.items[0].status = ReviewItemStatus::Fixed;
        session.items[1].status = ReviewItemStatus::Todo;
        session.items[2].status = ReviewItemStatus::InProgress;
        session.items[3].status = ReviewItemStatus::Discussion;
        session.items[4].status = ReviewItemStatus::Skipped;
        session.items[5].status = ReviewItemStatus::Skipped;
        session.items[6].status = ReviewItemStatus::Rejected;

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
    fn test_review_focus_badge_and_subplan_rendering() {
        let mut session = make_session("rev-focus", &[1]);
        session.items[0].subplan = vec![
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
        ];
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
        assert!(text.contains("Subtasks:"), "subtask header expected: {text}");
        assert!(text.contains("#1.1 First subtask"), "subtask expected: {text}");
        assert!(text.contains("#1.2 Second subtask"), "subtask expected: {text}");
    }
}
