use std::collections::HashSet;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};
use tauqe_protocol::{ReviewItem, ReviewSession, ReviewSeverity, ReviewStatus};

use crate::app::AppState;
use crate::ui::develop::ReasoningState;
use crate::ui::wrap_lines;

#[derive(Debug, Clone, Default)]
pub struct ReviewViewState {
    pub session: Option<ReviewSession>,
    pub selected_index: usize,
    pub scroll: u16,
    pub expanded: HashSet<u32>,
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
        self.session = Some(session);
        self.selected_index = 0;
        self.scroll = 0;
        self.expanded.clear();
        self.error = None;
    }

    pub fn apply_item(&mut self, item: ReviewItem) {
        if let Some(session) = self.session.as_mut() {
            if let Some(existing) = session.items.iter_mut().find(|i| i.id == item.id) {
                *existing = item;
            }
        }
    }

    pub fn toggle_expanded(&mut self, id: u32) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
    }

    pub fn visible_item_indices(&self) -> Vec<usize> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        session
            .items
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| {
                if self.hide_closed && item.status != ReviewStatus::Todo {
                    None
                } else {
                    Some(idx)
                }
            })
            .collect()
    }

    pub fn selected_item_index(&self) -> Option<usize> {
        let visible = self.visible_item_indices();
        visible.get(self.selected_index).copied()
    }

    pub fn clamp_selection(&mut self) {
        let visible_len = self.visible_item_indices().len();
        if visible_len == 0 {
            self.selected_index = 0;
        } else if self.selected_index >= visible_len {
            self.selected_index = visible_len - 1;
        }
    }

    pub fn toggle_hide_closed(&mut self) {
        let current_id = self
            .selected_item_index()
            .and_then(|idx| self.session.as_ref()?.items.get(idx))
            .map(|i| i.id);
        self.hide_closed = !self.hide_closed;
        let visible = self.visible_item_indices();
        if let Some(id) = current_id {
            if let Some(new_pos) = visible.iter().position(|&idx| {
                self.session
                    .as_ref()
                    .and_then(|s| s.items.get(idx))
                    .map(|i| i.id)
                    == Some(id)
            }) {
                self.selected_index = new_pos;
                return;
            }
        }
        self.clamp_selection();
    }

    pub fn scroll_to_selected(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        let visible = self.visible_item_indices();
        if visible.is_empty() || self.view_height == 0 {
            return;
        }
        let sel = self.selected_index.min(visible.len() - 1);
        self.selected_index = sel;
        let (lines, offsets) = compute_item_lines(
            &session.items,
            &visible,
            sel,
            &self.expanded,
            self.content_width.max(10),
        );
        let height = self.view_height as usize;
        let start = offsets.get(sel).copied().unwrap_or(0);
        let end = offsets.get(sel + 1).copied().unwrap_or(lines.len());
        let scroll = self.scroll as usize;
        let new_scroll = if start < scroll {
            start
        } else if end > scroll + height {
            end.saturating_sub(height).min(start)
        } else {
            scroll
        };
        self.scroll = new_scroll as u16;
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
        ReviewStatus::Todo => Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
        ReviewStatus::Done => Style::default().bg(Color::Green).fg(Color::Black).bold(),
        ReviewStatus::Rejected => Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
    }
}

fn location(item: &ReviewItem) -> Option<String> {
    item.file_path.as_ref().map(|path| match item.line_range {
        Some((a, b)) if a == b => format!("{}:{}", path, a),
        Some((a, b)) => format!("{}:{}-{}", path, a, b),
        None => path.clone(),
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

fn compute_item_lines(
    items: &[ReviewItem],
    visible_indices: &[usize],
    selected_vis_idx: usize,
    expanded: &HashSet<u32>,
    width: usize,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut all_lines = Vec::new();
    let mut offsets = Vec::with_capacity(visible_indices.len());

    for (vis_idx, &raw_idx) in visible_indices.iter().enumerate() {
        let item = &items[raw_idx];
        let is_selected = vis_idx == selected_vis_idx;
        let is_expanded = expanded.contains(&item.id);

        let cursor_style = if is_selected {
            Style::default().fg(Color::Cyan).bold()
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let title_style = if item.status == ReviewStatus::Todo {
            Style::default().bold()
        } else {
            Style::default().fg(Color::Gray)
        };

        let raw_title = item.title.replace(['\r', '\n'], " ");
        let (display_title, extra_path) = split_title_and_file_path(&raw_title);
        let spans = vec![
            Span::styled(if is_selected { "● " } else { "  " }, cursor_style),
            Span::styled(
                if is_expanded { "▼ " } else { "▶ " },
                Style::default().fg(Color::Yellow).bold(),
            ),
            Span::styled(
                if item.is_checked { "[x] " } else { "[ ] " },
                Style::default().fg(Color::Green).bold(),
            ),
            Span::styled(format!(" {} ", item.status), status_style(item.status)),
            Span::raw(" "),
            Span::styled(format!("[{}]", item.severity), severity_style(item.severity)),
            Span::raw(" "),
            Span::styled(format!("#{} {}", item.id, display_title), title_style),
        ];

        let mut lines = vec![Line::from(spans)];
        if is_expanded {
            let loc_str = location(item).or_else(|| extra_path.map(|s| s.to_string()));
            if let Some(loc) = loc_str {
                let clean_loc = loc.trim();
                if !clean_loc.is_empty() {
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled("File: ", Style::default().fg(Color::Cyan).bold()),
                        Span::styled(clean_loc.to_string(), Style::default().fg(Color::White)),
                    ]));
                    lines.push(Line::raw(""));
                }
            }
            let clean_body = item.body.trim();
            if !clean_body.is_empty() {
                let mut theme = crate::markdown::MarkdownTheme::answer();
                theme.line_prefix = Some(Span::raw("    "));
                let md = crate::markdown::render_markdown(clean_body, &theme);
                if !md.is_empty() {
                    lines.extend(md);
                    lines.push(Line::raw(""));
                }
            }
        }

        offsets.push(all_lines.len());
        all_lines.extend(wrap_lines(lines, width));
    }

    (all_lines, offsets)
}

fn markdown_lines(text: &str) -> Vec<Line<'static>> {
    crate::markdown::render_markdown(text, &crate::markdown::MarkdownTheme::answer())
}

