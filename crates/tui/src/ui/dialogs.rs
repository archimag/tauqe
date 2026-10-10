use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{tier_title, AppState, SelectionDialogKind, SelectionItem, ViewMode};
use crate::editor::InputEditor;

use super::centered_rect;
use super::header::truncate_to_width;

/// Single source of truth for the model selection dialog geometry,
/// shared by rendering and mouse hit-testing.
pub fn selection_dialog_area(term: Rect, item_count: usize) -> Rect {
    let height = (item_count as u16 + 6).clamp(9, 21).min(term.height);
    let width = (term.width.saturating_mul(70) / 100).clamp(40, 80).min(term.width);
    Rect::new(
        term.x + (term.width - width) / 2,
        term.y + (term.height - height) / 2,
        width,
        height,
    )
}

/// Row text for the model picker. Tier rows carry their quick-select number.
fn selection_item_label(item: &SelectionItem, index: usize) -> String {
    match item {
        SelectionItem::Tier(tier, model) => {
            format!("[{}] {:<7} {}", index + 1, tier_title(*tier), model)
        }
        SelectionItem::Model(model) => model.to_string(),
        SelectionItem::Others => format!("[{}] Others...  browse all models", index + 1),
    }
}

/// Computes the number of visible item rows in the selection dialog body.
pub fn selection_dialog_visible_height(area: Rect) -> usize {
    (area.height.saturating_sub(6) as usize).max(1)
}

pub fn status_dialog_area(term: Rect, options_count: usize) -> Rect {
    let height = (options_count as u16 + 9).clamp(11, 18);
    let height_percent = (height * 100 / term.height.max(1)).clamp(30, 65);
    centered_rect(60, height_percent, term)
}

