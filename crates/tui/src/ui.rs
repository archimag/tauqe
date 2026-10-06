use std::path::Path;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use workbench_protocol::ContextAccess;

use crate::app::{AppState, ViewMode};
use crate::model_view::compute_model_lines;

pub fn render_ui(frame: &mut ratatui::Frame, state: &mut AppState) {
    let term_height = frame.area().height;
    let max_input_height = (term_height * 4 / 10).clamp(6, 16);
    let needed_input_height = (state.input_editor.line_count() as u16 + 2).max(3);
    let input_height = needed_input_height.min(max_input_height);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(input_height),
            Constraint::Length(3),
        ])
        .split(frame.area());

    // Top Header
    let (project_name, dirty_status) = match &state.repo_state {
        Some(repo) => {
            let name = Path::new(&repo.root)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&repo.root);
            let dirty = if repo.dirty {
                Span::styled(" [DIRTY]", Style::default().fg(Color::Yellow).bold())
            } else {
                Span::styled(" [CLEAN]", Style::default().fg(Color::Green))
            };
            (name.to_string(), dirty)
        }
        None => ("no repository".to_string(), Span::raw("")),
    };

    let model_tab_style = if state.view_mode == ViewMode::Model {
        Style::default().bg(Color::Blue).fg(Color::White).bold()
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let context_tab_style = if state.view_mode == ViewMode::Context {
        Style::default().bg(Color::Blue).fg(Color::White).bold()
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let header_line = Line::from(vec![
        Span::styled(" WORKBENCH ", Style::default().bg(Color::Cyan).fg(Color::Black).bold()),
        Span::raw("  "),
        Span::styled(" 1: Model ", model_tab_style),
        Span::raw(" "),
        Span::styled(
            format!(" 2: Context ({}) ", state.context.items.len()),
            context_tab_style,
        ),
        Span::raw(" | Project: "),
        Span::styled(project_name, Style::default().bold()),
        dirty_status,
        Span::raw("  | Wf: "),
        Span::styled(&state.workflow, Style::default().fg(Color::Cyan).bold()),
        Span::raw("  Proto: "),
        Span::styled(&state.edit_protocol, Style::default().fg(Color::Magenta).bold()),
        Span::raw("  Model: "),
        Span::styled(
            if state.active_model.is_empty() { "-" } else { state.active_model.as_str() },
            Style::default().fg(Color::Green).bold(),
        ),
        Span::raw("  | Session: "),
        Span::styled(
            format!("${:.5}", state.model.session_total_cost),
            if state.model.session_total_cost > 0.0 {
                Style::default().fg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::raw("  Last: "),
        Span::styled(
            match state.model.last_op_cost {
                Some(cost) => format!("${:.5}", cost),
                None => "-".to_string(),
            },
            if state.model.last_op_cost.unwrap_or(0.0) > 0.0 {
                Style::default().fg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
    ]);

    let header = Paragraph::new(header_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    match state.view_mode {
        ViewMode::Model => {
            let content_height = chunks[1].height.saturating_sub(2);
            state.last_model_height = content_height;

            let model_lines = compute_model_lines(&state.model);
            let total_lines = model_lines.len() as u16;
            let max_scroll = total_lines.saturating_sub(content_height);
            if state.model.scroll > max_scroll {
                state.model.scroll = max_scroll;
            }

            let (status_text, status_style) = match state.model.status.as_str() {
                "starting" => ("Starting...", Style::default().bold().fg(Color::Yellow)),
                "streaming" => ("Streaming...", Style::default().bold().fg(Color::Yellow)),
                "done" => ("Done", Style::default().bold().fg(Color::Green)),
                "cancelled" => ("Cancelled", Style::default().bold().fg(Color::Red)),
                "error" => ("Error", Style::default().bold().fg(Color::Red)),
                _ => ("Idle", Style::default().bold().fg(Color::Gray)),
            };

            let scroll_indicator = if max_scroll > 0 {
                format!(" [{}/{}]", state.model.scroll + 1, total_lines)
            } else {
                String::new()
            };

            let title_line = Line::from(vec![
                Span::raw(" Model View: "),
                Span::styled(status_text, status_style),
                Span::styled(scroll_indicator, Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
            ]);

            let model_paragraph = Paragraph::new(model_lines)
                .block(Block::default().title(title_line).borders(Borders::ALL))
                .wrap(Wrap { trim: true })
                .scroll((state.model.scroll, 0));
            frame.render_widget(model_paragraph, chunks[1]);

            let editor = &state.input_editor;
            let text = editor.get_text();
            let mut input_lines = Vec::new();
            let (cur_line_idx, cur_col) = editor.cursor_line_col();

            if text.is_empty() {
                input_lines.push(Line::from(vec![
                    Span::styled(" > ", Style::default().fg(Color::Cyan).bold()),
                    Span::styled("█", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        " Type your prompt (Enter to send, Shift+Enter or Ctrl+J for newline, '?' for help)...",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            } else {
                let lines = editor.get_lines();

                for (l_idx, line_str) in lines.iter().enumerate() {
                    let prefix = if l_idx == 0 { " > " } else { "   " };
                    let mut spans = vec![Span::styled(prefix, Style::default().fg(Color::Cyan).bold())];

                    if l_idx == cur_line_idx {
                        let char_indices: Vec<(usize, char)> = line_str.char_indices().collect();
                        if cur_col >= char_indices.len() {
                            spans.push(Span::raw(line_str.to_string()));
                            spans.push(Span::styled("█", Style::default().fg(Color::Yellow)));
                        } else {
                            let (byte_offset, c) = char_indices[cur_col];
                            let before = &line_str[..byte_offset];
                            let char_len = c.len_utf8();
                            let after = &line_str[byte_offset + char_len..];

                            if !before.is_empty() {
                                spans.push(Span::raw(before.to_string()));
                            }
                            spans.push(Span::styled(
                                c.to_string(),
                                Style::default().bg(Color::White).fg(Color::Black).bold(),
                            ));
                            if !after.is_empty() {
                                spans.push(Span::raw(after.to_string()));
                            }
                        }
                    } else {
                        spans.push(Span::raw(line_str.to_string()));
                    }
                    input_lines.push(Line::from(spans));
                }
            }

            let visible_input_lines = chunks[2].height.saturating_sub(2);
            let input_scroll = if visible_input_lines > 0 && cur_line_idx >= visible_input_lines as usize {
                (cur_line_idx + 1 - visible_input_lines as usize) as u16
            } else {
                0
            };

            let input_title = if editor.line_count() > 1 {
                format!(
                    " Intent (Line {}/{}, Enter to send) ",
                    cur_line_idx + 1,
                    editor.line_count()
                )
            } else {
                " Intent (Enter to send, Shift+Enter / Ctrl+J for newline) ".to_string()
            };

            let input_paragraph = Paragraph::new(input_lines)
                .block(Block::default().title(input_title).borders(Borders::ALL))
                .scroll((input_scroll, 0));
            frame.render_widget(input_paragraph, chunks[2]);

            let footer_line = Line::from(vec![
                Span::styled(" Ctrl+1/2 ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Switch  "),
                Span::styled(" Ctrl+W ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Wf  "),
                Span::styled(" Ctrl+P ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Proto  "),
                Span::styled(" Ctrl+M ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Model  "),
                Span::styled(" Enter ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Send  "),
                Span::styled(" Shift+Enter / Ctrl+J ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Newline  "),
                Span::styled(" [/] ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Files  "),
                Span::styled(" Space ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Diff  "),
                Span::styled(" u ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Undo  "),
                Span::styled(" Esc ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Cancel  "),
                Span::styled(" ? ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Help"),
            ]);
            let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
            frame.render_widget(footer, chunks[3]);
        }
        ViewMode::Context => {
            let mut context_lines: Vec<Line> = Vec::new();

            context_lines.push(Line::from(vec![
                Span::raw("Total size: "),
                Span::styled(
                    format!("~{} tokens", state.context.total_estimated_tokens),
                    Style::default().fg(Color::Cyan).bold(),
                ),
                Span::raw(format!(" | Files: {}", state.context.items.len())),
                Span::raw(format!(" | Revision: #{}", state.context.revision)),
            ]));
            context_lines.push(Line::raw(""));

            if state.context.items.is_empty() {
                context_lines.push(Line::from(Span::styled(
                    "No files in context yet. Press 'a' to add files or glob patterns.",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                for (idx, item) in state.context.items.iter().enumerate() {
                    let is_selected = idx == state.context_view.cursor_index;
                    let cursor_prefix = if is_selected { " ▶ " } else { "   " };

                    let (access_badge, access_style) = match item.access {
                        ContextAccess::Editable => ("[EDITABLE] ", Style::default().fg(Color::Yellow).bold()),
                        ContextAccess::ReadOnly => ("[READ-ONLY]", Style::default().fg(Color::Green)),
                    };

                    let line_style = if is_selected {
                        Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };

                    let item_line = Line::from(vec![
                        Span::styled(cursor_prefix, Style::default().fg(Color::Cyan)).bold(),
                        Span::styled(access_badge, access_style),
                        Span::raw(" "),
                        Span::styled(&item.path, Style::default().bold()),
                        Span::styled(
                            format!("  (~{} tokens, {} B)", item.estimated_tokens, item.size_bytes),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]).style(line_style);

                    context_lines.push(item_line);
                }
            }

            let ctx_paragraph = Paragraph::new(context_lines)
                .block(Block::default().title(" Project Context ").borders(Borders::ALL))
                .wrap(Wrap { trim: false });
            frame.render_widget(ctx_paragraph, chunks[1]);

            let info_line = if let Some(msg) = &state.context_view.status_message {
                Line::from(Span::styled(msg, Style::default().fg(Color::Green).bold()))
            } else {
                Line::from(Span::styled(
                    "Press 'e' to add editable files, 'r' to add read-only files, 't' to toggle mode, 'd'/'x' to remove",
                    Style::default().fg(Color::DarkGray),
                ))
            };
            let prompt_widget = Paragraph::new(info_line)
                .block(Block::default().title(" Context Actions ").borders(Borders::ALL));
            frame.render_widget(prompt_widget, chunks[2]);

            let footer_line = Line::from(vec![
                Span::styled(" Ctrl+1/2 ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Switch  "),
                Span::styled(" e ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Add Edit  "),
                Span::styled(" r ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Add Read  "),
                Span::styled(" t ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Toggle  "),
                Span::styled(" d/x ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Remove  "),
                Span::styled(" ? ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Help"),
            ]);
            let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
            frame.render_widget(footer, chunks[3]);

            if state.context_view.adding_file {
                render_add_file_picker(frame, state);
            }
        }
    }

    if state.confirm_undo {
        render_confirm_undo_popup(frame, state);
    } else if let Some(dialog) = &state.selection_dialog {
        render_selection_dialog(frame, dialog);
    } else if state.show_help {
        render_help_popup(frame, state.view_mode);
    }
}

pub fn render_confirm_undo_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let area = centered_rect(55, 30, frame.area());
    frame.render_widget(Clear, area);

    let mut lines = Vec::new();
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Are you sure you want to undo the last AI commit?",
        Style::default().bold().fg(Color::Yellow),
    )));
    lines.push(Line::raw(""));

    if let Some(hash) = &state.model.last_commit_hash {
        lines.push(Line::from(vec![
            Span::raw("Commit: "),
            Span::styled(hash.clone(), Style::default().bold().fg(Color::Cyan)),
        ]));
    }
    if let Some(summary) = &state.model.last_commit_summary {
        lines.push(Line::from(vec![
            Span::raw("Summary: "),
            Span::styled(summary.clone(), Style::default().fg(Color::White)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Any uncommitted changes from the pre-edit checkpoint will be restored.",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(" [Y] / Enter ", Style::default().bg(Color::Red).fg(Color::White).bold()),
        Span::raw(" Confirm Undo    "),
        Span::styled(" [N] / Esc ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Cancel"),
    ]));

    let block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Confirm Undo ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        )
        .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(block, area);
}

pub fn render_selection_dialog(frame: &mut ratatui::Frame, dialog: &crate::app::SelectionDialogState) {
    let item_count = dialog.items.len();
    let height = (item_count as u16 + 4).clamp(5, 16);
    let area = centered_rect(50, (height * 100 / frame.area().height).clamp(15, 60), frame.area());
    frame.render_widget(Clear, area);

    let (title, title_color) = match dialog.kind {
        crate::app::SelectionDialogKind::Workflow => (" Select Workflow ", Color::Cyan),
        crate::app::SelectionDialogKind::EditProtocol => (" Select Edit Protocol ", Color::Magenta),
        crate::app::SelectionDialogKind::Model => (" Select Model ", Color::Green),
    };

    let mut lines = Vec::new();
    lines.push(Line::raw(""));

    for (idx, item) in dialog.items.iter().enumerate() {
        let is_selected = idx == dialog.selected_index;
        let prefix = if is_selected { " ▶ " } else { "   " };
        let line_style = if is_selected {
            Style::default().bg(Color::DarkGray).fg(Color::White).bold()
        } else {
            Style::default().fg(Color::Gray)
        };

        lines.push(
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(title_color).bold()),
                Span::styled(item.clone(), line_style),
            ])
            .style(line_style),
        );
    }
    lines.push(Line::raw(""));

    let block = Paragraph::new(lines).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(title_color)),
    );

    frame.render_widget(block, area);
}

pub fn render_add_file_picker(frame: &mut ratatui::Frame, state: &AppState) {
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
        ContextAccess::Editable => (" Add EDITABLE to Context (Path, directory or glob like *.rs) ", Color::Yellow),
        ContextAccess::ReadOnly => (" Add READ-ONLY to Context (Path, directory or glob like *.rs) ", Color::Cyan),
    };
    let input_block = Paragraph::new(input_line).block(
        Block::default()
            .title(access_title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color)),
    );
    frame.render_widget(input_block, chunks[0]);

    let mut candidate_lines = Vec::new();
    if state.context_view.filtered_candidates.is_empty() {
        candidate_lines.push(Line::from(Span::styled(
            "  No matching files found.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (idx, candidate) in state.context_view.filtered_candidates.iter().enumerate() {
            let is_sel = idx == state.context_view.selected_candidate_index;
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

    let list_title = match state.context_view.add_access {
        ContextAccess::Editable => " Matching Files / Actions (Enter: Add EDITABLE, Tab: Complete, ↑/↓: Navigate, Esc: Cancel) ",
        ContextAccess::ReadOnly => " Matching Files / Actions (Enter: Add READ-ONLY, Tab: Complete, ↑/↓: Navigate, Esc: Cancel) ",
    };
    let list_block = Paragraph::new(candidate_lines).block(
        Block::default()
            .title(list_title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(list_block, chunks[1]);
}

pub fn render_help_popup(frame: &mut ratatui::Frame, mode: ViewMode) {
    let area = centered_rect(65, 55, frame.area());
    frame.render_widget(Clear, area);

    let (title, help_lines) = match mode {
        ViewMode::Model => (
            " Help: Model View ",
            vec![
                Line::from(Span::styled("Global Navigation", Style::default().fg(Color::Cyan).bold())),
                Line::from("  Ctrl+1        Switch directly to Model view"),
                Line::from("  Ctrl+2        Switch directly to Context view (preserves input)"),
                Line::from("  Tab           Switch between views (if input empty)"),
                Line::from("  Esc / q       Close help / Cancel operation / Exit"),
                Line::from("  ?             Toggle this help popup"),
                Line::raw(""),
                Line::from(Span::styled("Workflow & Settings", Style::default().fg(Color::Green).bold())),
                Line::from("  Ctrl+W        Cycle workflow (disabled while generating)"),
                Line::from("  Ctrl+P        Cycle edit protocol (disabled while generating)"),
                Line::from("  Ctrl+M        Cycle active model (disabled while generating)"),
                Line::raw(""),
                Line::from(Span::styled("Editor & Prompt", Style::default().fg(Color::Yellow).bold())),
                Line::from("  Enter         Send prompt (also Ctrl+Enter / Alt+Enter)"),
                Line::from("  Shift+Enter   Insert newline in prompt (also Ctrl+J)"),
                Line::from("  ← / →         Move cursor left / right"),
                Line::from("  ↑ / ↓         Move cursor up / down across lines in prompt"),
                Line::from("  Ctrl+A / E    Move cursor to line start / end"),
                Line::from("  Alt+B / F     Move cursor word backward / forward"),
                Line::from("  Ctrl+K / U    Kill to line end / beginning"),
                Line::from("  Alt+D / Alt+Bksp Kill word forward / backward"),
                Line::from("  Ctrl+Y        Yank (paste) killed text"),
                Line::from("  Tab           Insert 2 spaces in prompt"),
                Line::from("  Esc           Clear prompt / Cancel streaming / Exit"),
                Line::raw(""),
                Line::from(Span::styled("Model & File Review", Style::default().fg(Color::Yellow).bold())),
                Line::from("  Space / Enter Fold / Unfold selected file diff (when input empty)"),
                Line::from("  [ / ]         Navigate modified files (when input empty)"),
                Line::from("  u             Undo last AI commit (asks confirmation)"),
                Line::from("  Ctrl+C        Cancel active streaming / thinking"),
                Line::from("  Ctrl+L        Clear conversation history & model view"),
                Line::from("  Ctrl+R        Toggle reasoning / thinking visibility"),
                Line::from("  PgUp / PgDn   Scroll output by page"),
            ],
        ),
        ViewMode::Context => (
            " Help: Context View ",
            vec![
                Line::from(Span::styled("Global Navigation", Style::default().fg(Color::Cyan).bold())),
                Line::from("  Ctrl+1        Switch directly to Model view"),
                Line::from("  Ctrl+2        Switch directly to Context view"),
                Line::from("  Tab           Switch between views"),
                Line::from("  Esc / q       Back to Model view / Close help"),
                Line::from("  ?             Toggle this help popup"),
                Line::raw(""),
                Line::from(Span::styled("Context Management", Style::default().fg(Color::Yellow).bold())),
                Line::from("  ↑/↓ or k/j    Navigate through context files"),
                Line::from("  e             Add file(s) for EDITING (write permissions)"),
                Line::from("  r             Add file(s) for READING (read-only)"),
                Line::from("  t             Toggle mode of active file (Editable ↔ Read-only)"),
                Line::from("  d / x / Del   Remove selected file from context"),
                Line::raw(""),
                Line::from(Span::styled("Add File / Pattern Picker", Style::default().fg(Color::Cyan).bold())),
                Line::from("  Type pattern  Filter by substring or glob (e.g. *.rs, src/)"),
                Line::from("  ↑ / ↓         Select matching file or '[+] Add all' action"),
                Line::from("  Tab           Autocomplete path into input"),
                Line::from("  Enter         Add highlighted file or all matching files"),
                Line::from("  Esc           Cancel picker"),
            ],
        ),
    };

    let popup_block = Paragraph::new(help_lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(popup_block, area);
}

pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
