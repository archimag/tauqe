use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};

use crate::app::AppState;
use crate::ui::header::truncate_to_width;

use super::{render_modal_confirm, ConfirmModalParams};

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
