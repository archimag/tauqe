use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{AppState, ViewMode};

use super::centered_rect;

pub fn render_squash_popup(frame: &mut ratatui::Frame, dialog: &crate::app::SquashDialogState) {
    let area = centered_rect(85, 85, frame.area());
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
        Some(s) => format!("1: Session ({})", &s[..7.min(s.len())]),
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

    let (style_s, style_u, style_c) = match dialog.base_mode {
        crate::app::SquashBaseMode::Session => (
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        crate::app::SquashBaseMode::Upstream => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        crate::app::SquashBaseMode::Custom => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
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
    let file_list_widget = Paragraph::new(file_lines).block(
        Block::default()
            .title(format!(" Files Changed ({}) ", dialog.files.len()))
            .borders(Borders::ALL)
            .border_style(if is_files_focus { Style::default().fg(Color::Cyan) } else { Style::default() }),
    );
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
        for l in dialog.message_buffer.lines() {
            msg_lines.push(Line::from(Span::raw(l)));
        }
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
        Span::styled(" Space ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Fold  "),
        Span::styled(" Tab ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Focus  "),
        Span::styled(" g ", Style::default().bg(Color::Yellow).fg(Color::Black).bold()),
        Span::raw(" Generate Msg (AI)  "),
        Span::styled(" e ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Edit  "),
        Span::styled(" Enter ", Style::default().bg(Color::Green).fg(Color::Black).bold()),
        Span::raw(" Apply Squash  "),
        Span::styled(" Esc ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Close"),
    ]);
    let footer_widget = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer_widget, chunks[3]);
}

pub fn render_confirm_cancel_popup(frame: &mut ratatui::Frame, _state: &AppState) {
    let area = centered_rect(58, 28, frame.area());
    frame.render_widget(Clear, area);

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
            Span::styled(
                " [Y] / Enter ",
                Style::default().bg(Color::Red).fg(Color::White).bold(),
            ),
            Span::raw(" Interrupt    "),
            Span::styled(
                " [N] / Esc ",
                Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
            ),
            Span::raw(" Continue"),
        ]),
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
        Span::styled(
            " [Y] / Enter ",
            Style::default().bg(Color::Red).fg(Color::White).bold(),
        ),
        Span::raw(" Confirm Undo    "),
        Span::styled(
            " [N] / Esc ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        ),
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

pub fn render_confirm_clear_history_popup(frame: &mut ratatui::Frame, _state: &AppState) {
    let area = centered_rect(58, 30, frame.area());
    frame.render_widget(Clear, area);

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
            Span::styled(
                " [Y] / Enter ",
                Style::default().bg(Color::Red).fg(Color::White).bold(),
            ),
            Span::raw(" Confirm Clear    "),
            Span::styled(
                " [N] / Esc ",
                Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
            ),
            Span::raw(" Cancel"),
        ]),
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
    let item_count = dialog.items.len();
    let height = (item_count as u16 + 4).clamp(5, 16);
    let area = centered_rect(
        50,
        (height * 100 / frame.area().height).clamp(15, 60),
        frame.area(),
    );
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
    crate::app::KeyCommand { key: "Ctrl+1 / 2 / 3 / 4", description: "Switch views: Develop │ Context │ History │ Review" },
    crate::app::KeyCommand { key: "Ctrl+M", description: "Select active model" },
    crate::app::KeyCommand { key: "F6 / Ctrl+S", description: "Squash commits dialog" },
    crate::app::KeyCommand { key: "Ctrl+O", description: "Reload configuration from disk" },
    crate::app::KeyCommand { key: "? / Esc", description: "Open / close this help dialog" },
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
        ViewMode::Onboarding => (" Help: Setup ", ONBOARDING_COMMANDS),
    }
}

pub fn render_help_popup(frame: &mut ratatui::Frame, mode: ViewMode) {
    let (title, mode_commands) = get_mode_key_commands(mode);
    let area = centered_rect(72, 70, frame.area());
    frame.render_widget(Clear, area);

    let mut help_lines = vec![
        Line::from(Span::styled(
            "Global Navigation",
            Style::default().fg(Color::Cyan).bold(),
        )),
    ];

    for cmd in GLOBAL_COMMANDS {
        help_lines.push(Line::from(vec![
            Span::styled(format!("  {:<20}", cmd.key), Style::default().fg(Color::White).bold()),
            Span::styled(cmd.description, Style::default().fg(Color::Gray)),
        ]));
    }

    help_lines.push(Line::raw(""));
    help_lines.push(Line::from(Span::styled(
        "Active Mode Shortcuts",
        Style::default().fg(Color::Yellow).bold(),
    )));

    for cmd in mode_commands {
        help_lines.push(Line::from(vec![
            Span::styled(format!("  {:<20}", cmd.key), Style::default().fg(Color::White).bold()),
            Span::styled(cmd.description, Style::default().fg(Color::Gray)),
        ]));
    }

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
