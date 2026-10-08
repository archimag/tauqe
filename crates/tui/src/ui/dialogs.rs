use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{AppState, ViewMode};

use super::centered_rect;
use super::header::truncate_to_width;

/// Single source of truth for the model selection dialog geometry,
/// shared by rendering and mouse hit-testing.
pub fn selection_dialog_area(term: Rect, item_count: usize) -> Rect {
    let height = (item_count as u16 + 4).clamp(5, 16);
    centered_rect(50, (height * 100 / term.height.max(1)).clamp(15, 60), term)
}

/// Squash dialog uses almost the whole terminal so that all panes stay readable.
fn squash_dialog_area(term: Rect) -> Rect {
    let width = term.width - term.width / 10;
    let height = term.height.saturating_sub(2);
    Rect::new(term.x + (term.width - width) / 2, term.y + 1, width, height)
}

pub fn render_disconnected_popup(frame: &mut ratatui::Frame, message: &str) {
    let area = centered_rect(66, 32, frame.area());
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Connection to tauqe-server Lost",
            Style::default().bold().fg(Color::Red),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            message,
            Style::default().fg(Color::White),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "Press 'q' or Ctrl+Q to exit.",
            Style::default().fg(Color::Yellow).bold(),
        )),
    ];

    let block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Server Disconnected ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        )
        .alignment(ratatui::layout::Alignment::Center)
        .wrap(Wrap { trim: false });

    frame.render_widget(block, area);
}

