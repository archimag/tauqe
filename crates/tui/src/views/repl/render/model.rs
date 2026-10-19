use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use crate::theme::engine::current_engine;
use crate::ui::format::format_tokens;

use super::super::state::{EditOp, FileEditStatus, ReplView, ToolchainState};
use super::indicator_style;
use super::SPINNER_FRAMES;

pub fn compute_model_lines(model: &mut ReplView) -> Vec<Line<'static>> {
    let engine = current_engine();
    let palette = engine.palette();

    let mut model_lines: Vec<Line<'static>> = Vec::new();

    if let Some(err) = &model.error {
        model_lines.push(Line::from(vec![
            Span::raw("Error: "),
            Span::styled(err.clone(), engine.fg(palette.status_error)),
        ]));
    }

    if let Some(toolchain) = &model.toolchain {
        let (badge, text) = match toolchain {
            ToolchainState::Running { command } => {
                let spin = SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()];
                (
                    Span::styled(
                        format!(" [{}] TOOLCHAIN ", spin),
                        indicator_style(&engine, palette.status_info),
                    ),
                    format!("Running `{}`...", command),
                )
            }
            ToolchainState::Passed { command } => (
                Span::styled(
                    " [TOOLCHAIN: OK] ",
                    indicator_style(&engine, palette.status_success),
                ),
                format!("`{}` passed", command),
            ),
            ToolchainState::Failed { command, message } => (
                Span::styled(
                    " [TOOLCHAIN: FAILED] ",
                    indicator_style(&engine, palette.status_error),
                ),
                message
                    .clone()
                    .unwrap_or_else(|| format!("`{}` failed", command)),
            ),
        };

        model_lines.push(Line::from(vec![
            badge,
            Span::raw(" "),
            Span::styled(text, Style::default().bold()),
        ]));
    }

    // 1. Model response text is always displayed at the top
    if !model.markdown_lines.is_empty() {
        model_lines.extend(model.markdown_lines.iter().cloned());
    } else if !model.text.is_empty() {
        for line in model.text.lines() {
            model_lines.push(Line::from(Span::raw(line.to_string())));
        }
    }

    // 2. Reasoning stream is displayed below the response text
    if !model.reasoning.is_empty() {
        if !model.text.is_empty() || !model_lines.is_empty() {
            model_lines.push(Line::raw(""));
        }
        model_lines.extend(model.reasoning.render_lines());
    }

    if model.edits_active || !model.files.is_empty() {
        model_lines.push(Line::raw(""));

        let total_files = model.files.len();
        let ok_files = model
            .files
            .iter()
            .filter(|f| f.status == FileEditStatus::Ok)
            .count();

        let header_badge = match model.edit_final_applied {
            Some(true) => {
                if let Some(hash) = &model.last_commit_hash {
                    Span::styled(
                        format!(" [COMMITTED: {}] ", hash),
                        indicator_style(&engine, palette.status_success),
                    )
                } else {
                    Span::styled(
                        " [APPLIED] ",
                        indicator_style(&engine, palette.status_success),
                    )
                }
            }
            Some(false) => Span::styled(
                " [REJECTED] ",
                indicator_style(&engine, palette.status_error),
            ),
            None => {
                let spin = SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()];
                Span::styled(
                    format!(" [{}] Modifying ", spin),
                    indicator_style(&engine, palette.status_warning),
                )
            }
        };

        let header_spans = vec![
            header_badge,
            Span::raw(" "),
            Span::styled(
                format!(
                    "{} ({}/{} files)",
                    crate::i18n::active_t("repl.proposed_edits"),
                    ok_files,
                    total_files
                ),
                engine.fg(palette.status_info).bold(),
            ),
        ];

        model_lines.push(Line::from(header_spans));

        if let Some(summary) = &model.last_commit_summary {
            model_lines.push(Line::from(vec![
                Span::raw("  Summary: "),
                Span::styled(summary.clone(), Style::default().bold()),
            ]));
        }

        for (idx, file) in model.files.iter().enumerate() {
            let is_selected = idx == model.selected_file_index;
            let fold_icon = if file.expanded { "▼ " } else { "▶ " };
            let cursor_prefix = if is_selected { "> " } else { "  " };

            let (status_icon, status_style) = match file.status {
                FileEditStatus::Ok => ("✓", engine.fg(palette.status_success).bold()),
                FileEditStatus::Error => ("✗", engine.fg(palette.status_error).bold()),
                FileEditStatus::Running | FileEditStatus::Retrying => (
                    SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()],
                    engine.fg(palette.status_warning).bold(),
                ),
            };

            let (op_label, op_style) = match file.op_type {
                EditOp::Create => ("[NEW] ", engine.fg(palette.status_success).bold()),
                EditOp::Delete => ("[DEL] ", engine.fg(palette.status_error).bold()),
                EditOp::Move => ("[MOVE] ", engine.fg(palette.status_info).bold()),
                EditOp::Overwrite => {
                    ("[OVERWRITE] ", engine.fg(palette.status_warning).bold())
                }
                EditOp::Replace => ("[EDIT] ", engine.fg(palette.button_active_bg).bold()),
            };

            let hunk_count = file.hunks.len();
            let hunk_label = match file.op_type {
                EditOp::Move => "renamed".to_string(),
                EditOp::Overwrite => "rewritten".to_string(),
                EditOp::Delete => "deleted".to_string(),
                EditOp::Create => {
                    let line_count = file
                        .hunks
                        .first()
                        .map(|h| h.new_text.lines().count())
                        .unwrap_or(0);
                    if line_count == 1 {
                        "1 line".to_string()
                    } else {
                        format!("{} lines", line_count)
                    }
                }
                EditOp::Replace => {
                    if hunk_count == 1 {
                        "1 hunk".to_string()
                    } else {
                        format!("{} hunks", hunk_count)
                    }
                }
            };

            let file_line_style = if is_selected {
                engine.bg(palette.overlay).bold()
            } else {
                Style::default()
            };

            let mut file_spans = vec![
                Span::raw("  "),
                Span::styled(cursor_prefix, engine.fg(palette.status_info)),
                Span::styled(fold_icon, engine.fg(palette.text_muted)),
                Span::styled(op_label, op_style),
                Span::styled(file.path.clone(), Style::default().bold()),
                Span::raw(" "),
                Span::styled(
                    format!("({})", hunk_label),
                    engine.fg(palette.text_muted),
                ),
                Span::raw(" "),
                Span::styled(status_icon, status_style),
            ];

            if file.status != FileEditStatus::Ok {
                if let Some(retry) = &file.retry_info {
                    file_spans.push(Span::raw(" "));
                    file_spans.push(Span::styled(
                        format!("(Retrying {})", retry),
                        engine.fg(palette.status_warning).bold(),
                    ));
                }
            }

            model_lines.push(Line::from(file_spans).style(file_line_style));

            if file.status != FileEditStatus::Ok {
                if let Some(err_msg) = &file.error {
                    model_lines.push(Line::from(vec![
                        Span::raw("      "),
                        Span::styled(
                            format!("{} ", crate::i18n::active_t("repl.validation_error")),
                            engine.fg(palette.status_error).bold(),
                        ),
                        Span::styled(err_msg.clone(), engine.fg(palette.status_warning)),
                    ]));
                }
            }

            if file.expanded {
                model_lines.extend(model.diff_cache.lines_for(file));
            }
        }
    }

    // The usage line mirrors the daemon's single accumulator: the operation in
    // flight while a turn runs, otherwise the last completed one. An empty
    // snapshot renders no line at all (c-1, c-3).
    let totals = model.usage_totals.operation_totals().clone();

    if !totals.is_empty() {
        model_lines.push(Line::raw(""));
        let mut usage_spans = vec![
            Span::styled(
                format!("{}: ", crate::i18n::active_t("repl.usage.tokens")),
                engine.fg(palette.text_muted).bold(),
            ),
            Span::styled(
                format!(
                    "{} {}, {} {}",
                    format_tokens(totals.prompt_tokens),
                    crate::i18n::active_t("repl.usage.prompt"),
                    format_tokens(totals.completion_tokens),
                    crate::i18n::active_t("repl.usage.completion"),
                ),
                engine.fg(palette.text_muted),
            ),
        ];

        if let Some(reasoning_tokens) = totals.reasoning_tokens.filter(|tokens| *tokens > 0) {
            usage_spans.push(Span::styled(
                format!(
                    ", {} {}",
                    format_tokens(reasoning_tokens),
                    crate::i18n::active_t("repl.usage.reasoning")
                ),
                engine.fg(palette.text_secondary),
            ));
        }

        if let Some(cached_tokens) = totals.cached_tokens.filter(|tokens| *tokens > 0) {
            usage_spans.push(Span::styled(
                format!(
                    ", {} {}",
                    format_tokens(cached_tokens),
                    crate::i18n::active_t("repl.usage.cached")
                ),
                engine.fg(palette.status_info),
            ));
        }

        model_lines.push(Line::from(usage_spans));
    }

    model_lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::model::UsageTotals;

    #[test]
    fn test_compute_model_lines_empty_when_splash_visible() {
        let mut model = ReplView::default();
        assert!(model.splash_visible());
        let lines = compute_model_lines(&mut model);
        assert!(
            lines.is_empty(),
            "splash state must leave model lines empty as splash is rendered separately"
        );
    }

    #[test]
    fn test_compute_model_lines_yields_on_output() {
        let mut model = ReplView {
            text: "Hello from model".to_string(),
            ..ReplView::default()
        };
        assert!(!model.splash_visible());
        let lines = compute_model_lines(&mut model);
        assert!(!lines.is_empty());
        let rendered: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(rendered, "Hello from model");
    }

    #[test]
    fn test_compute_model_lines_yields_on_error() {
        let mut model = ReplView {
            error: Some("Failed to connect".to_string()),
            ..ReplView::default()
        };
        assert!(!model.splash_visible());
        let lines = compute_model_lines(&mut model);
        assert!(!lines.is_empty());
        assert!(lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("Failed to connect"))));
    }

    #[test]
    fn test_compute_model_lines_yields_on_toolchain() {
        let mut model = ReplView {
            toolchain: Some(ToolchainState::Running {
                command: "cargo check".to_string(),
            }),
            ..ReplView::default()
        };
        assert!(!model.splash_visible());
        let lines = compute_model_lines(&mut model);
        assert!(!lines.is_empty());
        assert!(lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("cargo check"))));
    }

    #[test]
    fn usage_line_prefers_the_in_flight_operation() {
        let mut model = ReplView {
            text: "answer".to_string(),
            ..ReplView::default()
        };
        model.usage_totals = tauqe_protocol::model::UsageTotalsSnapshot {
            session: UsageTotals {
                total_tokens: 900,
                ..UsageTotals::default()
            },
            prev: UsageTotals {
                total_tokens: 300,
                ..UsageTotals::default()
            },
            cur: Some(UsageTotals {
                prompt_tokens: 7,
                completion_tokens: 3,
                total_tokens: 10,
                ..UsageTotals::default()
            }),
            operations: 1,
        };

        let lines = compute_model_lines(&mut model);
        let rendered: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.contains("7 prompt"), "{rendered}");
        assert!(rendered.contains("3 completion"), "{rendered}");
    }
}