pub fn render_status_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::StatusDialogState,
) {
    let options_count = match &dialog.target {
        crate::app::StatusDialogTarget::PlanItem { .. } => 5,
        crate::app::StatusDialogTarget::ReviewItem { .. } => 5,
    };
    let area = status_dialog_area(frame.area(), options_count);
    frame.render_widget(Clear, area);

    let (title, raw_target_title, current_idx, options) = match &dialog.target {
        crate::app::StatusDialogTarget::PlanItem { item_title, current_status, .. } => {
            let cur = match current_status {
                tauqe_protocol::PlanItemStatus::Discussion => 0,
                tauqe_protocol::PlanItemStatus::Todo => 1,
                tauqe_protocol::PlanItemStatus::InProgress => 2,
                tauqe_protocol::PlanItemStatus::Done => 3,
                tauqe_protocol::PlanItemStatus::Cancelled => 4,
            };
            let opts = vec![
                ("DISCUSSION", Color::Magenta, Color::Black, "Under review / requires discussion"),
                ("TODO", Color::Yellow, Color::Black, "Approved / ready for execution"),
                ("IN_PROGRESS", Color::Cyan, Color::Black, "Currently in active development"),
                ("DONE", Color::Green, Color::Black, "Completed and verified"),
                ("CANCELLED", Color::DarkGray, Color::White, "Superseded or cancelled"),
            ];
            (" Change Task Status ", item_title.as_str(), cur, opts)
        }
        crate::app::StatusDialogTarget::ReviewItem { item_title, current_status, .. } => {
            let cur = match current_status {
                tauqe_protocol::ReviewStatus::Discussion => 0,
                tauqe_protocol::ReviewStatus::Todo => 1,
                tauqe_protocol::ReviewStatus::InProgress => 2,
                tauqe_protocol::ReviewStatus::Fixed => 3,
                tauqe_protocol::ReviewStatus::Rejected => 4,
            };
            let opts = vec![
                ("DISCUSSION", Color::Magenta, Color::Black, "Under review / requires discussion"),
                ("TODO", Color::Yellow, Color::Black, "Approved / ready for implementation"),
                ("IN_PROGRESS", Color::Cyan, Color::Black, "Currently being fixed"),
                ("FIXED", Color::Green, Color::Black, "Resolved and addressed in code"),
                ("REJECTED", Color::DarkGray, Color::White, "False positive / won't fix"),
            ];
            (" Change Finding Status ", item_title.as_str(), cur, opts)
        }
    };

    let outer_block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(options_count as u16),
            Constraint::Length(3),
        ])
        .split(inner_area);

    let max_title_w = chunks[0].width.saturating_sub(10) as usize;
    let target_preview = truncate_to_width(raw_target_title.trim(), max_title_w);
    let target_line = Line::from(vec![
        Span::styled(" Item: ", Style::default().fg(Color::DarkGray)),
        Span::styled(target_preview, Style::default().bold().fg(Color::White)),
    ]);
    frame.render_widget(Paragraph::new(vec![Line::raw(""), target_line, Line::raw("")]), chunks[0]);

    let mut body_lines = Vec::new();
    for (idx, (label, bg, fg, desc)) in options.iter().enumerate() {
        let is_cursor = idx == dialog.selected_index;
        let is_current = idx == current_idx;

        let cursor_prefix = if is_cursor { " ▶ " } else { "   " };
        let current_marker = if is_current { "(•) " } else { "( ) " };
        let key_shortcut = format!("[{}] ", idx + 1);

        let row_style = if is_cursor {
            Style::default().bg(Color::DarkGray).fg(Color::White).bold()
        } else {
            Style::default()
        };

        let badge_style = Style::default().bg(*bg).fg(*fg).bold();

        body_lines.push(Line::from(vec![
            Span::styled(cursor_prefix, Style::default().fg(Color::Cyan).bold()),
            Span::styled(
                current_marker,
                if is_current {
                    Style::default().fg(Color::Green).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(key_shortcut, Style::default().fg(Color::Yellow).bold()),
            Span::styled(format!(" {:<11} ", label), badge_style),
            Span::raw(" "),
            Span::styled(*desc, if is_cursor { Style::default().fg(Color::White) } else { Style::default().fg(Color::Gray) }),
        ]).style(row_style));
    }
    frame.render_widget(Paragraph::new(body_lines), chunks[1]);

    let fast_select_label = if options_count >= 5 { "1-5" } else { "1-4" };
    let actions = vec![
        Line::from(vec![
            Span::styled(format!(" [ Apply (Enter / {}) ] ", fast_select_label), Style::default().bg(Color::Green).fg(Color::Black).bold()),
            Span::raw("    "),
            Span::styled(" [ Cancel (Esc) ] ", Style::default().bg(Color::DarkGray).fg(Color::White)),
        ]),
        Line::from(Span::styled(
            format!("{} fast select • n/p or j/k navigate • Enter apply • Esc cancel", fast_select_label),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let actions_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(actions_widget, chunks[2]);
}

pub struct ConfirmModalParams<'a> {
    pub title: &'a str,
    pub border_color: Color,
    pub body_lines: Vec<Line<'a>>,
    pub confirm_label: &'a str,
    pub cancel_label: &'a str,
    pub confirm_button: crate::app::ConfirmDialogButton,
    pub destructive: bool,
    pub width: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfirmModalGeometry {
    pub area: Rect,
    pub confirm_button: Rect,
    pub cancel_button: Rect,
}

/// Computes the exact geometry and button bounds for three-tier confirmation modals,
/// shared between renderer and mouse hit-testing.
pub fn confirm_modal_geometry(
    term: Rect,
    width: u16,
    body_lines_count: usize,
    confirm_label: &str,
    cancel_label: &str,
) -> ConfirmModalGeometry {
    let needed_height = (body_lines_count as u16 + 7).clamp(10, 22);
    let height_percent = (needed_height * 100 / term.height.max(1)).clamp(25, 75);
    let area = centered_rect(width, height_percent, term);

    let inner_y = area.y + 1;
    let inner_h = area.height.saturating_sub(2);
    let inner_x = area.x + 1;
    let inner_w = area.width.saturating_sub(2);

    let actions_y = inner_y + inner_h.saturating_sub(3);
    let btn_y = actions_y;

    let l1 = confirm_label.chars().count() + 6; // " [ " + label + " ] "
    let l2 = cancel_label.chars().count() + 6;  // " [ " + label + " ] "
    let total_len = l1 + 4 + l2;                 // 4 spaces gap
    let start_x = inner_x + (inner_w.saturating_sub(total_len as u16)) / 2;

    ConfirmModalGeometry {
        area,
        confirm_button: Rect::new(start_x, btn_y, l1 as u16, 1),
        cancel_button: Rect::new(start_x + l1 as u16 + 4, btn_y, l2 as u16, 1),
    }
}

/// Standardized three-tier confirmation modal renderer (Header, Body, Actions).
pub fn render_modal_confirm(frame: &mut ratatui::Frame, params: ConfirmModalParams) {
    let geom = confirm_modal_geometry(
        frame.area(),
        params.width,
        params.body_lines.len(),
        params.confirm_label,
        params.cancel_label,
    );
    let area = geom.area;
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(format!(" {} ", params.title))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(params.border_color));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(2),
            Constraint::Length(3),
        ])
        .split(inner_area);

    let body_widget = Paragraph::new(params.body_lines)
        .alignment(ratatui::layout::Alignment::Center)
        .wrap(Wrap { trim: false });
    frame.render_widget(body_widget, chunks[0]);

    let (confirm_style, cancel_style) = match params.confirm_button {
        crate::app::ConfirmDialogButton::Confirm => {
            let bg = if params.destructive { Color::Red } else { Color::Green };
            (
                Style::default().bg(bg).fg(Color::White).bold(),
                Style::default().bg(Color::DarkGray).fg(Color::White),
            )
        }
        crate::app::ConfirmDialogButton::Cancel => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
    };

    let actions_lines = vec![
        Line::from(vec![
            Span::styled(format!(" [ {} ] ", params.confirm_label), confirm_style),
            Span::raw("    "),
            Span::styled(format!(" [ {} ] ", params.cancel_label), cancel_style),
        ]),
        Line::from(Span::styled(
            "Tab / ← / → focus • Enter/Space execute • y / n fast select • Esc cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let actions_widget = Paragraph::new(actions_lines).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(actions_widget, chunks[1]);
}

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

/// Renders editor text as lines, drawing the cursor (reversed char or a block at line end).
fn editor_lines(editor: &InputEditor, show_cursor: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for raw in editor.get_text().split('\n') {
        let end = start + raw.len();
        if show_cursor && editor.cursor >= start && editor.cursor <= end {
            let (before, rest) = raw.split_at(editor.cursor - start);
            let mut spans = vec![Span::raw(before.to_string())];
            let mut chars = rest.chars();
            match chars.next() {
                Some(c) => {
                    spans.push(Span::styled(c.to_string(), Style::default().reversed()));
                    spans.push(Span::raw(chars.as_str().to_string()));
                }
                None => spans.push(Span::styled("█", Style::default().fg(Color::Yellow))),
            }
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::raw(raw.to_string()));
        }
        start = end + 1;
    }
    lines
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

pub fn render_disconnected_popup(frame: &mut ratatui::Frame, message: &str) {
    let area = centered_rect(64, 30, frame.area());
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(" Server Disconnected ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(3)])
        .split(inner_area);

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Connection to tauqe-server was terminated",
            Style::default().bold().fg(Color::Red),
        )),
        Line::raw(""),
        Line::from(Span::styled(message, Style::default().fg(Color::White))),
    ];
    let body = Paragraph::new(lines)
        .alignment(ratatui::layout::Alignment::Center)
        .wrap(Wrap { trim: false });
    frame.render_widget(body, chunks[0]);

    let actions = vec![
        Line::from(Span::styled(
            " [ Quit TAUQE (q / Ctrl+Q) ] ",
            Style::default().bg(Color::Red).fg(Color::White).bold(),
        )),
        Line::from(Span::styled(
            "Press 'q', 'Esc' or Ctrl+Q to exit",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer, chunks[1]);
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

pub fn render_confirm_cancel_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let body_lines = vec![
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
    ];

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Confirm Interruption",
            border_color: Color::Yellow,
            body_lines,
            confirm_label: "Interrupt (Y)",
            cancel_label: "Cancel (Esc)",
            confirm_button: state.confirm_button,
            destructive: true,
            width: 58,
        },
    );
}

pub fn render_confirm_quit_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let activity = if state.review.running {
        "Code review is currently in progress."
    } else if state.model.is_busy() {
        "Model generation / turn is currently in progress."
    } else if !state.input_editor.is_empty() {
        "You have an unsubmitted prompt draft in the editor."
    } else if state.squash_dialog.as_ref().is_some_and(|d| d.applying) {
        "Squash operation is currently being applied."
    } else if state.squash_dialog.as_ref().is_some_and(|d| !d.message_editor.is_empty()) {
        "You have an uncommitted squash message in progress."
    } else {
        "Active session work may be lost."
    };

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Quit TAUQE?",
            Style::default().bold().fg(Color::Red),
        )),
        Line::raw(""),
        Line::from(activity),
        Line::from(Span::styled(
            "Exiting now will terminate the session and discard active work.",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Confirm Quit",
            border_color: Color::Red,
            body_lines,
            confirm_label: "Quit TAUQE (Y)",
            cancel_label: "Cancel (Esc)",
            confirm_button: state.confirm_button,
            destructive: true,
            width: 58,
        },
    );
}

