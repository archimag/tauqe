//! Rendering for the squash dialog: base selection, commit and file/diff
//! inspection, commit message editing, and the apply/discard confirmation popups.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::ui::header::truncate_to_width;
use super::{editor_lines, render_modal_confirm, ConfirmModalParams};

/// Squash dialog uses almost the whole terminal so that all panes stay readable.
pub fn squash_dialog_area(term: Rect) -> Rect {
    let width = term.width - term.width / 10;
    let height = term.height.saturating_sub(2);
    Rect::new(term.x + (term.width - width) / 2, term.y + 1, width, height)
}

/// Returns the outer area, file list pane rect, and diff pane rect for squash dialog.
pub fn squash_dialog_chunks(term: Rect) -> (Rect, Rect, Rect) {
    let area = squash_dialog_area(term);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(6),
            Constraint::Length(3),
        ])
        .split(area);
    let mid_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(chunks[1]);
    (area, mid_chunks[0], mid_chunks[1])
}

pub fn render_squash_discard_popup(frame: &mut ratatui::Frame, dialog: &crate::app::SquashDialogState) {
    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Discard the commit message and close the dialog?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "The message in the editor will be permanently lost.",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Discard Message",
            border_color: Color::Yellow,
            body_lines,
            confirm_label: "Discard (Y)",
            cancel_label: "Cancel (Esc)",
            confirm_button: dialog.confirm_button,
            destructive: true,
            width: 58,
        },
    );
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
    header_lines.push(Line::raw(""));
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
        let mut custom_line = vec![Span::styled(
            " Enter custom base ref: ",
            Style::default().fg(Color::Yellow).bold(),
        )];
        custom_line.extend(
            editor_lines(&dialog.custom_editor, true)
                .into_iter()
                .flat_map(|l| l.spans),
        );
        custom_line.push(Span::styled(
            " (Enter to apply, Esc to cancel)",
            Style::default().fg(Color::DarkGray),
        ));
        header_lines.push(Line::from(custom_line));
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
    // Hints depend on focus: bare letters are literal text while the editor is focused.
    let msg_visible = chunks[2].height.saturating_sub(2) as usize;
    let mut msg_scroll = 0u16;
    let msg_lines = if dialog.generating_message {
        vec![Line::from(Span::styled(
            "Generating Conventional Commit message via LLM...",
            Style::default().fg(Color::Yellow),
        ))]
    } else if dialog.message_editor.is_empty() {
        let (cursor, hint) = if is_msg_focus {
            ("█", " Ctrl+G to generate message via AI, or type manually...")
        } else {
            (" ", " Press 'g' to generate message via AI, or 'e' to type manually...")
        };
        vec![Line::from(vec![
            Span::styled(cursor, Style::default().fg(Color::Yellow)),
            Span::styled(hint, Style::default().fg(Color::DarkGray)),
        ])]
    } else {
        // Keep the cursor line visible inside the box.
        let (cursor_line, _) = dialog.message_editor.cursor_line_col();
        msg_scroll = (cursor_line + 1)
            .saturating_sub(msg_visible)
            .min(u16::MAX as usize) as u16;
        editor_lines(&dialog.message_editor, is_msg_focus)
    };

    let msg_title = if is_msg_focus {
        " Commit Message (Ctrl+G Generate via AI, Tab/Esc File list) "
    } else {
        " Commit Message ('g' Generate via AI, 'e' Edit) "
    };
    let msg_widget = Paragraph::new(msg_lines)
        .block(
            Block::default()
                .title(msg_title)
                .borders(Borders::ALL)
                .border_style(if is_msg_focus { Style::default().fg(Color::Green) } else { Style::default() }),
        )
        .scroll((msg_scroll, 0));
    frame.render_widget(msg_widget, chunks[2]);

    // Footer actions (contextual: letter keys are literal text while the editor has focus)
    let footer_line = if is_msg_focus {
        Line::from(vec![
            Span::styled(" Ctrl+G ", Style::default().bg(Color::Yellow).fg(Color::Black).bold()),
            Span::raw(" Generate Msg  "),
            Span::styled(" Ctrl+Enter / Ctrl+S ", Style::default().bg(Color::Green).fg(Color::Black).bold()),
            Span::raw(" Apply Squash  "),
            Span::styled(" Tab / Esc ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
            Span::raw(" File list"),
        ])
    } else {
        Line::from(vec![
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
        ])
    };
    let footer_widget = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer_widget, chunks[3]);

    match dialog.confirm {
        Some(crate::app::SquashConfirm::Apply) => render_squash_confirm_popup(frame, dialog),
        Some(crate::app::SquashConfirm::Discard) => render_squash_discard_popup(frame, dialog),
        None => {}
    }
}

pub fn render_squash_confirm_popup(frame: &mut ratatui::Frame, dialog: &crate::app::SquashDialogState) {
    let first_line = dialog.message_editor.get_text().lines().next().unwrap_or("").trim();
    let msg_preview = if first_line.is_empty() {
        "(no message)".to_string()
    } else {
        truncate_to_width(first_line, 50)
    };

    let body_lines = vec![
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
    ];

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Confirm Git Squash",
            border_color: Color::Red,
            body_lines,
            confirm_label: "Confirm Squash (Y)",
            cancel_label: "Cancel (Esc)",
            confirm_button: dialog.confirm_button,
            destructive: true,
            width: 60,
        },
    );
}
