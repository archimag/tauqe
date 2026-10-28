use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use tauqe_protocol::{ContextAccess, ContextLayer};

use crate::app::AppState;
use crate::i18n::active_t;
use crate::ui::centered_rect;

use super::{ContextCandidate, ContextRow};

pub fn render_context_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    main_area: Rect,
    info_area: Rect,
) {
    let engine = crate::theme::engine::current_engine();
    let palette = engine.palette();
    let mut context_lines: Vec<Line> = Vec::new();

    let pinned_count = state
        .context
        .items
        .iter()
        .filter(|i| i.layer == ContextLayer::Pinned)
        .count();
    let user_count = state
        .context
        .items
        .iter()
        .filter(|i| i.layer == ContextLayer::User)
        .count();
    let auto_count = state
        .context
        .items
        .iter()
        .filter(|i| i.layer == ContextLayer::Auto)
        .count();
    let refs_count = state
        .context
        .items
        .iter()
        .filter(|i| i.layer == ContextLayer::Refs)
        .count();

    context_lines.push(Line::from(vec![
        Span::raw(format!("{} ", active_t("context.total_size"))),
        Span::styled(
            format!("~{} tokens", state.context.total_estimated_tokens),
            engine.fg(palette.status_info).bold(),
        ),
        Span::raw(format!(
            " | Files: {} (Pinned: {}, Refs: {}, User: {}, Auto: {})",
            state.context.items.len(),
            pinned_count,
            refs_count,
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
                let (arrow, name, count, tokens, role, hint) = match layer {
                    ContextLayer::Pinned => {
                        let arrow = if state.context_view.pinned_expanded {
                            "▼"
                        } else {
                            "▶"
                        };
                        let tokens: u64 = state
                            .context
                            .items
                            .iter()
                            .filter(|i| i.layer == ContextLayer::Pinned)
                            .map(|i| i.estimated_tokens)
                            .sum();
                        (
                            arrow,
                            active_t("context.section.pinned"),
                            pinned_count,
                            tokens,
                            palette.status_info,
                            active_t("context.section.pinned_hint"),
                        )
                    }
                    ContextLayer::Refs => {
                        let arrow = if state.context_view.refs_expanded {
                            "▼"
                        } else {
                            "▶"
                        };
                        let tokens: u64 = state
                            .context
                            .items
                            .iter()
                            .filter(|i| i.layer == ContextLayer::Refs)
                            .map(|i| i.estimated_tokens)
                            .sum();
                        (
                            arrow,
                            active_t("context.section.refs"),
                            refs_count,
                            tokens,
                            palette.status_success,
                            active_t("context.section.refs_hint"),
                        )
                    }
                    ContextLayer::User => {
                        let arrow = if state.context_view.user_expanded {
                            "▼"
                        } else {
                            "▶"
                        };
                        let tokens: u64 = state
                            .context
                            .items
                            .iter()
                            .filter(|i| i.layer == ContextLayer::User)
                            .map(|i| i.estimated_tokens)
                            .sum();
                        (
                            arrow,
                            active_t("context.section.user"),
                            user_count,
                            tokens,
                            palette.status_warning,
                            active_t("context.section.user_hint"),
                        )
                    }
                    ContextLayer::Auto => {
                        let arrow = if state.context_view.auto_expanded {
                            "▼"
                        } else {
                            "▶"
                        };
                        let tokens: u64 = state
                            .context
                            .items
                            .iter()
                            .filter(|i| i.layer == ContextLayer::Auto)
                            .map(|i| i.estimated_tokens)
                            .sum();
                        (
                            arrow,
                            active_t("context.section.auto"),
                            auto_count,
                            tokens,
                            palette.button_active_bg,
                            active_t("context.section.auto_hint"),
                        )
                    }
                };

                let prefix = if is_selected { "● " } else { "  " };
                let line_style = if is_selected {
                    engine.bg(palette.overlay).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                let header_line = Line::from(vec![
                    Span::styled(prefix, engine.fg(role).bold()),
                    Span::styled(format!("{} {} ", arrow, name), engine.fg(role).bold()),
                    Span::styled(
                        format!("({} files, ~{} tokens)", count, tokens),
                        engine.fg(palette.text_primary).bold(),
                    ),
                    Span::styled(hint, engine.fg(palette.text_muted)),
                ])
                .style(line_style);

                context_lines.push(header_line);
            }
            ContextRow::Item(item) => {
                let cursor_prefix = if is_selected { "   ▶ " } else { "     " };

                let (access_badge, access_style) = match item.access {
                    ContextAccess::Editable => (
                        format!("{} ", active_t("context.badge.editable")),
                        engine.fg(palette.status_warning).bold(),
                    ),
                    ContextAccess::ReadOnly => (
                        active_t("context.badge.readonly"),
                        engine.fg(palette.status_success),
                    ),
                };

                let line_style = if is_selected {
                    engine.bg(palette.overlay).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                let item_line = Line::from(vec![
                    Span::styled(cursor_prefix, engine.fg(palette.status_info)).bold(),
                    Span::styled(access_badge, access_style),
                    Span::raw(" "),
                    Span::styled(&item.path, Style::default().bold()),
                    Span::styled(
                        format!(
                            "  (~{} tokens, {} B)",
                            item.estimated_tokens, item.size_bytes
                        ),
                        engine.fg(palette.text_muted),
                    ),
                ])
                .style(line_style);

                context_lines.push(item_line);
            }
        }
    }

    let inner_height = main_area.height as usize;
    state.context_view.view_height = inner_height;
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

    let info_line = match active_row {
        Some(ContextRow::Header(ContextLayer::Pinned)) => Line::from(Span::styled(
            active_t("context.info.pinned"),
            engine.fg(palette.status_info),
        )),
        Some(ContextRow::Header(ContextLayer::Refs)) => Line::from(Span::styled(
            active_t("context.info.refs"),
            engine.fg(palette.status_success),
        )),
        Some(ContextRow::Header(ContextLayer::User)) => Line::from(Span::styled(
            active_t("context.info.user"),
            engine.fg(palette.status_warning),
        )),
        Some(ContextRow::Header(ContextLayer::Auto)) => Line::from(Span::styled(
            active_t("context.info.auto"),
            engine.fg(palette.button_active_bg),
        )),
        Some(ContextRow::Item(ref it)) => match it.layer {
            ContextLayer::Pinned => Line::from(Span::styled(
                active_t("context.info.pinned_item")
                    .replacen("{}", it.path.as_str(), 1)
                    .replacen("{}", &it.estimated_tokens.to_string(), 1),
                engine.fg(palette.status_info),
            )),
            ContextLayer::Refs => Line::from(Span::styled(
                active_t("context.info.refs_item")
                    .replacen("{}", it.path.as_str(), 1)
                    .replacen("{}", &it.estimated_tokens.to_string(), 1),
                engine.fg(palette.status_success),
            )),
            ContextLayer::User => {
                let badge = match it.access {
                    ContextAccess::Editable => active_t("context.badge.editable"),
                    ContextAccess::ReadOnly => active_t("context.badge.readonly"),
                };
                Line::from(Span::styled(
                    active_t("context.info.user_item")
                        .replacen("{}", it.path.as_str(), 1)
                        .replacen("{}", badge.as_str(), 1)
                        .replacen("{}", &it.estimated_tokens.to_string(), 1),
                    engine.fg(palette.text_primary),
                ))
            }
            ContextLayer::Auto => {
                let badge = match it.access {
                    ContextAccess::Editable => active_t("context.badge.editable"),
                    ContextAccess::ReadOnly => active_t("context.badge.readonly"),
                };
                Line::from(Span::styled(
                    active_t("context.info.auto_item")
                        .replacen("{}", it.path.as_str(), 1)
                        .replacen("{}", badge.as_str(), 1)
                        .replacen("{}", &it.estimated_tokens.to_string(), 1),
                    engine.fg(palette.button_active_bg),
                ))
            }
        },
        None => Line::from(Span::styled(
            active_t("context.info.default"),
            engine.fg(palette.text_muted),
        )),
    };
    let prompt_widget =
        Paragraph::new(info_line).block(Block::default().padding(Padding::horizontal(1)));
    frame.render_widget(prompt_widget, info_area);

    if state.context_view.adding_file {
        render_add_file_picker(frame, state);
    }
}

fn render_add_file_picker(frame: &mut ratatui::Frame, state: &AppState) {
    let engine = crate::theme::engine::current_engine();
    let palette = engine.palette();
    let area = centered_rect(70, 50, frame.area());
    frame.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    let input_line = Line::from(vec![
        Span::styled(
            format!(" {} ", active_t("context.picker.file_pattern")),
            engine.fg(palette.status_info).bold(),
        ),
        Span::raw(state.context_view.add_input.get_text()),
        Span::styled("█", engine.fg(palette.status_warning)),
    ]);
    let (access_title, border_style) = match state.context_view.add_access {
        ContextAccess::Editable => (
            format!(" {} ", active_t("context.picker.title_editable")),
            engine.fg(palette.status_warning),
        ),
        ContextAccess::ReadOnly => (
            format!(" {} ", active_t("context.picker.title_readonly")),
            engine.fg(palette.status_info),
        ),
    };
    let input_block = Paragraph::new(input_line).block(
        Block::default()
            .title(access_title)
            .borders(Borders::ALL)
            .border_style(border_style)
            .style(Style::default().bg(engine.color(palette.surface))),
    );
    frame.render_widget(input_block, chunks[0]);

    let mut candidate_lines = Vec::new();
    let visible_rows = chunks[1].height.saturating_sub(2) as usize;
    let selected = state.context_view.selected_candidate_index;
    let total_candidates = state.context_view.filtered_candidates.len();

    if state.all_repo_files.is_empty() {
        candidate_lines.push(Line::from(Span::styled(
            format!("  {}", active_t("context.picker.loading")),
            engine.fg(palette.status_warning).italic(),
        )));
    } else if total_candidates == 0 {
        candidate_lines.push(Line::from(Span::styled(
            format!("  {}", active_t("context.picker.no_match")),
            engine.fg(palette.text_muted),
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
            let prefix = if is_sel { " ▶ " } else { "   " };

            let line = match candidate {
                ContextCandidate::BulkPattern { .. } => Line::from(vec![
                    Span::styled(prefix, engine.fg(palette.status_success).bold()),
                    Span::styled(
                        candidate.display_label(),
                        if is_sel {
                            engine
                                .bg(palette.status_success)
                                .fg(engine.color(palette.text_inverse))
                                .bold()
                        } else {
                            engine.fg(palette.status_success).bold()
                        },
                    ),
                ]),
                ContextCandidate::File(path) => {
                    let path_style = if is_sel {
                        engine
                            .bg(palette.overlay)
                            .fg(engine.color(palette.text_primary))
                            .bold()
                    } else {
                        engine.fg(palette.text_secondary)
                    };
                    Line::from(vec![
                        Span::styled(prefix, engine.fg(palette.status_info).bold()),
                        Span::styled(path.as_str(), path_style),
                    ])
                }
            };

            candidate_lines.push(line);
        }
    }

    let count_info = if state.all_repo_files.is_empty() {
        active_t("context.picker.count_loading")
    } else if state.context_view.total_matching > total_candidates {
        active_t("context.picker.count_partial")
            .replacen("{}", &total_candidates.to_string(), 1)
            .replacen("{}", &state.context_view.total_matching.to_string(), 1)
    } else {
        active_t("context.picker.count_found").replacen("{}", &total_candidates.to_string(), 1)
    };

    let list_title = match state.context_view.add_access {
        ContextAccess::Editable => format!(
            " {} ",
            active_t("context.picker.list_title_editable").replacen("{}", &count_info, 1)
        ),
        ContextAccess::ReadOnly => format!(
            " {} ",
            active_t("context.picker.list_title_readonly").replacen("{}", &count_info, 1)
        ),
    };
    let list_block = Paragraph::new(candidate_lines).block(
        Block::default()
            .title(list_title)
            .borders(Borders::ALL)
            .border_style(engine.fg(palette.text_muted))
            .style(Style::default().bg(engine.color(palette.surface))),
    );
    frame.render_widget(list_block, chunks[1]);
}