pub fn render_squash_popup(frame: &mut ratatui::Frame, dialog: &mut crate::app::SquashDialogState) {
    let area = squash_dialog_area(frame.area());
    frame.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(6),
            Constraint::Length(3),
        ])
        .split(area);

    let session_label = match &dialog.session_base {
        Some(s) => {
            let short: String = s.chars().take(7).collect();
            format!("1: Session ({})", short)
        }
        None => "1: Session (none)".to_string(),
    };
    let upstream_label = match &dialog.upstream_base {
        Some(u) => format!("2: Upstream ({})", u),
        None => "2: Upstream (none)".to_string(),
    };
    let custom_label = match dialog.base_mode {
        crate::app::SquashBaseMode::Custom => format!("3: Custom [{}]", dialog.base_ref),
        _ => "3: Custom ref".to_string(),
    };

    let session_available = dialog.session_base.is_some();
    let upstream_available = dialog.upstream_base.is_some();

    let style_s = if dialog.base_mode == crate::app::SquashBaseMode::Session {
        Style::default().bg(Color::Cyan).fg(Color::Black).bold()
    } else if session_available {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let style_u = if dialog.base_mode == crate::app::SquashBaseMode::Upstream {
        Style::default().bg(Color::Cyan).fg(Color::Black).bold()
    } else if upstream_available {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let style_c = if dialog.base_mode == crate::app::SquashBaseMode::Custom {
        Style::default().bg(Color::Cyan).fg(Color::Black).bold()
    } else {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    };

    let mut header_lines = Vec::new();
    header_lines.push(Line::from(vec![
        Span::raw(" Base: "),
        Span::styled(format!(" {} ", session_label), style_s),
        Span::raw("  "),
        Span::styled(format!(" {} ", upstream_label), style_u),
        Span::raw("  "),
        Span::styled(format!(" {} ", custom_label), style_c),
        Span::raw(" | Scope: "),
        Span::styled(
            format!("{} commit(s) ahead of {}", dialog.commits.len(), dialog.base_ref),
            Style::default().bold().fg(Color::White),
        ),
    ]));

    if dialog.custom_input_active {
        header_lines.push(Line::from(vec![
            Span::styled(" Enter custom base ref: ", Style::default().fg(Color::Yellow).bold()),
            Span::styled(&dialog.custom_input, Style::default().bold().fg(Color::White)),
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(" (Enter to apply, Esc to cancel)", Style::default().fg(Color::DarkGray)),
        ]));
    } else if let Some(status) = &dialog.status_message {
        header_lines.push(Line::from(Span::styled(status, Style::default().fg(Color::Yellow))));
    } else {
        header_lines.push(Line::from(vec![
            Span::styled(" Stats: ", Style::default().fg(Color::DarkGray).bold()),
            Span::styled(&dialog.diff_stat, Style::default().fg(Color::Green)),
            Span::styled(" | 100% local inspection: preview diff without sending tokens.", Style::default().fg(Color::DarkGray)),
        ]));
    }

    let header_widget = Paragraph::new(header_lines).block(
        Block::default()
            .title(" Squash Commits into Developer Feature Commit ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(header_widget, chunks[0]);

    // Middle section: Left = File List (with folding), Right = Diff Viewer
    let mid_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(chunks[1]);

    let mut file_lines = Vec::new();
    if dialog.files.is_empty() {
        if dialog.loading {
            file_lines.push(Line::from(Span::styled("  Loading diff...", Style::default().fg(Color::DarkGray))));
        } else {
            file_lines.push(Line::from(Span::styled("  No changed files.", Style::default().fg(Color::DarkGray))));
        }
    } else {
        for (idx, f) in dialog.files.iter().enumerate() {
            let is_sel = idx == dialog.selected_file_index;
            let fold_icon = if f.expanded { "▼ " } else { "▶ " };
            let prefix = if is_sel { "● " } else { "  " };
            let style = if is_sel {
                Style::default().bg(Color::DarkGray).fg(Color::White).bold()
            } else {
                Style::default().fg(Color::White)
            };

            file_lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::Cyan)),
                Span::styled(fold_icon, Style::default().fg(Color::Yellow)),
                Span::styled(&f.path, style),
            ]));
        }
    }

    let is_files_focus = dialog.focus == crate::app::SquashDialogFocus::FileList;
    let files_visible = mid_chunks[0].height.saturating_sub(2) as usize;
    let files_offset = (dialog.selected_file_index + 1).saturating_sub(files_visible);
    let file_list_widget = Paragraph::new(file_lines)
        .block(
            Block::default()
                .title(format!(" Files Changed ({}) ", dialog.files.len()))
                .borders(Borders::ALL)
                .border_style(if is_files_focus { Style::default().fg(Color::Cyan) } else { Style::default() }),
        )
        .scroll((files_offset as u16, 0));
    frame.render_widget(file_list_widget, mid_chunks[0]);

    // Right: Diff Viewer
    let mut diff_lines = Vec::new();
    if let Some(active_file) = dialog.files.get(dialog.selected_file_index) {
        if !active_file.expanded {
            diff_lines.push(Line::from(Span::styled(
                format!("  [File '{}' is folded. Press Space to expand diff]", active_file.path),
                Style::default().fg(Color::DarkGray).italic(),
            )));
        } else {
            for l in active_file.diff.lines() {
                if l.starts_with('+') && !l.starts_with("+++") {
                    diff_lines.push(Line::from(Span::styled(l, Style::default().fg(Color::Green))));
                } else if l.starts_with('-') && !l.starts_with("---") {
                    diff_lines.push(Line::from(Span::styled(l, Style::default().fg(Color::Red))));
                } else if l.starts_with("@@") {
                    diff_lines.push(Line::from(Span::styled(l, Style::default().fg(Color::Cyan).bold())));
                } else {
                    diff_lines.push(Line::from(Span::styled(l, Style::default().fg(Color::Gray))));
                }
            }
        }
    } else {
        diff_lines.push(Line::from(Span::styled("  No diff to display.", Style::default().fg(Color::DarkGray))));
    }

    let diff_visible = mid_chunks[1].height.saturating_sub(2) as usize;
    let diff_max_scroll = diff_lines
        .len()
        .saturating_sub(diff_visible)
        .min(u16::MAX as usize) as u16;
    dialog.diff_scroll = dialog.diff_scroll.min(diff_max_scroll);

    let is_diff_focus = dialog.focus == crate::app::SquashDialogFocus::DiffView;
    let diff_widget = Paragraph::new(diff_lines)
        .block(
            Block::default()
                .title(" Diff Inspection (Space: Fold/Unfold, PgUp/PgDn: Scroll) ")
                .borders(Borders::ALL)
                .border_style(if is_diff_focus { Style::default().fg(Color::Cyan) } else { Style::default() }),
        )
        .scroll((dialog.diff_scroll, 0));
    frame.render_widget(diff_widget, mid_chunks[1]);

    // Commit Message Box
    let is_msg_focus = dialog.focus == crate::app::SquashDialogFocus::MessageEditor;
    let mut msg_lines = Vec::new();
    if dialog.generating_message {
        msg_lines.push(Line::from(Span::styled(
            "Generating Conventional Commit message via LLM...",
            Style::default().fg(Color::Yellow),
        )));
    } else if dialog.message_buffer.is_empty() {
        msg_lines.push(Line::from(vec![
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(" Press 'g' to generate message via AI, or type manually...", Style::default().fg(Color::DarkGray)),
        ]));
    } else {
        let mut rows: Vec<Line> = dialog.message_buffer.split('\n').map(Line::raw).collect();
        if let Some(last) = rows.last_mut().filter(|_| is_msg_focus) {
            last.spans.push(Span::styled("█", Style::default().fg(Color::Yellow)));
        }
        // Keep the tail visible so the line being typed never leaves the box.
        let visible = chunks[2].height.saturating_sub(2) as usize;
        if rows.len() > visible {
            rows.drain(..rows.len() - visible);
        }
        msg_lines.extend(rows);
    }

    let msg_widget = Paragraph::new(msg_lines).block(
        Block::default()
            .title(" Commit Message (Press 'g' to Generate via AI, 'e' to Edit) ")
            .borders(Borders::ALL)
            .border_style(if is_msg_focus { Style::default().fg(Color::Green) } else { Style::default() }),
    );
    frame.render_widget(msg_widget, chunks[2]);

    // Footer actions
    let footer_line = Line::from(vec![
        Span::styled(" 1/2/3 ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Base  "),
        Span::styled(" Space/Enter ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Fold  "),
        Span::styled(" Tab ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Focus  "),
        Span::styled(" g ", Style::default().bg(Color::Yellow).fg(Color::Black).bold()),
        Span::raw(" Generate Msg  "),
        Span::styled(" e ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Edit  "),
        Span::styled(" Ctrl+Enter / Ctrl+S ", Style::default().bg(Color::Green).fg(Color::Black).bold()),
        Span::raw(" Apply Squash  "),
        Span::styled(" Esc ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Close"),
    ]);
    let footer_widget = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer_widget, chunks[3]);

    if dialog.confirm_apply {
        render_squash_confirm_popup(frame, dialog);
    }
}

