use crate::theme::engine::{current_engine, ThemeEngine};
use crate::ui::format::format_tokens;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};

use super::super::state::{EditOp, FileEditStatus, ReplView, StagedFileSpan, ToolchainState};
use super::indicator_style;
use super::SPINNER_FRAMES;

/// Resolves the compact operation badge of a staged file.
///
/// The Viewport file row, the sticky overlay of the reading position, and the
/// diff inspector file pane share this single mapping, so a badge can never name
/// a different operation than the row it summarizes (c-1, c-3). The label is
/// resolved through the catalog and the separator column is appended here, so a
/// translated badge never has to preserve trailing whitespace (Conventions §9).
pub(crate) fn file_op_badge(engine: &ThemeEngine, op: &EditOp) -> Span<'static> {
    let palette = engine.palette();
    let (key, color) = match op {
        EditOp::Create => ("repl.op.create", palette.status_success),
        EditOp::Delete => ("repl.op.delete", palette.status_error),
        EditOp::Move => ("repl.op.move", palette.status_info),
        EditOp::Overwrite => ("repl.op.overwrite", palette.status_warning),
        EditOp::Replace => ("repl.op.replace", palette.button_active_bg),
    };
    Span::styled(
        format!("{} ", crate::i18n::active_t(key)),
        engine.fg(color).bold(),
    )
}

pub fn compute_model_lines(model: &mut ReplView) -> Vec<Line<'static>> {
    let engine = current_engine();
    let palette = engine.palette();

    let mut model_lines: Vec<Line<'static>> = Vec::new();

    if let Some(err) = &model.error {
        model_lines.push(Line::from(vec![
            Span::raw(crate::i18n::active_t("repl.error_label")),
            Span::styled(err.clone(), engine.fg(palette.status_error)),
        ]));
    }

    if let Some(toolchain) = &model.toolchain {
        let (badge, text) = match toolchain {
            ToolchainState::Running { command } => {
                let spin = SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()];
                (
                    Span::styled(
                        crate::i18n::active_t("repl.toolchain_badge_running")
                            .replacen("{}", spin, 1),
                        indicator_style(&engine, palette.status_info),
                    ),
                    crate::i18n::active_t("repl.toolchain_running").replacen(
                        "{}",
                        command.as_str(),
                        1,
                    ),
                )
            }
            ToolchainState::Passed { command } => (
                Span::styled(
                    crate::i18n::active_t("repl.toolchain_badge_passed"),
                    indicator_style(&engine, palette.status_success),
                ),
                crate::i18n::active_t("repl.toolchain_passed").replacen("{}", command.as_str(), 1),
            ),
            ToolchainState::Failed { command, message } => (
                Span::styled(
                    crate::i18n::active_t("repl.toolchain_badge_failed"),
                    indicator_style(&engine, palette.status_error),
                ),
                message.clone().unwrap_or_else(|| {
                    crate::i18n::active_t("repl.toolchain_failed").replacen(
                        "{}",
                        command.as_str(),
                        1,
                    )
                }),
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

    let mut staged_spans: Vec<StagedFileSpan> = Vec::new();
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
                        crate::i18n::active_t("repl.edits_badge_committed").replacen(
                            "{}",
                            hash.as_str(),
                            1,
                        ),
                        indicator_style(&engine, palette.status_success),
                    )
                } else {
                    Span::styled(
                        crate::i18n::active_t("repl.edits_badge_applied"),
                        indicator_style(&engine, palette.status_success),
                    )
                }
            }
            Some(false) => Span::styled(
                crate::i18n::active_t("repl.edits_badge_rejected"),
                indicator_style(&engine, palette.status_error),
            ),
            None => {
                let spin = SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()];
                Span::styled(
                    crate::i18n::active_t("repl.edits_badge_modifying").replacen("{}", spin, 1),
                    indicator_style(&engine, palette.status_warning),
                )
            }
        };

        let header_spans = vec![
            header_badge,
            Span::raw(" "),
            Span::styled(
                format!(
                    "{} {}",
                    crate::i18n::active_t("repl.proposed_edits"),
                    crate::i18n::active_t("repl.edits_files_progress")
                        .replacen("{}", &ok_files.to_string(), 1)
                        .replacen("{}", &total_files.to_string(), 1)
                ),
                engine.fg(palette.status_info).bold(),
            ),
        ];

        model_lines.push(Line::from(header_spans));

        if let Some(summary) = &model.last_commit_summary {
            model_lines.push(Line::from(vec![
                Span::raw(format!(
                    "  {}",
                    crate::i18n::active_t("repl.edits_summary_label")
                )),
                Span::styled(summary.clone(), Style::default().bold()),
            ]));
        }

        for (idx, file) in model.files.iter().enumerate() {
            let header_line = model_lines.len();
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

            let op_badge = file_op_badge(&engine, &file.op_type);

            let hunk_count = file.hunks.len();
            let hunk_label = match file.op_type {
                EditOp::Move => crate::i18n::active_t("repl.hunk_renamed"),
                EditOp::Overwrite => crate::i18n::active_t("repl.hunk_rewritten"),
                EditOp::Delete => crate::i18n::active_t("repl.hunk_deleted"),
                EditOp::Create => {
                    let line_count = file
                        .hunks
                        .first()
                        .map(|h| h.new_text.lines().count())
                        .unwrap_or(0);
                    crate::i18n::active_t("repl.hunk_line_count").replacen(
                        "{}",
                        &line_count.to_string(),
                        1,
                    )
                }
                EditOp::Replace => crate::i18n::active_t("repl.hunk_count").replacen(
                    "{}",
                    &hunk_count.to_string(),
                    1,
                ),
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
                op_badge,
                Span::styled(file.path.clone(), Style::default().bold()),
                Span::raw(" "),
                Span::styled(format!("({})", hunk_label), engine.fg(palette.text_muted)),
                Span::raw(" "),
                Span::styled(status_icon, status_style),
            ];

            if file.status != FileEditStatus::Ok {
                if let Some(retry) = &file.retry_info {
                    file_spans.push(Span::raw(" "));
                    file_spans.push(Span::styled(
                        crate::i18n::active_t("repl.edits_retry_label").replacen(
                            "{}",
                            retry.as_str(),
                            1,
                        ),
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

            let body_start = model_lines.len();
            let hunk_offsets = if file.expanded {
                let rendered = model.diff_cache.render(file);
                model_lines.extend(rendered.lines);
                rendered.hunk_rows
            } else {
                Vec::new()
            };
            staged_spans.push(StagedFileSpan {
                header_line,
                body_start: (model_lines.len() > body_start).then_some(body_start),
                body_end: model_lines.len(),
                hunk_offsets,
            });
        }
    }
    model.staged_file_spans = staged_spans;

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
    use tauqe_protocol::model::{UsageTotals, UsageTotalsSnapshot};

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
        let rendered: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
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
        assert!(lines.iter().any(|l| l
            .spans
            .iter()
            .any(|s| s.content.contains("Failed to connect"))));
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
            ..UsageTotalsSnapshot::default()
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