pub fn render_confirm_undo_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Are you sure you want to undo the last AI commit?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
    ];

    if let Some(hash) = &state.model.last_commit_hash {
        body_lines.push(Line::from(vec![
            Span::raw("Commit: "),
            Span::styled(hash.clone(), Style::default().bold().fg(Color::Cyan)),
        ]));
    }
    if let Some(summary) = &state.model.last_commit_summary {
        body_lines.push(Line::from(vec![
            Span::raw("Summary: "),
            Span::styled(summary.clone(), Style::default().fg(Color::White)),
        ]));
    }
    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        "Any uncommitted changes from the pre-edit checkpoint will be restored.",
        Style::default().fg(Color::DarkGray),
    )));

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Confirm Undo",
            border_color: Color::Red,
            body_lines,
            confirm_label: "Confirm Undo (Y)",
            cancel_label: "Cancel (Esc)",
            confirm_button: state.confirm_button,
            destructive: true,
            width: 58,
        },
    );
}

pub fn render_confirm_execute_review_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let Some(target) = &state.confirm_execute_review else {
        return;
    };

    let is_single = target.items.len() == 1;
    let title_text = if is_single {
        "Execute Review Finding"
    } else {
        "Execute Review Session"
    };

    let heading = if is_single {
        "Resolve target review finding autonomously?"
    } else {
        "Resolve remaining findings sequentially with automated verification?"
    };

    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            heading,
            Style::default().bold().fg(Color::Cyan),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" Review: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&target.review_title, Style::default().bold().fg(Color::White)),
            Span::styled(format!(" [{}]", target.review_id), Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(vec![
            Span::styled(" Scope: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&target.scope_title, Style::default().bold().fg(Color::Yellow)),
            Span::styled(format!(" ({} finding(s) to resolve)", target.items.len()), Style::default().fg(Color::White)),
        ]),
    ];

    if is_single {
        let item = &target.items[0];
        if let Some(path) = &item.file_path {
            let loc = match item.line_range {
                Some((s, e)) if s == e => format!("{}:{}", path, s),
                Some((s, e)) => format!("{}:{}-{}", path, s, e),
                None => path.clone(),
            };
            body_lines.push(Line::from(vec![
                Span::styled(" Location: ", Style::default().fg(Color::DarkGray)),
                Span::styled(loc, Style::default().fg(Color::Cyan)),
            ]));
        }
        let trimmed = item.body.trim();
        if !trimmed.is_empty() {
            let first_line = trimmed.lines().next().unwrap_or("");
            body_lines.push(Line::from(vec![
                Span::styled(" Recommendation: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    truncate_to_width(first_line, 45),
                    Style::default().fg(Color::Gray),
                ),
            ]));
        }
    } else {
        body_lines.push(Line::raw(""));
        body_lines.push(Line::from(Span::styled(" Findings in execution queue:", Style::default().fg(Color::DarkGray))));
        for (i, item) in target.items.iter().take(4).enumerate() {
            body_lines.push(Line::from(vec![
                Span::styled(format!("   {}. ", i + 1), Style::default().fg(Color::Cyan)),
                Span::styled(format!("[{}] #{} {}", item.severity, item.id, truncate_to_width(&item.title, 36)), Style::default().fg(Color::White)),
            ]));
        }
        if target.items.len() > 4 {
            body_lines.push(Line::from(Span::styled(
                format!("   ... and {} more finding(s)", target.items.len() - 4),
                Style::default().fg(Color::DarkGray).italic(),
            )));
        }
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        "TAUQE will resolve each finding in an isolated turn with compiler verification gates.",
        Style::default().fg(Color::DarkGray),
    )));
    if !is_single {
        body_lines.push(Line::from(Span::styled(
            "Fail-Fast Policy: execution stops and rolls back immediately if any finding fails verification.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let confirm_label = if is_single {
        "Resolve (Enter)"
    } else {
        "Resolve Queue (Enter)"
    };

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: title_text,
            border_color: Color::Cyan,
            body_lines,
            confirm_label,
            cancel_label: "Cancel (Esc)",
            confirm_button: state.confirm_button,
            destructive: false,
            width: 66,
        },
    );
}

