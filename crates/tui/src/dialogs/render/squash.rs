//! Rendering for the squash dialog: base selection, commit and file/diff
//! inspection, commit message editing, and the apply/discard confirmation popups.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::theme::engine::current_engine;
use crate::ui::header::truncate_to_width;
use super::{editor_lines, render_modal_confirm, ConfirmModalParams, ModalTone};

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
    let engine = current_engine();
    let palette = engine.palette();
    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Discard the commit message and close the dialog?",
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "The message in the editor will be permanently lost.",
            engine.fg(palette.text_muted),
        )),
    ];

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Discard Message",
            tone: ModalTone::Warning,
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
    let engine = current_engine();
    let palette = engine.palette();
    let area = squash_dialog_area(frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(engine.color(palette.surface))),
        area,
    );

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
        engine.bg(palette.button_active_bg).fg(engine.color(palette.text_inverse)).bold()
    } else if session_available {
        engine.bg(palette.button_bg).fg(engine.color(palette.button_fg))
    } else {
        engine.fg(palette.text_muted)
    };

    let style_u = if dialog.base_mode == crate::app::SquashBaseMode::Upstream {
        engine.bg(palette.button_active_bg).fg(engine.color(palette.text_inverse)).bold()
    } else if upstream_available {
        engine.bg(palette.button_bg).fg(engine.color(palette.button_fg))
    } else {
        engine.fg(palette.text_muted)
    };

    let style_c = if dialog.base_mode == crate::app::SquashBaseMode::Custom {
        engine.bg(palette.button_active_bg).fg(engine.color(palette.text_inverse)).bold()
    } else {
        engine.bg(palette.button_bg).fg(engine.color(palette.button_fg))
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
            engine.fg(palette.text_primary).bold(),
        ),
    ]));

    if dialog.custom_input_active {
        let mut custom_line = vec![Span::styled(
            " Enter custom base ref: ",
            engine.fg(palette.status_warning).bold(),
        )];
        custom_line.extend(
            editor_lines(&dialog.custom_editor, true)
                .into_iter()
                .flat_map(|l| l.spans),
        );
        custom_line.push(Span::styled(
            " (Enter to apply, Esc to cancel)",
            engine.fg(palette.text_muted),
        ));
        header_lines.push(Line::from(custom_line));
    } else if let Some(status) = &dialog.status_message {
        header_lines.push(Line::from(Span::styled(status, engine.fg(palette.status_warning))));
    } else {
        header_lines.push(Line::from(vec![
            Span::styled(" Stats: ", engine.fg(palette.text_muted).bold()),
            Span::styled(&dialog.diff_stat, engine.fg(palette.diff_added)),
            Span::styled(" | 100% local inspection: preview diff without sending tokens.", engine.fg(palette.text_muted)),
        ]));
    }

    let header_widget = Paragraph::new(header_lines).block(
        Block::default()
            .title(" Squash Commits into Developer Feature Commit ")
            .borders(Borders::ALL)
            .border_style(engine.fg(palette.status_info))
            .style(Style::default().bg(engine.color(palette.surface))),
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
            file_lines.push(Line::from(Span::styled("  Loading diff...", engine.fg(palette.text_muted))));
        } else {
            file_lines.push(Line::from(Span::styled("  No changed files.", engine.fg(palette.text_muted))));
        }
    } else {
        for (idx, f) in dialog.files.iter().enumerate() {
            let is_sel = idx == dialog.selected_file_index;
            let fold_icon = if f.expanded { "▼ " } else { "▶ " };
            let prefix = if is_sel { "● " } else { "  " };
            let style = if is_sel {
                engine.bg(palette.overlay).fg(engine.color(palette.text_primary)).bold()
            } else {
                engine.fg(palette.text_primary)
            };

            file_lines.push(Line::from(vec![
                Span::styled(prefix, engine.fg(palette.status_info)),
                Span::styled(fold_icon, engine.fg(palette.status_warning)),
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
                .border_style(if is_files_focus { engine.fg(palette.status_info) } else { engine.fg(palette.border) })
                .style(Style::default().bg(engine.color(palette.surface))),
        )
        .scroll((files_offset as u16, 0));
    frame.render_widget(file_list_widget, mid_chunks[0]);

    // Right: Diff Viewer
    let mut diff_lines = Vec::new();
    if let Some(active_file) = dialog.files.get(dialog.selected_file_index) {
        if !active_file.expanded {
            diff_lines.push(Line::from(Span::styled(
                format!("  [File '{}' is folded. Press Space to expand diff]", active_file.path),
                engine.fg(palette.text_muted).italic(),
            )));
        } else {
            for l in active_file.diff.lines() {
                if l.starts_with('+') && !l.starts_with("+++") {
                    diff_lines.push(Line::from(Span::styled(l, engine.fg(palette.diff_added))));
                } else if l.starts_with('-') && !l.starts_with("---") {
                    diff_lines.push(Line::from(Span::styled(l, engine.fg(palette.diff_removed))));
                } else if l.starts_with("@@") {
                    diff_lines.push(Line::from(Span::styled(l, engine.fg(palette.status_info).bold())));
                } else {
                    diff_lines.push(Line::from(Span::styled(l, engine.fg(palette.diff_context))));
                }
            }
        }
    } else {
        diff_lines.push(Line::from(Span::styled("  No diff to display.", engine.fg(palette.text_muted))));
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
                .border_style(if is_diff_focus { engine.fg(palette.status_info) } else { engine.fg(palette.border) })
                .style(Style::default().bg(engine.color(palette.surface))),
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
            engine.fg(palette.status_warning),
        ))]
    } else if dialog.message_editor.is_empty() {
        let (cursor, hint) = if is_msg_focus {
            ("█", " Ctrl+G to generate message via AI, or type manually...")
        } else {
            (" ", " Press 'g' to generate message via AI, or 'e' to type manually...")
        };
        vec![Line::from(vec![
            Span::styled(cursor, engine.fg(palette.status_warning)),
            Span::styled(hint, engine.fg(palette.text_muted)),
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
                .border_style(if is_msg_focus { engine.fg(palette.status_success) } else { engine.fg(palette.border) })
                .style(Style::default().bg(engine.color(palette.surface))),
        )
        .scroll((msg_scroll, 0));
    frame.render_widget(msg_widget, chunks[2]);

    // Footer actions (contextual: letter keys are literal text while the editor has focus)
    let footer_line = if is_msg_focus {
        Line::from(vec![
            Span::styled(" Ctrl+G ", engine.bg(palette.status_warning).fg(engine.color(palette.text_inverse)).bold()),
            Span::raw(" Generate Msg  "),
            Span::styled(" Ctrl+Enter / Ctrl+S ", engine.bg(palette.status_success).fg(engine.color(palette.text_inverse)).bold()),
            Span::raw(" Apply Squash  "),
            Span::styled(" Tab / Esc ", engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)).bold()),
            Span::raw(" File list"),
        ])
    } else {
        Line::from(vec![
        Span::styled(" 1/2/3 ", engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)).bold()),
        Span::raw(" Base  "),
        Span::styled(" Space/Enter ", engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)).bold()),
        Span::raw(" Fold  "),
        Span::styled(" Tab ", engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)).bold()),
        Span::raw(" Focus  "),
        Span::styled(" g ", engine.bg(palette.status_warning).fg(engine.color(palette.text_inverse)).bold()),
        Span::raw(" Generate Msg  "),
        Span::styled(" e ", engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)).bold()),
        Span::raw(" Edit  "),
        Span::styled(" Ctrl+Enter / Ctrl+S ", engine.bg(palette.status_success).fg(engine.color(palette.text_inverse)).bold()),
        Span::raw(" Apply Squash  "),
        Span::styled(" Esc ", engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)).bold()),
        Span::raw(" Close"),
        ])
    };
    let footer_widget = Paragraph::new(footer_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(engine.fg(palette.border))
            .style(Style::default().bg(engine.color(palette.surface))),
    );
    frame.render_widget(footer_widget, chunks[3]);

    match dialog.confirm {
        Some(crate::app::SquashConfirm::Apply) => render_squash_confirm_popup(frame, dialog),
        Some(crate::app::SquashConfirm::Discard) => render_squash_discard_popup(frame, dialog),
        None => {}
    }
}

pub fn render_squash_confirm_popup(frame: &mut ratatui::Frame, dialog: &crate::app::SquashDialogState) {
    let engine = current_engine();
    let palette = engine.palette();
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
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Squashing "),
            Span::styled(
                format!("{} commit(s)", dialog.commits.len()),
                engine.fg(palette.status_info).bold(),
            ),
            Span::raw(" into 1 commit on top of "),
            Span::styled(dialog.base_ref.clone(), engine.fg(palette.status_success).bold()),
        ]),
        Line::from(vec![
            Span::raw("Message: "),
            Span::styled(format!("\"{}\"", msg_preview), engine.fg(palette.text_primary)),
        ]),
        Line::from(Span::styled(
            "This operation rewrites local Git history ahead of the base ref.",
            engine.fg(palette.text_muted),
        )),
    ];

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Confirm Git Squash",
            tone: ModalTone::Dangerous,
            body_lines,
            confirm_label: "Confirm Squash (Y)",
            cancel_label: "Cancel (Esc)",
            confirm_button: dialog.confirm_button,
            destructive: true,
            width: 60,
        },
    );
}
