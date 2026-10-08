use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use tauqe_protocol::{matches_glob_pattern, ContextAccess, ContextItem, ContextLayer};

use crate::app::AppState;
use crate::ui::centered_rect;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextRow {
    Header(ContextLayer),
    Item(ContextItem),
}

#[derive(Debug, Clone)]
pub struct ContextViewState {
    pub cursor_index: usize,
    pub scroll: u16,
    pub pinned_expanded: bool,
    pub user_expanded: bool,
    pub auto_expanded: bool,
    pub adding_file: bool,
    pub add_access: ContextAccess,
    pub add_input: String,
    pub filtered_candidates: Vec<String>,
    pub total_matching: usize,
    pub selected_candidate_index: usize,
    pub status_message: Option<String>,
    pub confirm_clear_auto: bool,
}

impl Default for ContextViewState {
    fn default() -> Self {
        Self {
            cursor_index: 0,
            scroll: 0,
            pinned_expanded: true,
            user_expanded: true,
            auto_expanded: true,
            adding_file: false,
            add_access: ContextAccess::ReadOnly,
            add_input: String::new(),
            filtered_candidates: Vec::new(),
            total_matching: 0,
            selected_candidate_index: 0,
            status_message: None,
            confirm_clear_auto: false,
        }
    }
}

impl ContextViewState {
    pub fn compute_rows(&self, items: &[ContextItem]) -> Vec<ContextRow> {
        let mut rows = Vec::new();

        rows.push(ContextRow::Header(ContextLayer::Pinned));
        if self.pinned_expanded {
            for it in items.iter().filter(|i| i.layer == ContextLayer::Pinned) {
                rows.push(ContextRow::Item(it.clone()));
            }
        }

        rows.push(ContextRow::Header(ContextLayer::User));
        if self.user_expanded {
            for it in items.iter().filter(|i| i.layer == ContextLayer::User) {
                rows.push(ContextRow::Item(it.clone()));
            }
        }

        rows.push(ContextRow::Header(ContextLayer::Auto));
        if self.auto_expanded {
            for it in items.iter().filter(|i| i.layer == ContextLayer::Auto) {
                rows.push(ContextRow::Item(it.clone()));
            }
        }

        rows
    }

    pub fn toggle_section(&mut self, layer: ContextLayer) {
        match layer {
            ContextLayer::Pinned => self.pinned_expanded = !self.pinned_expanded,
            ContextLayer::User => self.user_expanded = !self.user_expanded,
            ContextLayer::Auto => self.auto_expanded = !self.auto_expanded,
        }
    }

    pub fn update_filtered_candidates(
        &mut self,
        items: &[ContextItem],
        all_repo_files: &[String],
    ) {
        let query = self.add_input.trim();
        let existing: std::collections::HashSet<&str> =
            items.iter().map(|it| it.path.as_str()).collect();

        let mut candidates = Vec::new();

        if !query.is_empty() {
            let is_pattern_query = query.contains('*')
                || query.contains('?')
                || query.ends_with('/')
                || !query.contains('.');

            let matching_pattern_count = all_repo_files
                .iter()
                .filter(|f| !existing.contains(f.as_str()) && matches_glob_pattern(query, f))
                .count();

            if matching_pattern_count > 0 && is_pattern_query {
                candidates.push(format!(
                    "[+] Add all matching '{}' ({} files)",
                    query, matching_pattern_count
                ));
            }

            let query_lower = query.to_lowercase();
            let all_matches: Vec<String> = all_repo_files
                .iter()
                .filter(|f| !existing.contains(f.as_str()))
                .filter(|f| {
                    f.to_lowercase().contains(&query_lower) || matches_glob_pattern(query, f)
                })
                .cloned()
                .collect();

            self.total_matching = all_matches.len();
            candidates.extend(all_matches.into_iter().take(50));
        } else {
            let available: Vec<String> = all_repo_files
                .iter()
                .filter(|f| !existing.contains(f.as_str()))
                .cloned()
                .collect();
            self.total_matching = available.len();
            candidates = available.into_iter().take(50).collect();
        }

        self.filtered_candidates = candidates;

        if self.filtered_candidates.is_empty() {
            self.selected_candidate_index = 0;
        } else if self.selected_candidate_index >= self.filtered_candidates.len() {
            self.selected_candidate_index = self.filtered_candidates.len() - 1;
        }
    }
}