pub fn render_confirm_execute_scope_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let Some(target) = &state.confirm_execute_scope else {
        return;
    };

    let is_single = target.steps.len() == 1;
    let title_text = if is_single {
        "Execute Plan Step"
    } else {
        "Execute Plan Scope"
    };

    let heading = if is_single {
        "Execute target plan step autonomously?"
    } else {
        "Execute plan steps sequentially with automated verification?"
    };

    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            heading,
            Style::default().bold().fg(Color::Cyan),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" Plan: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&target.plan_title, Style::default().bold().fg(Color::White)),
            Span::styled(format!(" [{}]", target.plan_id), Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(vec![
            Span::styled(" Scope: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&target.scope_title, Style::default().bold().fg(Color::Yellow)),
            Span::styled(format!(" ({} step(s) to execute)", target.steps.len()), Style::default().fg(Color::White)),
        ]),
    ];

    if is_single {
        let step = &target.steps[0];
        if let Some(details) = &step.details {
            let trimmed = details.trim();
            if !trimmed.is_empty() {
                let first_line = trimmed.lines().next().unwrap_or("");
                body_lines.push(Line::from(vec![
                    Span::styled(" Details: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        truncate_to_width(first_line, 45),
                        Style::default().fg(Color::Gray),
                    ),
                ]));
            }
        }
    } else {
        body_lines.push(Line::raw(""));
        body_lines.push(Line::from(Span::styled(" Steps in execution queue:", Style::default().fg(Color::DarkGray))));
        for (i, step) in target.steps.iter().take(4).enumerate() {
            body_lines.push(Line::from(vec![
                Span::styled(format!("   {}. ", i + 1), Style::default().fg(Color::Cyan)),
                Span::styled(format!("#{} {}", step.id, truncate_to_width(&step.title, 40)), Style::default().fg(Color::White)),
            ]));
        }
        if target.steps.len() > 4 {
            body_lines.push(Line::from(Span::styled(
                format!("   ... and {} more step(s)", target.steps.len() - 4),
                Style::default().fg(Color::DarkGray).italic(),
            )));
        }
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        "TAUQE will execute each step in an isolated turn with compiler verification gates.",
        Style::default().fg(Color::DarkGray),
    )));
    if !is_single {
        body_lines.push(Line::from(Span::styled(
            "Execution stops immediately if any step fails or is interrupted.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let confirm_label = if is_single {
        "Execute (Enter)"
    } else {
        "Execute Scope (Enter)"
    };

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: title_text,
            border_color: Color::Cyan,
            body_lines,
            confirm_label,
            cancel_label: "Cancel (Esc)",
            confirm_button: state.confirm_button,
            destructive: false,
            width: 66,
        },
    );
}

