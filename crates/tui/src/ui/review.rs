use std::collections::HashSet;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};
use tauqe_protocol::{ModelSelection, ReviewItem, ReviewSession, ReviewSeverity, ReviewStatus};

use crate::app::AppState;
use crate::ui::develop::ReasoningState;
use crate::ui::wrap_lines;

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
            Self::ReviewItem { session_id, .. } => session_id,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReviewViewState {
    pub sessions: Vec<ReviewSession>,
    pub session: Option<ReviewSession>,
    pub selected_index: usize,
    pub scroll: u16,
    pub collapsed_sessions: HashSet<String>,
    pub expanded_items: HashSet<String>,
    pub hide_closed: bool,
    pub running: bool,
    pub reasoning: ReasoningState,
    pub content: String,
    pub error: Option<String>,
    pub view_height: u16,
    pub content_width: usize,
    pub rendered_lines: usize,
}

impl ReviewViewState {
    pub fn begin(&mut self) {
        self.running = true;
        self.reasoning.clear();
        self.content.clear();
        self.error = None;
        self.scroll = 0;
    }

    pub fn set_session(&mut self, session: ReviewSession) {
        self.running = false;
        self.update_session(session);
        self.error = None;
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

    pub fn apply_item(&mut self, item: ReviewItem) {
        if let Some(session) = self.session.as_mut() {
            if let Some(existing) = session.items.iter_mut().find(|i| i.id == item.id) {
                *existing = item.clone();
            }
        }
        for session in &mut self.sessions {
            if let Some(existing) = session.items.iter_mut().find(|i| i.id == item.id) {
                *existing = item.clone();
                break;
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

        let (lines, offsets) = compute_unified_review_lines(&rows, sel, self.content_width.max(10));
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
        ModelSelection::Tier(tier) => match tier {
            tauqe_protocol::ModelTier::Junior => {
                Span::styled(" [junior]", Style::default().fg(Color::Green))
            }
            tauqe_protocol::ModelTier::Middle => {
                Span::styled(" [middle]", Style::default().fg(Color::Yellow))
            }
            tauqe_protocol::ModelTier::Senior => {
                Span::styled(" [senior]", Style::default().fg(Color::Red))
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
                description,
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

                let mut spans = vec![
                    gutter,
                    Span::raw(" "),
                    Span::styled(fold_icon, Style::default().fg(Color::Yellow).bold()),
                    Span::styled(format!("[{}] ", session_id), Style::default().bold().fg(Color::Cyan)),
                    Span::styled(title.clone(), Style::default().bold().fg(Color::White)),
                    Span::styled(format!(" ({}) ", model), Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("({})", stats_str), Style::default().fg(if is_selected { Color::White } else { Color::DarkGray })),
                ];

                if is_selected {
                    spans.push(Span::styled(
                        "  [Tab: fold, x: exec all todo, d: discuss, Del: delete]",
                        Style::default().fg(Color::Yellow).bold(),
                    ));
                }

                let mut header_line = Line::from(spans).style(row_style);
                if is_selected {
                    header_line = pad_line_to_width(header_line, width, row_style);
                }
                all_lines.extend(wrap_lines(vec![header_line], width));

                if *is_expanded {
                    if let Some(desc) = description {
                        let trimmed = desc.trim();
                        if !trimmed.is_empty() {
                            let mut desc_lines = Vec::new();
                            let mut theme = crate::markdown::MarkdownTheme::answer();
                            theme.text_style = Style::default().fg(Color::Gray);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix = Some(Span::styled("    │ ", Style::default().fg(Color::DarkGray)));
                            let md = crate::markdown::render_markdown(trimmed, &theme);
                            if !md.is_empty() {
                                desc_lines.extend(md);
                            } else {
                                for line in trimmed.lines() {
                                    desc_lines.push(Line::from(vec![
                                        Span::styled("    │ ", Style::default().fg(Color::DarkGray)),
                                        Span::styled(line.to_string(), Style::default().fg(Color::Gray)),
                                    ]));
                                }
                            }
                            all_lines.extend(wrap_lines(desc_lines, width));
                        }
                    }
                }
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

                if is_selected {
                    let hint = if *status == ReviewStatus::Discussion {
                        "  [Tab: fold, d: discuss, t: status (approve to todo)]"
                    } else if *status == ReviewStatus::Fixed || *status == ReviewStatus::Rejected {
                        "  [Tab: fold, t: status]"
                    } else {
                        "  [x: exec, Tab: fold, d: discuss, t: status]"
                    };
                    spans.push(Span::styled(hint, Style::default().fg(Color::Yellow).bold()));
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
                        let mut theme = crate::markdown::MarkdownTheme::answer();
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

fn streaming_lines(review: &ReviewViewState, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        " Reviewing active context...",
        Style::default().fg(Color::Yellow).bold(),
    ))];
    if !review.reasoning.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(review.reasoning.render_lines());
        lines.push(Line::raw(""));
    }
    if !review.content.is_empty() {
        lines.push(Line::from(Span::styled(
            " Live Findings Draft:",
            Style::default().fg(Color::Green).bold(),
        )));
        let md = crate::markdown::render_markdown(
            &review.content,
            &crate::markdown::MarkdownTheme::answer(),
        );
        lines.extend(md);
    }
    wrap_lines(lines, width)
}

pub fn render_review_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    main_area: Rect,
    info_area: Rect,
) {
    let block = Block::default().padding(Padding::horizontal(1));
    let inner = block.inner(main_area);
    let height = inner.height;
    let width = (inner.width as usize).max(10);
    state.review.view_height = height;
    state.review.content_width = width;

    if state.review.running {
        let lines = streaming_lines(&state.review, width);
        state.review.rendered_lines = lines.len();
        let max_scroll = (lines.len() as u16).saturating_sub(height);
        if state.review.scroll > max_scroll {
            state.review.scroll = max_scroll;
        }
        let paragraph = Paragraph::new(lines).block(block).scroll((state.review.scroll, 0));
        frame.render_widget(paragraph, main_area);

        let status_line = Line::from(vec![
            Span::styled(" Reviewing... ", Style::default().fg(Color::Yellow).bold()),
        ]);
        let info = Paragraph::new(vec![status_line]).block(Block::default().padding(Padding::horizontal(1)));
        frame.render_widget(info, info_area);
        return;
    }

    let rows = state.review.flatten_rows();
    state.review.clamp_selection();

    let (lines, offsets) = if rows.is_empty() {
        (
            vec![
                Line::raw(""),
                Line::from(Span::styled(
                    "  No code reviews yet.",
                    Style::default().fg(Color::DarkGray).bold(),
                )),
                Line::from(Span::styled(
                    "  Add files to Context (Ctrl+2), then press 'r' to run a code review.",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            Vec::new(),
        )
    } else {
        compute_unified_review_lines(&rows, state.review.selected_index, width)
    };

    state.review.rendered_lines = lines.len();

    if !rows.is_empty() && height > 0 {
        let sel = state.review.selected_index.min(rows.len() - 1);
        let start = offsets.get(sel).copied().unwrap_or(0);
        let end = offsets.get(sel + 1).copied().unwrap_or(lines.len());
        let scroll = state.review.scroll as usize;

        let new_scroll = if sel == 0 {
            0
        } else if start < scroll {
            start
        } else if end > scroll + height as usize {
            end.saturating_sub(height as usize).min(start)
        } else {
            scroll
        };
        state.review.scroll = new_scroll as u16;
    }

    let max_scroll = (lines.len() as u16).saturating_sub(height);
    if state.review.scroll > max_scroll {
        state.review.scroll = max_scroll;
    }

    let tree_widget = Paragraph::new(lines).block(block).scroll((state.review.scroll, 0));
    frame.render_widget(tree_widget, main_area);

    let effective = state.review.effective_sessions();
    let total_sessions = effective.len();
    let total_items: usize = effective.iter().map(|s| s.items.len()).sum();
    let fixed_items: usize = effective
        .iter()
        .map(|s| s.items.iter().filter(|i| i.status == ReviewStatus::Fixed).count())
        .sum();

    let mut status_spans = Vec::new();
    if let Some(err) = &state.review.error {
        status_spans.push(Span::styled(format!(" {}", err), Style::default().fg(Color::Red).bold()));
    } else {
        status_spans.push(Span::styled(
            format!(
                " {} review session(s), {} finding(s) ({} fixed)",
                total_sessions, total_items, fixed_items
            ),
            Style::default().fg(Color::Gray),
        ));
        if state.review.hide_closed {
            status_spans.push(Span::styled(" | Filter: Open only", Style::default().fg(Color::Yellow)));
        }
        if !rows.is_empty() {
            status_spans.push(Span::styled(
                format!(" | [Row {}/{}]", state.review.selected_index + 1, rows.len()),
                Style::default().fg(Color::Gray),
            ));
        }
        if let Some(ref queue) = state.review_batch_queue {
            let cur = queue.current_index + 1;
            let total = queue.items.len();
            let item_id = queue.items.get(queue.current_index).map(|i| i.id).unwrap_or(0);
            status_spans.push(Span::styled(
                format!(" | [BATCH: Finding {}/{} (#{})]", cur, total, item_id),
                Style::default().fg(Color::Yellow).bold(),
            ));
        }
    }

    let info_widget = Paragraph::new(vec![Line::from(status_spans)])
        .block(Block::default().padding(Padding::horizontal(1)));
    frame.render_widget(info_widget, info_area);
}