pub fn render_squash_confirm_popup(frame: &mut ratatui::Frame, dialog: &crate::app::SquashDialogState) {
    let area = centered_rect(60, 32, frame.area());
    frame.render_widget(Clear, area);

    let (confirm_style, cancel_style) = match dialog.confirm_button {
        crate::app::ConfirmDialogButton::Confirm => (
            Style::default().bg(Color::Red).fg(Color::White).bold(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        crate::app::ConfirmDialogButton::Cancel => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
    };

    let first_line = dialog.message_buffer.lines().next().unwrap_or("").trim();
    let msg_preview = if first_line.is_empty() {
        "(no message)".to_string()
    } else {
        truncate_to_width(first_line, 50)
    };

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Confirm Git History Squash?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Squashing "),
            Span::styled(
                format!("{} commit(s)", dialog.commits.len()),
                Style::default().bold().fg(Color::Cyan),
            ),
            Span::raw(" into 1 commit on top of "),
            Span::styled(dialog.base_ref.clone(), Style::default().bold().fg(Color::Green)),
        ]),
        Line::from(vec![
            Span::raw("Message: "),
            Span::styled(format!("\"{}\"", msg_preview), Style::default().fg(Color::White)),
        ]),
        Line::from(Span::styled(
            "This operation rewrites local Git history ahead of the base ref.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" [ Confirm Squash (Y) ] ", confirm_style),
            Span::raw("   "),
            Span::styled(" [ Cancel (Esc) ] ", cancel_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "Tab / ← / → to select button, Enter to apply",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Confirm Git Squash ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        )
        .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(block, area);
}

pub fn render_confirm_cancel_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let area = centered_rect(58, 28, frame.area());
    frame.render_widget(Clear, area);

    let (confirm_style, cancel_style) = match state.confirm_button {
        crate::app::ConfirmDialogButton::Confirm => (
            Style::default().bg(Color::Red).fg(Color::White).bold(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        crate::app::ConfirmDialogButton::Cancel => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
    };

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Interrupt active model operation?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from("Generation will be stopped immediately."),
        Line::from(Span::styled(
            "All in-progress edits and uncommitted diffs will be discarded.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" [ Confirm (Y) ] ", confirm_style),
            Span::raw("   "),
            Span::styled(" [ Cancel (Esc) ] ", cancel_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "Tab / ← / → to select button, Enter to apply",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Confirm Interruption ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(block, area);
}

pub fn render_confirm_quit_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let area = centered_rect(58, 28, frame.area());
    frame.render_widget(Clear, area);

    let (confirm_style, cancel_style) = match state.confirm_button {
        crate::app::ConfirmDialogButton::Confirm => (
            Style::default().bg(Color::Red).fg(Color::White).bold(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        crate::app::ConfirmDialogButton::Cancel => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
    };

    let activity = if state.review.running {
        "Code review is currently in progress."
    } else {
        "Model generation / turn is currently in progress."
    };

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Quit TAUQE while operation is running?",
            Style::default().bold().fg(Color::Red),
        )),
        Line::raw(""),
        Line::from(activity),
        Line::from(Span::styled(
            "Exiting now will terminate the server and discard active work.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" [ Quit TAUQE (Y) ] ", confirm_style),
            Span::raw("   "),
            Span::styled(" [ Cancel (Esc) ] ", cancel_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "Tab / ← / → to select button, Enter to apply",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Confirm Quit ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        )
        .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(block, area);
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
    let (confirm_style, cancel_style) = match state.confirm_button {
        crate::app::ConfirmDialogButton::Confirm => (
            Style::default().bg(Color::Red).fg(Color::White).bold(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        crate::app::ConfirmDialogButton::Cancel => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
    };

    lines.push(Line::from(vec![
        Span::styled(" [ Confirm Undo (Y) ] ", confirm_style),
        Span::raw("   "),
        Span::styled(" [ Cancel (Esc) ] ", cancel_style),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Tab / ← / → to select button, Enter to apply",
        Style::default().fg(Color::DarkGray),
    )));

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

pub fn render_confirm_delete_plan_popup(frame: &mut ratatui::Frame, plan_id: &str, state: &AppState) {
    let area = centered_rect(58, 28, frame.area());
    frame.render_widget(Clear, area);

    let plan_title = state
        .plans_view
        .current_plan
        .as_ref()
        .filter(|p| p.id == plan_id)
        .map(|p| p.title.clone())
        .or_else(|| {
            state
                .plans_view
                .plans_list
                .iter()
                .find(|p| p.id == plan_id)
                .map(|p| p.title.clone())
        })
        .unwrap_or_else(|| plan_id.to_string());

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Are you sure you want to delete this plan?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Plan: "),
            Span::styled(format!("{} ({})", plan_title, plan_id), Style::default().bold().fg(Color::Cyan)),
        ]),
        Line::from(Span::styled(
            "The plan file in .tauqe/plans/ will be permanently removed.",
            Style::default().fg(Color::Gray),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                " [ Confirm Delete (Y) ] ",
                if state.confirm_button == crate::app::ConfirmDialogButton::Confirm {
                    Style::default().bg(Color::Red).fg(Color::White).bold()
                } else {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                },
            ),
            Span::raw("   "),
            Span::styled(
                " [ Cancel (Esc) ] ",
                if state.confirm_button == crate::app::ConfirmDialogButton::Cancel {
                    Style::default().bg(Color::Cyan).fg(Color::Black).bold()
                } else {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                },
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "Tab / ← / → to select button, Enter to apply",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Confirm Delete Plan ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        )
        .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(block, area);
}

pub fn render_confirm_clear_auto_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let area = centered_rect(58, 28, frame.area());
    frame.render_widget(Clear, area);

    let (confirm_style, cancel_style) = match state.confirm_button {
        crate::app::ConfirmDialogButton::Confirm => (
            Style::default().bg(Color::Red).fg(Color::White).bold(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        crate::app::ConfirmDialogButton::Cancel => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
    };

    let auto_count = state
        .context
        .items
        .iter()
        .filter(|i| i.layer == tauqe_protocol::ContextLayer::Auto)
        .count();

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Clear all Auto-context files?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Are you sure you want to remove all "),
            Span::styled(
                format!("{} file(s)", auto_count),
                Style::default().bold().fg(Color::Magenta),
            ),
            Span::raw(" from the Auto layer?"),
        ]),
        Line::from(Span::styled(
            "Persistent User files and Pinned files will not be affected.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" [ Confirm Clear (Y) ] ", confirm_style),
            Span::raw("   "),
            Span::styled(" [ Cancel (Esc) ] ", cancel_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "Tab / ← / → to select button, Enter to apply",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Confirm Clear Auto Context ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(block, area);
}

pub fn render_confirm_clear_history_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let area = centered_rect(58, 30, frame.area());
    frame.render_widget(Clear, area);

    let (confirm_style, cancel_style) = match state.confirm_button {
        crate::app::ConfirmDialogButton::Confirm => (
            Style::default().bg(Color::Red).fg(Color::White).bold(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        crate::app::ConfirmDialogButton::Cancel => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
    };

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Are you sure you want to clear the conversation history?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from("This will permanently erase the session history:"),
        Line::from(Span::styled(
            "  .tauqe/history.jsonl",
            Style::default().fg(Color::DarkGray).italic(),
        )),
        Line::from(Span::styled(
            "and clear the current model view output.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" [ Confirm Clear (Y) ] ", confirm_style),
            Span::raw("   "),
            Span::styled(" [ Cancel (Esc) ] ", cancel_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "Tab / ← / → to select button, Enter to apply",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Confirm Clear History ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        )
        .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(block, area);
}

pub fn render_selection_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::SelectionDialogState,
) {
    let area = selection_dialog_area(frame.area(), dialog.items.len());
    frame.render_widget(Clear, area);

    let title = " Select Model (Mouse Click / Enter) ";
    let title_color = Color::Green;

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
                Span::styled(item.to_string(), line_style),
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

pub fn render_review_dialog(frame: &mut ratatui::Frame, dialog: &crate::app::ReviewDialogState) {
    let area = centered_rect(64, 52, frame.area());
    frame.render_widget(Clear, area);

    let model = dialog
        .models
        .get(dialog.model_index)
        .map(|m| m.to_string())
        .unwrap_or_else(|| "(none)".to_string());
    let dim = Style::default().fg(Color::DarkGray);

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Run a code review on the current context?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  Files: ", dim),
            Span::styled(
                dialog.files_count.to_string(),
                Style::default().fg(Color::White).bold(),
            ),
            Span::styled("    Estimated tokens: ", dim),
            Span::styled(
                format!("~{}", dialog.estimated_tokens),
                Style::default().fg(Color::White).bold(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Model: ", dim),
            Span::styled(
                format!("◄ {} ►", model),
                Style::default().fg(Color::Green).bold(),
            ),
            Span::styled("  (Up/Down or Tab to change)", dim),
        ]),
        Line::from(Span::styled(
            "  Only file contents are sent: no history, no repo map.",
            dim,
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  Extra instructions (optional):",
            Style::default().fg(Color::Cyan).bold(),
        )),
        Line::from(vec![
            Span::raw("  > "),
            Span::styled(dialog.prompt.clone(), Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(Color::Yellow)),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                " Enter ",
                Style::default().bg(Color::Green).fg(Color::Black).bold(),
            ),
            Span::raw(" Start review    "),
            Span::styled(
                " Esc ",
                Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
            ),
            Span::raw(" Cancel"),
        ]),
    ];

    let widget = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Code Review ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Magenta)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

pub const GLOBAL_COMMANDS: &[crate::app::KeyCommand] = &[
    crate::app::KeyCommand { key: "Ctrl+1..5 / Alt+1..5 / F1..F5", description: "Switch views: Develop │ Context │ Review │ Plans │ History" },
    crate::app::KeyCommand { key: "Ctrl+M / Alt+M / Ctrl+Y", description: "Select active model" },
    crate::app::KeyCommand { key: "F6 / Ctrl+S", description: "Squash commits dialog" },
    crate::app::KeyCommand { key: "Ctrl+C", description: "Cancel active generation" },
    crate::app::KeyCommand { key: "Ctrl+L", description: "Clear conversation history" },
    crate::app::KeyCommand { key: "Ctrl+R", description: "Fold / unfold thinking (reasoning) stream" },
    crate::app::KeyCommand { key: "Ctrl+O", description: "Reload configuration from disk" },
    crate::app::KeyCommand { key: "Ctrl+Q", description: "Quit TAUQE" },
    crate::app::KeyCommand { key: "F1 / ? / Esc", description: "Open / close this help dialog" },
];

pub const ONBOARDING_COMMANDS: &[crate::app::KeyCommand] = &[
    crate::app::KeyCommand { key: "Ctrl+V", description: "Paste API key from clipboard" },
    crate::app::KeyCommand { key: "Ctrl+R", description: "Show / hide entered key" },
    crate::app::KeyCommand { key: "Ctrl+U", description: "Clear key input buffer" },
    crate::app::KeyCommand { key: "Ctrl+O", description: "Reload configuration from disk" },
    crate::app::KeyCommand { key: "Enter", description: "Execute selected onboarding action" },
    crate::app::KeyCommand { key: "Esc", description: "Exit help dialog" },
];

pub fn get_mode_key_commands(mode: ViewMode) -> (&'static str, &'static [crate::app::KeyCommand]) {
    match mode {
        ViewMode::Develop => (" Help: Develop ", crate::input::develop::DEVELOP_COMMANDS),
        ViewMode::Context => (" Help: Context ", crate::input::context::CONTEXT_COMMANDS),
        ViewMode::History => (" Help: History ", crate::input::history::HISTORY_COMMANDS),
        ViewMode::Review => (" Help: Review ", crate::input::review::REVIEW_COMMANDS),
        ViewMode::Plans => (" Help: Plans ", crate::input::plans::PLANS_COMMANDS),
        ViewMode::Onboarding => (" Help: Setup ", ONBOARDING_COMMANDS),
    }
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let current_len = current.chars().count();
        let word_len = word.chars().count();
        if !current.is_empty() && current_len + 1 + word_len > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn format_command_key(raw_key: &str, primary_modifier: crate::config::PrimaryModifier) -> String {
    let mod_prefix = match primary_modifier {
        crate::config::PrimaryModifier::Ctrl => "Ctrl+",
        crate::config::PrimaryModifier::Alt => "Alt+",
    };
    raw_key.replace("C-", mod_prefix)
}

fn push_command_lines(
    lines: &mut Vec<Line<'static>>,
    commands: &[crate::app::KeyCommand],
    col_w: usize,
    desc_w: usize,
    primary_modifier: crate::config::PrimaryModifier,
) {
    for cmd in commands {
        let display_key = format_command_key(cmd.key, primary_modifier);
        let mut wrapped = wrap_words(cmd.description, desc_w).into_iter();
        let first = wrapped.next().unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<col_w$}", display_key), Style::default().fg(Color::White).bold()),
            Span::styled(first, Style::default().fg(Color::Gray)),
        ]));
        for rest in wrapped {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(2 + col_w)),
                Span::styled(rest, Style::default().fg(Color::Gray)),
            ]));
        }
    }
}

pub fn render_help_popup(
    frame: &mut ratatui::Frame,
    mode: ViewMode,
    scroll: &mut u16,
    primary_modifier: crate::config::PrimaryModifier,
) {
    let (title, mode_commands) = get_mode_key_commands(mode);
    let area = centered_rect(76, 75, frame.area());
    frame.render_widget(Clear, area);

    let key_w = GLOBAL_COMMANDS
        .iter()
        .chain(mode_commands.iter())
        .map(|c| format_command_key(c.key, primary_modifier).chars().count())
        .max()
        .unwrap_or(0);
    let col_w = key_w + 2;
    let inner_w = area.width.saturating_sub(2) as usize;
    let desc_w = inner_w.saturating_sub(2 + col_w).max(10);

    let mut help_lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
        "Global Navigation",
        Style::default().fg(Color::Cyan).bold(),
    ))];
    push_command_lines(&mut help_lines, GLOBAL_COMMANDS, col_w, desc_w, primary_modifier);

    help_lines.push(Line::raw(""));
    help_lines.push(Line::from(Span::styled(
        "Active Mode Shortcuts",
        Style::default().fg(Color::Yellow).bold(),
    )));
    push_command_lines(&mut help_lines, mode_commands, col_w, desc_w, primary_modifier);

    let inner_h = area.height.saturating_sub(2) as usize;
    let max_scroll = help_lines
        .len()
        .saturating_sub(inner_h)
        .min(u16::MAX as usize) as u16;
    *scroll = (*scroll).min(max_scroll);
    let title = if max_scroll > 0 {
        format!("{}(j/k to scroll) ", title)
    } else {
        title.to_string()
    };

    let popup_block = Paragraph::new(help_lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .scroll((*scroll, 0));

    frame.render_widget(popup_block, area);
}