pub fn render_confirm_delete_plan_popup(frame: &mut ratatui::Frame, plan_id: &str, state: &AppState) {
    let plan_title = state
        .plans_view
        .plans
        .iter()
        .find(|p| p.id == plan_id)
        .map(|p| p.title.clone())
        .unwrap_or_else(|| plan_id.to_string());

    let body_lines = vec![
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
    ];

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Confirm Delete Plan",
            border_color: Color::Red,
            body_lines,
            confirm_label: "Confirm Delete (Y)",
            cancel_label: "Cancel (Esc)",
            confirm_button: state.confirm_button,
            destructive: true,
            width: 58,
        },
    );
}

pub fn render_confirm_delete_review_popup(frame: &mut ratatui::Frame, session_id: &str, state: &AppState) {
    let review_title = state
        .review
        .sessions
        .iter()
        .find(|s| s.id == session_id)
        .map(|s| s.title.clone())
        .or_else(|| state.review.session.as_ref().filter(|s| s.id == session_id).map(|s| s.title.clone()))
        .unwrap_or_else(|| session_id.to_string());

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Are you sure you want to delete this review session?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Review: "),
            Span::styled(format!("{} ({})", review_title, session_id), Style::default().bold().fg(Color::Cyan)),
        ]),
        Line::from(Span::styled(
            "The review file in .tauqe/reviews/ will be permanently removed.",
            Style::default().fg(Color::Gray),
        )),
    ];

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Confirm Delete Review",
            border_color: Color::Red,
            body_lines,
            confirm_label: "Confirm Delete (Y)",
            cancel_label: "Cancel (Esc)",
            confirm_button: state.confirm_button,
            destructive: true,
            width: 58,
        },
    );
}

pub fn render_confirm_clear_auto_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let auto_count = state
        .context
        .items
        .iter()
        .filter(|i| i.layer == tauqe_protocol::ContextLayer::Auto)
        .count();

    let body_lines = vec![
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
    ];

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Confirm Clear Auto Context",
            border_color: Color::Yellow,
            body_lines,
            confirm_label: "Confirm Clear (Y)",
            cancel_label: "Cancel (Esc)",
            confirm_button: state.confirm_button,
            destructive: false,
            width: 58,
        },
    );
}

pub fn render_confirm_clear_history_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Are you sure you want to clear conversation history?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from("This will permanently erase session history:"),
        Line::from(Span::styled(
            "  .tauqe/history.jsonl",
            Style::default().fg(Color::DarkGray).italic(),
        )),
        Line::from(Span::styled(
            "and clear the current model view output.",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Confirm Clear History",
            border_color: Color::Red,
            body_lines,
            confirm_label: "Confirm Clear (Y)",
            cancel_label: "Cancel (Esc)",
            confirm_button: state.confirm_button,
            destructive: true,
            width: 58,
        },
    );
}

