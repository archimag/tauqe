use std::collections::HashSet;

use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use tauqe_protocol::{ModelSelection, ReviewItem, ReviewSession, ReviewSeverity, ReviewStatus};

use crate::ui::wrap_lines;

pub mod input;
pub mod render;

pub use self::input::{handle_review_key, REVIEW_COMMANDS};
pub use self::render::render_review_view;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleReviewRow {
    SessionHeader {
        session_id: String,
        title: String,
        description: Option<String>,
        created_at: u64,
        model: String,
        total: usize,
        fixed: usize,
        todo: usize,
        in_progress: usize,
        discussion: usize,
        rejected: usize,
        is_expanded: bool,
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
        status: ReviewStatus,
        model: Option<ModelSelection>,
        file_path: Option<String>,
        line_range: Option<(usize, usize)>,
        is_expanded: bool,
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
    pub fn set_item_status(&mut self, review_id: Option<&str>, item_id: u32, status: ReviewStatus) {
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

    fn effective_sessions(&self) -> Vec<ReviewSession> {
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

    pub fn flatten_rows(&self) -> Vec<VisibleReviewRow> {
        let mut rows = Vec::new();
        let effective = self.effective_sessions();

        for session in &effective {
            let is_expanded = self.is_session_expanded(&session.id);
            let total = session.items.len();
            let fixed = session
                .items
                .iter()
                .filter(|i| i.status == ReviewStatus::Fixed)
                .count();
            let todo = session
                .items
                .iter()
                .filter(|i| i.status == ReviewStatus::Todo)
                .count();
            let in_progress = session
                .items
                .iter()
                .filter(|i| i.status == ReviewStatus::InProgress)
                .count();
            let discussion = session
                .items
                .iter()
                .filter(|i| i.status == ReviewStatus::Discussion)
                .count();
            let rejected = session
                .items
                .iter()
                .filter(|i| i.status == ReviewStatus::Rejected)
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
                rejected,
                is_expanded,
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
                        && (item.status == ReviewStatus::Fixed
                            || item.status == ReviewStatus::Rejected)
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
                        file_path: item.file_path.clone(),
                        line_range: item.line_range,
                        is_expanded: item_expanded,
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
                    && (item.status == ReviewStatus::Fixed || item.status == ReviewStatus::Rejected)
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
    match severity {
        ReviewSeverity::Critical => Style::default().fg(Color::Red).bold(),
        ReviewSeverity::Warning => Style::default().fg(Color::Yellow).bold(),
        ReviewSeverity::Suggestion => Style::default().fg(Color::Cyan).bold(),
        ReviewSeverity::Info => Style::default().fg(Color::Gray).bold(),
    }
}

fn status_style(status: ReviewStatus) -> Style {
    match status {
        ReviewStatus::Discussion => Style::default().bg(Color::Magenta).fg(Color::Black).bold(),
        ReviewStatus::Todo => Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
        ReviewStatus::InProgress => Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ReviewStatus::Fixed => Style::default().bg(Color::Green).fg(Color::Black).bold(),
        ReviewStatus::Rejected => Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
    }
}

fn model_badge(selection: &ModelSelection) -> Span<'static> {
    match selection {
        ModelSelection::Role(role) => match role {
            tauqe_protocol::ModelRole::Scribe => {
                Span::styled(" [Scr]", Style::default().fg(Color::Green))
            }
            tauqe_protocol::ModelRole::Coder => {
                Span::styled(" [Cod]", Style::default().fg(Color::Yellow))
            }
            tauqe_protocol::ModelRole::Architect => {
                Span::styled(" [Arc]", Style::default().fg(Color::Red))
            }
            tauqe_protocol::ModelRole::Primary => {
                Span::styled(" [Pri]", Style::default().fg(Color::Cyan))
            }
            tauqe_protocol::ModelRole::Reviewer => {
                Span::styled(" [Rev]", Style::default().fg(Color::Magenta))
            }
        },
        ModelSelection::Specific(model_ref) => {
            Span::styled(format!(" [{}]", model_ref.name), Style::default().fg(Color::Cyan))
        }
    }
}

fn status_label(status: ReviewStatus) -> &'static str {
    match status {
        ReviewStatus::Discussion => "DISCUSSION",
        ReviewStatus::Todo => "TODO",
        ReviewStatus::InProgress => "IN_PROGRESS",
        ReviewStatus::Fixed => "FIXED",
        ReviewStatus::Rejected => "REJECTED",
    }
}

fn location(file_path: Option<&str>, line_range: Option<(usize, usize)>) -> Option<String> {
    file_path.map(|path| match line_range {
        Some((a, b)) if a == b => format!("{}:{}", path, a),
        Some((a, b)) => format!("{}:{}-{}", path, a, b),
        None => path.to_string(),
    })
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
    let mut all_lines = Vec::new();
    let mut offsets = Vec::with_capacity(rows.len());

    for (idx, row) in rows.iter().enumerate() {
        let is_selected = idx == selected_idx;
        offsets.push(all_lines.len());

        let row_style = if is_selected {
            Style::default().bg(Color::DarkGray)
        } else {
            Style::default()
        };

        let gutter = if is_selected {
            Span::styled("▌", Style::default().fg(Color::Cyan).bold())
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
                rejected,
                is_expanded,
                ..
            } => {
                if idx > 0 {
                    all_lines.push(Line::raw(""));
                }

                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let stats_str = format!(
                    "{}/{} fixed ({} todo, {} prog, {} disc, {} rej)",
                    fixed, total, todo, in_progress, discussion, rejected
                );

                let spans = vec![
                    gutter,
                    Span::raw(" "),
                    Span::styled(fold_icon, Style::default().fg(Color::Yellow).bold()),
                    Span::styled(format!("[{}] ", session_id), Style::default().bold().fg(Color::Cyan)),
                    Span::styled(title.clone(), Style::default().bold().fg(Color::White)),
                    Span::styled(format!(" ({}) ", model), Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("({})", stats_str), Style::default().fg(if is_selected { Color::White } else { Color::DarkGray })),
                ];

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
                    Span::styled(fold_icon, Style::default().fg(Color::Yellow).bold()),
                    Span::styled(
                        "Executive Summary:",
                        Style::default().fg(Color::Green).bold(),
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
                        theme.text_style = Style::default().fg(Color::Gray);
                        theme.hard_breaks = true;
                        theme.compact = true;
                        theme.line_prefix = Some(Span::styled("    │ ", Style::default().fg(Color::DarkGray)));
                        let md = crate::markdown::render_markdown(trimmed, &theme);
                        if !md.is_empty() {
                            row_lines.extend(md);
                        } else {
                            for line in trimmed.lines() {
                                row_lines.push(Line::from(vec![
                                    Span::styled("    │ ", Style::default().fg(Color::DarkGray)),
                                    Span::styled(line.to_string(), Style::default().fg(Color::Gray)),
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
                file_path,
                line_range,
                is_expanded,
                ..
            } => {
                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let label = status_label(*status);

                let title_style = if is_selected {
                    Style::default().fg(Color::White).bold()
                } else if *status == ReviewStatus::Fixed || *status == ReviewStatus::Rejected {
                    Style::default().fg(Color::Gray)
                } else {
                    Style::default().bold()
                };

                let raw_title = title.replace(['\r', '\n'], " ");
                let (display_title, extra_path) = split_title_and_file_path(&raw_title);

                let mut spans = vec![
                    gutter,
                    Span::raw("    "),
                    Span::styled(fold_icon, Style::default().fg(Color::Yellow).bold()),
                    Span::styled(format!(" {} ", label), status_style(*status)),
                    Span::raw(" "),
                    Span::styled(format!("[{}]", severity), severity_style(*severity)),
                    Span::raw(" "),
                    Span::styled(format!("#{} {}", item_id, display_title), title_style),
                ];
                if let Some(selection) = model {
                    spans.push(model_badge(selection));
                }

                let mut item_line = Line::from(spans).style(row_style);
                if is_selected {
                    item_line = pad_line_to_width(item_line, width, row_style);
                }
                let mut row_lines = vec![item_line];

                if *is_expanded {
                    let loc_str = location(file_path.as_deref(), *line_range)
                        .or_else(|| extra_path.map(|s| s.to_string()));
                    if let Some(loc) = loc_str {
                        let clean_loc = loc.trim();
                        if !clean_loc.is_empty() {
                            row_lines.push(Line::from(vec![
                                Span::styled("        File: ", Style::default().fg(Color::Cyan).bold()),
                                Span::styled(clean_loc.to_string(), Style::default().fg(Color::White)),
                            ]));
                        }
                    }

                    let clean_body = body.trim();
                    if !clean_body.is_empty() {
                        let mut theme =
                            crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                        theme.text_style = Style::default().fg(Color::Gray);
                        theme.hard_breaks = true;
                        theme.compact = true;
                        theme.line_prefix = Some(Span::styled("        │ ", Style::default().fg(Color::DarkGray)));
                        let md = crate::markdown::render_markdown(clean_body, &theme);
                        if !md.is_empty() {
                            row_lines.extend(md);
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
    use tauqe_protocol::{ReviewItem, ReviewSeverity, ReviewSession, ReviewStatus};

    fn make_item(id: u32) -> ReviewItem {
        ReviewItem {
            id,
            title: format!("Finding {}", id),
            model: None,
            severity: ReviewSeverity::Warning,
            status: ReviewStatus::Todo,
            file_path: None,
            line_range: None,
            resolution_notes: None,
            body: String::new(),
        }
    }

    fn make_session(id: &str, item_ids: &[u32]) -> ReviewSession {
        ReviewSession {
            id: id.to_string(),
            title: format!("Session {}", id),
            created_at: 0,
            model: "test-model".to_string(),
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
        view.set_item_status(Some("rev-2"), 1, ReviewStatus::Fixed);
        assert_eq!(view.sessions[0].items[0].status, ReviewStatus::Todo);
        assert_eq!(view.sessions[1].items[0].status, ReviewStatus::Fixed);
        assert_eq!(
            view.session.as_ref().map(|s| s.items[0].status),
            Some(ReviewStatus::Todo)
        );

        // Updating the active session keeps both copies in sync.
        view.set_item_status(Some("rev-1"), 1, ReviewStatus::InProgress);
        assert_eq!(view.sessions[0].items[0].status, ReviewStatus::InProgress);
        assert_eq!(
            view.session.as_ref().map(|s| s.items[0].status),
            Some(ReviewStatus::InProgress)
        );
        assert_eq!(view.sessions[1].items[0].status, ReviewStatus::Fixed);

        // apply_item is scoped the same way.
        let mut replacement = make_item(1);
        replacement.status = ReviewStatus::Rejected;
        view.apply_item(Some("rev-2"), replacement);
        assert_eq!(view.sessions[0].items[0].status, ReviewStatus::InProgress);
        assert_eq!(view.sessions[1].items[0].status, ReviewStatus::Rejected);
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
}