fn streaming_lines(review: &ReviewViewState, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        " Reviewing... (Esc to cancel, Ctrl+R to fold/unfold thinking)",
        Style::default().fg(Color::Yellow).bold(),
    ))];
    if !review.reasoning.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(review.reasoning.render_lines());
        lines.push(Line::raw(""));
    }
    if !review.content.is_empty() {
        lines.push(Line::from(Span::styled(
            " Draft:",
            Style::default().fg(Color::Green).bold(),
        )));
        lines.extend(markdown_lines(&review.content));
    }
    wrap_lines(lines, width)
}

fn build_lines(review: &ReviewViewState, width: usize) -> Vec<Line<'static>> {
    if review.running {
        return streaming_lines(review, width);
    }
    match &review.session {
        Some(session) if !session.items.is_empty() => {
            let visible = review.visible_item_indices();
            if visible.is_empty() {
                vec![
                    Line::raw(""),
                    Line::from(Span::styled(
                        "  All findings are closed (DONE / REJECTED).",
                        Style::default().fg(Color::DarkGray).bold(),
                    )),
                    Line::from(Span::styled(
                        "  Press 's' to toggle filter and show all findings.",
                        Style::default().fg(Color::Yellow),
                    )),
                ]
            } else {
                compute_item_lines(
                    &session.items,
                    &visible,
                    review.selected_index,
                    &review.expanded,
                    width,
                )
                .0
            }
        }
        Some(session) => {
            let mut lines = vec![Line::from(Span::styled(
                " No structured findings were parsed; showing the raw review:",
                Style::default().fg(Color::Yellow),
            ))];
            lines.extend(markdown_lines(&session.raw_markdown));
            wrap_lines(lines, width)
        }
        None => vec![
            Line::raw(""),
            Line::from(Span::styled(
                "  No review yet.",
                Style::default().fg(Color::DarkGray).bold(),
            )),
            Line::from(Span::styled(
                "  Add files to the Context (Ctrl+2), then press 'r' to run a code review.",
                Style::default().fg(Color::DarkGray),
            )),
        ],
    }
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

    let items_len = state
        .review
        .session
        .as_ref()
        .map(|s| s.items.len())
        .unwrap_or(0);
    let visible_indices = state.review.visible_item_indices();
    let visible_len = visible_indices.len();
    if visible_len > 0 && state.review.selected_index >= visible_len {
        state.review.selected_index = visible_len - 1;
    }

    let lines = build_lines(&state.review, width);
    state.review.rendered_lines = lines.len();
    let max_scroll = (lines.len() as u16).saturating_sub(height);
    if state.review.running || state.review.scroll > max_scroll {
        state.review.scroll = max_scroll;
    }

    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((state.review.scroll, 0));
    frame.render_widget(paragraph, main_area);

    let checked = state
        .review
        .session
        .as_ref()
        .map(|s| s.items.iter().filter(|i| i.is_checked).count())
        .unwrap_or(0);

    let status_line = if state.review.running {
        Line::from(vec![
            Span::styled(" Reviewing... ", Style::default().fg(Color::Yellow).bold()),
            Span::styled("[Esc: Cancel]", Style::default().fg(Color::Gray)),
        ])
    } else if let Some(err) = &state.review.error {
        Line::from(Span::styled(
            format!(" {}", err),
            Style::default().fg(Color::Red).bold(),
        ))
    } else if let Some(session) = &state.review.session {
        let filter_tag = if state.review.hide_closed {
            " | Filter: Open only"
        } else {
            ""
        };
        let item_pos = if visible_len > 0 {
            if state.review.hide_closed && visible_len < items_len {
                format!(" | [Item {}/{} ({} total)]", state.review.selected_index + 1, visible_len, items_len)
            } else {
                format!(" | [Item {}/{}]", state.review.selected_index + 1, items_len)
            }
        } else if items_len > 0 && state.review.hide_closed {
            format!(" | [0/{} visible (all closed)]", items_len)
        } else {
            String::new()
        };
        Line::from(Span::styled(
            format!(
                " Review {} | {} | {} item(s), {} checked for Develop{}{}",
                session.id, session.model, items_len, checked, filter_tag, item_pos
            ),
            Style::default().fg(Color::Gray),
        ))
    } else {
        Line::raw("")
    };

    let info = Paragraph::new(vec![status_line])
        .block(Block::default().padding(Padding::horizontal(1)));
    frame.render_widget(info, info_area);
}