pub fn render_selection_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::SelectionDialogState,
) {
    let area = selection_dialog_area(frame.area(), dialog.items.len());
    frame.render_widget(Clear, area);

    let title = match dialog.kind {
        SelectionDialogKind::ModelTiers => " Select Model Tier ".to_string(),
        SelectionDialogKind::ModelCatalog => format!(
            " Select Model ({}/{}) ",
            dialog.selected_index + 1,
            dialog.items.len()
        ),
    };

    let outer_block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(2),
            Constraint::Length(3),
        ])
        .split(inner_area);

    let intro = match dialog.kind {
        SelectionDialogKind::ModelTiers => " Roles map to [models] in tauqe.toml",
        SelectionDialogKind::ModelCatalog => " All available models",
    };
    frame.render_widget(
        Paragraph::new(Span::styled(intro, Style::default().fg(Color::DarkGray))),
        chunks[0],
    );

    let inner_height = chunks[1].height as usize;
    let max_scroll = dialog.items.len().saturating_sub(inner_height);
    let scroll_offset = dialog.scroll_offset.min(max_scroll);

    let mut lines = Vec::new();
    let visible_items = dialog
        .items
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(inner_height);

    for (idx, item) in visible_items {
        let is_selected = idx == dialog.selected_index;
        let prefix = if is_selected { " ▶ " } else { "   " };
        let line_style = if is_selected {
            Style::default().bg(Color::DarkGray).fg(Color::White).bold()
        } else {
            Style::default().fg(Color::Gray)
        };

        lines.push(
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::Green).bold()),
                Span::styled(selection_item_label(item, idx), line_style),
            ])
            .style(line_style),
        );
    }

    let list_widget = Paragraph::new(lines);
    frame.render_widget(list_widget, chunks[1]);

    let actions = vec![
        Line::from(vec![
            Span::styled(" [ Select (Enter) ] ", Style::default().bg(Color::Green).fg(Color::Black).bold()),
            Span::raw("    "),
            Span::styled(" [ Cancel (Esc) ] ", Style::default().bg(Color::DarkGray).fg(Color::White)),
        ]),
        Line::from(Span::styled(
            match dialog.kind {
                SelectionDialogKind::ModelTiers => "1-4 quick select • n/p navigate • Esc close",
                SelectionDialogKind::ModelCatalog => "n/p navigate • Enter select • Esc/q dismiss",
            },
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let actions_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(actions_widget, chunks[2]);
}

pub fn render_plan_refine_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::ui::plans::PlanRefineDialogState,
) {
    let area = centered_rect(68, 56, frame.area());
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(" Deep Plan Refinement (AI Architect) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(6), Constraint::Length(3)])
        .split(inner_area);

    let (model_label, _) = dialog
        .models
        .get(dialog.selected_model_index)
        .cloned()
        .unwrap_or_else(|| ("Default".to_string(), None));
    let dim = Style::default().fg(Color::DarkGray);

    let header_lines = vec![
        Line::from(vec![
            Span::styled(" Plan: ", dim),
            Span::styled(&dialog.plan_title, Style::default().bold().fg(Color::White)),
            Span::styled(format!(" [{}]", dialog.plan_id), dim),
        ]),
        Line::from(vec![
            Span::styled(" Model: ", dim),
            Span::styled(
                format!("◄ {} ►", model_label),
                Style::default().fg(Color::Green).bold(),
            ),
            Span::styled("  (Tab / Shift+Tab to switch)", dim),
        ]),
        Line::from(Span::styled(
            " Decomposes high-level tasks into verified atomic engineering steps without altering agreed architecture.",
            dim,
        )),
    ];
    frame.render_widget(Paragraph::new(header_lines), chunks[0]);

    let mut editor_content = Vec::new();
    if dialog.instructions_editor.is_empty() {
        editor_content.push(Line::from(vec![
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(
                " Additional refinement directives (e.g. 'split step 2 into atomic subtasks') [optional]...",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    } else {
        editor_content.extend(editor_lines(&dialog.instructions_editor, true));
    }

    let editor_block = Block::default()
        .title(" Architect Instructions (optional) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    let editor_widget = Paragraph::new(editor_content).block(editor_block).wrap(Wrap { trim: false });
    frame.render_widget(editor_widget, chunks[1]);

    let (confirm_style, cancel_style) = match dialog.confirm_button {
        crate::app::ConfirmDialogButton::Confirm => (
            Style::default().bg(Color::Green).fg(Color::Black).bold(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        crate::app::ConfirmDialogButton::Cancel => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
    };

    let actions = vec![
        Line::from(vec![
            Span::styled(" [ Start Refinement ] ", confirm_style),
            Span::raw("    "),
            Span::styled(" [ Cancel ] ", cancel_style),
        ]),
        Line::from(Span::styled(
            "Tab / ← / → switch button • Enter execute focused • Esc cancel • ↑ / ↓ switch model",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer_widget, chunks[2]);
}

pub fn render_plan_refine_blocked_dialog(
    frame: &mut ratatui::Frame,
    blocked: &crate::ui::plans::PlanRefineBlockedState,
) {
    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "AI Architect Refused Decomposition (Blockers Detected)",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("Plan: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} [{}]", blocked.plan_title, blocked.plan_id),
                Style::default().bold().fg(Color::White),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled("Reason / Contradiction:", Style::default().fg(Color::Red).bold())),
    ];

    for line in blocked.reason.lines() {
        body_lines.push(Line::from(Span::styled(format!("  {}", line), Style::default().fg(Color::White))));
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        "TAUQE prohibits speculative guessing. Resolve architectural blockers with AI in Develop.",
        Style::default().fg(Color::DarkGray),
    )));

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Plan Refinement Blocked",
            border_color: Color::Red,
            body_lines,
            confirm_label: "Discuss in Develop (D)",
            cancel_label: "Dismiss (Esc)",
            confirm_button: crate::app::ConfirmDialogButton::Cancel,
            destructive: false,
            width: 70,
        },
    );
}

pub fn render_discuss_plan_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::DiscussPlanDialogState,
) {
    let area = centered_rect(68, 56, frame.area());
    frame.render_widget(Clear, area);

    let dialog_title = if let Some((step_id, _)) = dialog.focused_items.first() {
        format!(" Discuss Step: [{}] #{} ", dialog.plan_id, step_id)
    } else {
        format!(" Discuss Plan: [{}] ", dialog.plan_id)
    };

    let outer_block = Block::default()
        .title(dialog_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(6), Constraint::Length(3)])
        .split(inner_area);

    let mut header_lines = vec![
        Line::from(vec![
            Span::styled(" Plan: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&dialog.plan_title, Style::default().bold().fg(Color::White)),
        ]),
    ];

    if dialog.focused_items.is_empty() {
        header_lines.push(Line::from(vec![
            Span::styled(" Scope: ", Style::default().fg(Color::DarkGray)),
            Span::styled("Entire plan", Style::default().fg(Color::Yellow).bold()),
            Span::styled(" (discussing architecture and overall strategy)", Style::default().fg(Color::DarkGray)),
        ]));
    } else if let Some((id, title)) = dialog.focused_items.first() {
        header_lines.push(Line::from(vec![
            Span::styled(" Target step: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncate_to_width(&format!("#{} {}", id, title), chunks[0].width.saturating_sub(18) as usize),
                Style::default().bold().fg(Color::Green),
            ),
        ]));
    }

    let header_widget = Paragraph::new(header_lines);
    frame.render_widget(header_widget, chunks[0]);

    let mut editor_content = Vec::new();
    if dialog.prompt_editor.is_empty() {
        editor_content.push(Line::from(vec![
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(
                " Enter your architectural questions, task critique, or refinement proposals for Tauqe AI...",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    } else {
        editor_content.extend(editor_lines(&dialog.prompt_editor, true));
    }

    let editor_block = Block::default()
        .title(" Your Comments / Questions (Enter send, Shift+Enter / Alt+Enter / C-J / C-N newline) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    let editor_widget = Paragraph::new(editor_content).block(editor_block).wrap(Wrap { trim: false });
    frame.render_widget(editor_widget, chunks[1]);

    let actions = vec![
        Line::from(vec![
            Span::styled(
                " [ Discuss in Develop (Enter / C-Enter) ] ",
                Style::default().bg(Color::Green).fg(Color::Black).bold(),
            ),
            Span::raw("    "),
            Span::styled(
                " [ Cancel (Esc) ] ",
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
        ]),
        Line::from(Span::styled(
            "Enter send • Shift+Enter / Alt+Enter / C-J / C-N newline • Esc cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer_widget, chunks[2]);
}

pub fn render_discuss_review_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::DiscussReviewDialogState,
) {
    let area = centered_rect(68, 56, frame.area());
    frame.render_widget(Clear, area);

    let dialog_title = if let Some((item_id, _)) = dialog.focused_items.first() {
        format!(" Discuss Finding: [{}] #{} ", dialog.review_id, item_id)
    } else {
        format!(" Discuss Review Session: [{}] ", dialog.review_id)
    };

    let outer_block = Block::default()
        .title(dialog_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(6), Constraint::Length(3)])
        .split(inner_area);

    let mut header_lines = vec![
        Line::from(vec![
            Span::styled(" Review: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&dialog.review_title, Style::default().bold().fg(Color::White)),
        ]),
    ];

    if dialog.focused_items.is_empty() {
        header_lines.push(Line::from(vec![
            Span::styled(" Scope: ", Style::default().fg(Color::DarkGray)),
            Span::styled("Entire review session", Style::default().fg(Color::Yellow).bold()),
            Span::styled(" (discussing code quality findings and resolution plan)", Style::default().fg(Color::DarkGray)),
        ]));
    } else if let Some((id, title)) = dialog.focused_items.first() {
        header_lines.push(Line::from(vec![
            Span::styled(" Target finding: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncate_to_width(&format!("#{} {}", id, title), chunks[0].width.saturating_sub(18) as usize),
                Style::default().bold().fg(Color::Green),
            ),
        ]));
    }

    let header_widget = Paragraph::new(header_lines);
    frame.render_widget(header_widget, chunks[0]);

    let mut editor_content = Vec::new();
    if dialog.prompt_editor.is_empty() {
        editor_content.push(Line::from(vec![
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(
                " Enter your questions, counter-arguments, or instructions for Tauqe AI regarding this finding...",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    } else {
        editor_content.extend(editor_lines(&dialog.prompt_editor, true));
    }

    let editor_block = Block::default()
        .title(" Your Comments / Questions (Enter send, Shift+Enter / Alt+Enter / C-J newline) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    let editor_widget = Paragraph::new(editor_content).block(editor_block).wrap(Wrap { trim: false });
    frame.render_widget(editor_widget, chunks[1]);

    let actions = vec![
        Line::from(vec![
            Span::styled(
                " [ Discuss in Develop (Enter / C-Enter) ] ",
                Style::default().bg(Color::Green).fg(Color::Black).bold(),
            ),
            Span::raw("    "),
            Span::styled(
                " [ Cancel (Esc) ] ",
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
        ]),
        Line::from(Span::styled(
            "Enter send • Shift+Enter / Alt+Enter / C-J newline • Esc cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer_widget, chunks[2]);
}

pub fn render_review_dialog(frame: &mut ratatui::Frame, dialog: &crate::app::ReviewDialogState) {
    let area = centered_rect(64, 54, frame.area());
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(" Launch Code Review ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(3)])
        .split(inner_area);

    let model = dialog
        .models
        .get(dialog.model_index)
        .map(|m| m.to_string())
        .unwrap_or_else(|| "(none)".to_string());
    let dim = Style::default().fg(Color::DarkGray);

    let mut lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Run automated code review on active context files?",
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
            Span::styled("  (Tab / Shift+Tab to switch)", dim),
        ]),
        Line::from(Span::styled(
            "  Only file contents are sent: history and repo map are excluded.",
            dim,
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  Additional review instructions (optional):",
            Style::default().fg(Color::Cyan).bold(),
        )),
    ];

    if dialog.prompt_editor.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("  > "),
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(
                " (type additional review focus or leave empty)...",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    } else {
        for l in editor_lines(&dialog.prompt_editor, true) {
            let mut spans = vec![Span::raw("  > ")];
            spans.extend(l.spans);
            lines.push(Line::from(spans));
        }
    }

    let body = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(body, chunks[0]);

    let actions = vec![
        Line::from(vec![
            Span::styled(
                " [ Start Review (Enter) ] ",
                Style::default().bg(Color::Green).fg(Color::Black).bold(),
            ),
            Span::raw("    "),
            Span::styled(
                " [ Cancel (Esc) ] ",
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
        ]),
        Line::from(Span::styled(
            "Enter start review • Esc cancel • Tab / Shift+Tab or ↑/↓ switch model",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer, chunks[1]);
}

pub const GLOBAL_COMMANDS: &[crate::app::KeyCommand] = &[
    crate::app::KeyCommand { key: "Ctrl+1..5 / Alt+1..5 / F1..F5", description: "Switch views: Develop │ Context │ Review │ Plans │ History" },
    crate::app::KeyCommand { key: "Ctrl+M / Alt+M", description: "Select active model" },
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

    let mut help_lines: Vec<Line<'static>> = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Global Navigation",
            Style::default().fg(Color::Cyan).bold(),
        )),
    ];
    push_command_lines(&mut help_lines, GLOBAL_COMMANDS, col_w, desc_w, primary_modifier);

    help_lines.push(Line::raw(""));
    help_lines.push(Line::from(Span::styled(
        "Active Mode Shortcuts",
        Style::default().fg(Color::Yellow).bold(),
    )));
    push_command_lines(&mut help_lines, mode_commands, col_w, desc_w, primary_modifier);

    let outer_block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(3)])
        .split(inner_area);

    let inner_h = chunks[0].height as usize;
    let max_scroll = help_lines
        .len()
        .saturating_sub(inner_h)
        .min(u16::MAX as usize) as u16;
    *scroll = (*scroll).min(max_scroll);

    let help_widget = Paragraph::new(help_lines).scroll((*scroll, 0));
    frame.render_widget(help_widget, chunks[0]);

    let actions = vec![
        Line::from(Span::styled(
            " [ Close Help (Esc / q / ?) ] ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        )),
        Line::from(Span::styled(
            "n/p or j/k scroll • PgUp/PgDn page scroll • Home top • Esc/q close",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer, chunks[1]);
}