pub fn render_context_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    main_area: Rect,
    info_area: Rect,
) {
    let mut context_lines: Vec<Line> = Vec::new();

    let pinned_count = state.context.items.iter().filter(|i| i.layer == ContextLayer::Pinned).count();
    let user_count = state.context.items.iter().filter(|i| i.layer == ContextLayer::User).count();
    let auto_count = state.context.items.iter().filter(|i| i.layer == ContextLayer::Auto).count();

    context_lines.push(Line::from(vec![
        Span::raw("Total size: "),
        Span::styled(
            format!("~{} tokens", state.context.total_estimated_tokens),
            Style::default().fg(Color::Cyan).bold(),
        ),
        Span::raw(format!(
            " | Files: {} (Pinned: {}, User: {}, Auto: {})",
            state.context.items.len(),
            pinned_count,
            user_count,
            auto_count
        )),
        Span::raw(format!(" | Revision: #{}", state.context.revision)),
    ]));
    context_lines.push(Line::raw(""));

    let rows = state.context_view.compute_rows(&state.context.items);
    let active_row = rows.get(state.context_view.cursor_index).cloned();

    for (idx, row) in rows.iter().enumerate() {
        let is_selected = idx == state.context_view.cursor_index;
        match row {
            ContextRow::Header(layer) => {
                let (arrow, name, count, tokens, style_color, hint) = match layer {
                    ContextLayer::Pinned => {
                        let arrow = if state.context_view.pinned_expanded { "▼" } else { "▶" };
                        let tokens: u64 = state.context.items.iter()
                            .filter(|i| i.layer == ContextLayer::Pinned)
                            .map(|i| i.estimated_tokens)
                            .sum();
                        (arrow, "Pinned Files", pinned_count, tokens, Color::Cyan, " [Protected / Config]")
                    }
                    ContextLayer::User => {
                        let arrow = if state.context_view.user_expanded { "▼" } else { "▶" };
                        let tokens: u64 = state.context.items.iter()
                            .filter(|i| i.layer == ContextLayer::User)
                            .map(|i| i.estimated_tokens)
                            .sum();
                        (arrow, "User Files", user_count, tokens, Color::Yellow, " [Editable / Read-Only]")
                    }
                    ContextLayer::Auto => {
                        let arrow = if state.context_view.auto_expanded { "▼" } else { "▶" };
                        let tokens: u64 = state.context.items.iter()
                            .filter(|i| i.layer == ContextLayer::Auto)
                            .map(|i| i.estimated_tokens)
                            .sum();
                        (arrow, "Auto Files", auto_count, tokens, Color::Magenta, " [Model Requested]")
                    }
                };

                let prefix = if is_selected { "● " } else { "  " };
                let line_style = if is_selected {
                    Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                let header_line = Line::from(vec![
                    Span::styled(prefix, Style::default().fg(style_color).bold()),
                    Span::styled(format!("{} {} ", arrow, name), Style::default().fg(style_color).bold()),
                    Span::styled(
                        format!("({} files, ~{} tokens)", count, tokens),
                        Style::default().fg(Color::White).bold(),
                    ),
                    Span::styled(hint, Style::default().fg(Color::DarkGray)),
                ]).style(line_style);

                context_lines.push(header_line);
            }
            ContextRow::Item(item) => {
                let cursor_prefix = if is_selected { "   ▶ " } else { "     " };

                let (access_badge, access_style) = match item.access {
                    ContextAccess::Editable => {
                        ("[EDITABLE] ", Style::default().fg(Color::Yellow).bold())
                    }
                    ContextAccess::ReadOnly => {
                        ("[READ-ONLY]", Style::default().fg(Color::Green))
                    }
                };

                let line_style = if is_selected {
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                let item_line = Line::from(vec![
                    Span::styled(cursor_prefix, Style::default().fg(Color::Cyan)).bold(),
                    Span::styled(access_badge, access_style),
                    Span::raw(" "),
                    Span::styled(&item.path, Style::default().bold()),
                    Span::styled(
                        format!(
                            "  (~{} tokens, {} B)",
                            item.estimated_tokens, item.size_bytes
                        ),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
                .style(line_style);

                context_lines.push(item_line);
            }
        }
    }

    let inner_height = main_area.height as usize;
    if inner_height > 0 {
        let target_line = state.context_view.cursor_index + 2;
        if target_line < state.context_view.scroll as usize {
            state.context_view.scroll = target_line as u16;
        } else if target_line >= (state.context_view.scroll as usize + inner_height) {
            state.context_view.scroll = (target_line + 1).saturating_sub(inner_height) as u16;
        }
        let max_scroll = context_lines.len().saturating_sub(inner_height) as u16;
        state.context_view.scroll = state.context_view.scroll.min(max_scroll);
    }

    let ctx_paragraph = Paragraph::new(context_lines)
        .block(Block::default().padding(Padding::horizontal(1)))
        .scroll((state.context_view.scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(ctx_paragraph, main_area);

    let info_line = if let Some(msg) = &state.context_view.status_message {
        Line::from(Span::styled(msg, Style::default().fg(Color::Green).bold()))
    } else {
        match active_row {
            Some(ContextRow::Header(ContextLayer::Pinned)) => Line::from(Span::styled(
                "Pinned layer: Architecture & reference documents specified in tauqe.toml",
                Style::default().fg(Color::Cyan),
            )),
            Some(ContextRow::Header(ContextLayer::User)) => Line::from(Span::styled(
                "User layer: Persistent files curated by the developer (Editable or Read-Only)",
                Style::default().fg(Color::Yellow),
            )),
            Some(ContextRow::Header(ContextLayer::Auto)) => Line::from(Span::styled(
                "Auto layer: Files autonomously requested by the model during discovery rounds",
                Style::default().fg(Color::Magenta),
            )),
            Some(ContextRow::Item(ref it)) => match it.layer {
                ContextLayer::Pinned => Line::from(Span::styled(
                    format!("Pinned document: {} (protected, ~{} tokens)", it.path, it.estimated_tokens),
                    Style::default().fg(Color::Cyan),
                )),
                ContextLayer::User => Line::from(Span::styled(
                    format!("User file: {} [{:?}], ~{} tokens", it.path, it.access, it.estimated_tokens),
                    Style::default().fg(Color::White),
                )),
                ContextLayer::Auto => Line::from(Span::styled(
                    format!("Auto-discovered file: {} [{:?}], ~{} tokens", it.path, it.access, it.estimated_tokens),
                    Style::default().fg(Color::Magenta),
                )),
            },
            None => Line::from(Span::styled(
                "Context state: 3 layers (Pinned, User, Auto) | Press '?' for commands",
                Style::default().fg(Color::DarkGray),
            )),
        }
    };
    let prompt_widget = Paragraph::new(info_line)
        .block(Block::default().padding(Padding::horizontal(1)));
    frame.render_widget(prompt_widget, info_area);

    if state.context_view.adding_file {
        render_add_file_picker(frame, state);
    }
}

fn render_add_file_picker(frame: &mut ratatui::Frame, state: &AppState) {
    let area = centered_rect(70, 50, frame.area());
    frame.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    let input_line = Line::from(vec![
        Span::styled(" File / Pattern: ", Style::default().fg(Color::Cyan).bold()),
        Span::raw(&state.context_view.add_input),
        Span::styled("█", Style::default().fg(Color::Yellow)),
    ]);
    let (access_title, border_color) = match state.context_view.add_access {
        ContextAccess::Editable => (
            " Add EDITABLE to Context (Path, directory or glob like *.rs) ",
            Color::Yellow,
        ),
        ContextAccess::ReadOnly => (
            " Add READ-ONLY to Context (Path, directory or glob like *.rs) ",
            Color::Cyan,
        ),
    };
    let input_block = Paragraph::new(input_line).block(
        Block::default()
            .title(access_title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color)),
    );
    frame.render_widget(input_block, chunks[0]);

    let mut candidate_lines = Vec::new();
    let visible_rows = chunks[1].height.saturating_sub(2) as usize;
    let selected = state.context_view.selected_candidate_index;
    let total_candidates = state.context_view.filtered_candidates.len();

    if state.all_repo_files.is_empty() {
        candidate_lines.push(Line::from(Span::styled(
            "  Loading repository files from server...",
            Style::default().fg(Color::Yellow).italic(),
        )));
    } else if total_candidates == 0 {
        candidate_lines.push(Line::from(Span::styled(
            "  No matching files found.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let max_visible = visible_rows.max(1);
        let start_idx = if selected < max_visible {
            0
        } else {
            selected + 1 - max_visible
        };
        let end_idx = (start_idx + max_visible).min(total_candidates);

        for idx in start_idx..end_idx {
            let candidate = &state.context_view.filtered_candidates[idx];
            let is_sel = idx == selected;
            let (prefix, style) = if is_sel {
                (
                    " ▶ ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                )
            } else {
                ("   ", Style::default().fg(Color::Gray))
            };

            let is_pattern_entry = candidate.starts_with("[+] Add all matching '");

            let line = if is_pattern_entry {
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Green).bold()),
                    Span::styled(
                        candidate,
                        if is_sel {
                            Style::default().bg(Color::Green).fg(Color::Black).bold()
                        } else {
                            Style::default().fg(Color::Green).bold()
                        },
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Cyan)).bold(),
                    Span::styled(candidate, style),
                ])
            };

            candidate_lines.push(line);
        }
    }

    let count_info = if state.all_repo_files.is_empty() {
        "loading...".to_string()
    } else if state.context_view.total_matching > total_candidates {
        format!(
            "showing {} of {} matching",
            total_candidates, state.context_view.total_matching
        )
    } else {
        format!("{} found", total_candidates)
    };

    let list_title = match state.context_view.add_access {
        ContextAccess::Editable => format!(
            " Matching Files ({}) [Enter: Add EDITABLE, Tab: Complete, ↑/↓: Navigate, Esc: Cancel] ",
            count_info
        ),
        ContextAccess::ReadOnly => format!(
            " Matching Files ({}) [Enter: Add READ-ONLY, Tab: Complete, ↑/↓: Navigate, Esc: Cancel] ",
            count_info
        ),
    };
    let list_block = Paragraph::new(candidate_lines).block(
        Block::default()
            .title(list_title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(list_block, chunks[1]);
}
