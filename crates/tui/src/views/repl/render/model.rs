use crate::markdown::ExtractedLink;
use crate::theme::engine::{current_engine, ThemeEngine};
use crate::ui::format::format_tokens;
use crate::ui::wrap::wrapped_line_count;
use crate::ui::wrap_line;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};

use super::super::state::{CanvasProjection, EditOp, FileEditStatus, ReplView, ToolchainState};
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
        EditOp::Move | EditOp::MoveDir => ("repl.op.move", palette.status_info),
        EditOp::Overwrite => ("repl.op.overwrite", palette.status_warning),
        EditOp::Replace => ("repl.op.replace", palette.button_active_bg),
    };
    Span::styled(
        format!("{} ", crate::i18n::active_t(key)),
        engine.fg(color).bold(),
    )
}

/// Wraps the canvas into the rows the viewport draws.
///
/// Every raw line yields at least one visual row, so the returned offsets are
/// indexed by the raw line index and name the exact visual span that line
/// occupies. The renderer and every scroll derivation wrap through this single
/// pass, so a row resolved for scrolling is the row the viewport draws (c-1, c-4).
pub(crate) fn wrap_model_lines(
    raw: Vec<Line<'static>>,
    width: usize,
) -> (Vec<Line<'static>>, Vec<usize>, Vec<usize>) {
    let mut wrapped = Vec::with_capacity(raw.len());
    let mut visual_start = Vec::with_capacity(raw.len());
    let mut visual_end = Vec::with_capacity(raw.len());
    let mut current_visual = 0usize;
    for line in raw {
        let line_rows = wrap_line(line, width);
        let row_count = line_rows.len();
        visual_start.push(current_visual);
        visual_end.push(current_visual + row_count.saturating_sub(1));
        current_visual += row_count;
        wrapped.extend(line_rows);
    }
    (wrapped, visual_start, visual_end)
}

/// Measures the wrap projection of the current canvas without materializing it.
///
/// Every raw line is measured through the same break decisions the rendering
/// pass takes, so a scroll target resolved through the measurement names the row
/// the viewport draws. This is the fallback the navigation paths use when no
/// recording describes the live state — for example before the first frame of a
/// turn (c-1, c-4).
pub(crate) fn measure_canvas(model: &mut ReplView) -> CanvasProjection {
    let width = model.content_width.max(10);
    let (raw, file_rows) = compute_model_lines_with_file_rows(model);
    let mut visual_start = Vec::with_capacity(raw.len());
    let mut visual_end = Vec::with_capacity(raw.len());
    let mut total_lines = 0usize;
    for line in &raw {
        let rows = wrapped_line_count(line, width);
        visual_start.push(total_lines);
        visual_end.push(total_lines + rows.saturating_sub(1));
        total_lines += rows;
    }
    CanvasProjection {
        signature: model.canvas_signature(),
        visual_start,
        visual_end,
        file_rows,
        total_lines,
    }
}

/// One clickable row of a Markdown hyperlink, in canvas coordinates.
///
/// The Markdown renderer names a link by the display columns it painted on the
/// unwrapped line, while the viewport draws a wrapped canvas. Resolving the two
/// belongs to the surface that wrapped the canvas, so the pointer dispatcher
/// receives finished rows instead of a line index and a column pair (c-1, c-3).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LinkRow {
    /// Address the row opens.
    pub(crate) url: String,
    /// Visual row of the drawn canvas.
    pub(crate) visual_line: usize,
    /// First painted column of the link on that row.
    pub(crate) start_col: usize,
    /// Column just past the link on that row.
    pub(crate) end_col: usize,
}

/// Resolves the answer hyperlinks onto the rows the canvas draws.
///
/// Every link arrives with the columns of the unwrapped Markdown line, so the row
/// band of that line is read from the wrap projection and the link is clipped row
/// by row: each visual row accounts for the width it paints, and a link that
/// crosses a break yields one row per part it covers. A link of a line that never
/// wrapped therefore keeps its exact columns, and a link whose line is not part of
/// the canvas — a parse that predates the current turn — resolves to nothing
/// instead of a mispositioned target (c-1, c-3).
pub(crate) fn link_rows(
    links: &[ExtractedLink],
    prefix_count: usize,
    rows: &[Line<'static>],
    visual_start: &[usize],
    visual_end: &[usize],
    width: usize,
) -> Vec<LinkRow> {
    let mut resolved = Vec::new();
    if width == 0 {
        return resolved;
    }
    for link in links {
        let raw = prefix_count + link.line_idx;
        let (Some(&first), Some(&last)) = (visual_start.get(raw), visual_end.get(raw)) else {
            continue;
        };
        // Columns of the source line the rows walked so far have consumed.
        let mut source_col = 0usize;
        for visual_line in first..=last {
            let painted = rows.get(visual_line).map_or(0, |row| row.width());
            let row_end = source_col.saturating_add(painted);
            let start = link.start_col.clamp(source_col, row_end) - source_col;
            let end = link.end_col.clamp(source_col, row_end) - source_col;
            let start = start.min(width);
            let end = end.min(width);
            if end > start {
                resolved.push(LinkRow {
                    url: link.url.clone(),
                    visual_line,
                    start_col: start,
                    end_col: end,
                });
            }
            source_col = row_end;
        }
    }
    resolved
}

pub fn compute_model_lines(model: &mut ReplView) -> Vec<Line<'static>> {
    compute_model_lines_with_file_rows(model).0
}

/// Builds the canvas and records the raw index of every staged file summary row.
///
/// The compact file list renders exactly one summary row per staged file, and
/// that index is recorded while the canvas is built instead of being re-derived
/// by a second arithmetic. Selecting a file, keeping it visible, and recentering
/// on it therefore resolve through the layout that is actually drawn (c-1, c-4).
/// Callers that need no indices use [`compute_model_lines`].
pub fn compute_model_lines_with_file_rows(
    model: &mut ReplView,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let engine = current_engine();
    let palette = engine.palette();

    let mut model_lines: Vec<Line<'static>> = Vec::new();
    let mut file_rows: Vec<usize> = Vec::new();
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

    // The proposed edits are a compact summary: every file renders exactly one
    // row with its operation, path, patch count, and verification status. The
    // patch bodies belong to the two-pane diff inspector, so a long diff never
    // breaks the conversational flow of the canvas (c-4).
    if model.edits_active || !model.files.is_empty() || model.edit_final_applied.is_some() {
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

        if model.edit_final_applied == Some(false) {
            if let Some(err) = &model.edit_final_error {
                model_lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{} ", crate::i18n::active_t("repl.validation_error")),
                        engine.fg(palette.status_error).bold(),
                    ),
                    Span::styled(err.clone(), engine.fg(palette.status_warning)),
                ]));
            }
        }

        for (idx, file) in model.files.iter().enumerate() {
            let is_selected = idx == model.selected_file_index;
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
                EditOp::Move | EditOp::MoveDir => crate::i18n::active_t("repl.hunk_renamed"),
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

            file_rows.push(model_lines.len());
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

    (model_lines, file_rows)
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
    fn test_compute_model_lines_yields_on_rejected_error() {
        let mut model = ReplView {
            edit_final_applied: Some(false),
            edit_final_error: Some("Target file not in context".to_string()),
            ..ReplView::default()
        };
        assert!(!model.splash_visible());
        let lines = compute_model_lines(&mut model);
        assert!(!lines.is_empty());
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(rendered.contains("Target file not in context"));
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

#[cfg(test)]
mod link_row_tests {
    use super::*;

    /// A link as the Markdown renderer reports it: address plus the display
    /// columns of the unwrapped line it painted.
    fn link(url: &str, line_idx: usize, start_col: usize, end_col: usize) -> ExtractedLink {
        ExtractedLink {
            id: 0,
            url: url.to_string(),
            line_idx,
            start_col,
            end_col,
        }
    }

    fn row(text: &str) -> Line<'static> {
        Line::raw(text.to_string())
    }

    #[test]
    fn a_link_of_an_unwrapped_line_keeps_its_painted_columns() {
        let rows = vec![row("See docs for details")];
        let links = vec![link("https://example.com", 0, 4, 8)];

        assert_eq!(
            link_rows(&links, 0, &rows, &[0], &[0], 40),
            vec![LinkRow {
                url: "https://example.com".to_string(),
                visual_line: 0,
                start_col: 4,
                end_col: 8,
            }]
        );
    }

    #[test]
    fn the_prefix_rows_of_the_canvas_shift_the_resolved_line() {
        // An error row and a toolchain row precede the answer, so the first
        // Markdown line is drawn on the third visual row.
        let rows = vec![row("error"), row("cargo check"), row("See docs")];
        let links = vec![link("https://example.com", 0, 4, 8)];

        let resolved = link_rows(&links, 2, &rows, &[0, 1, 2], &[0, 1, 2], 40);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].visual_line, 2);
        assert_eq!((resolved[0].start_col, resolved[0].end_col), (4, 8));
    }

    #[test]
    fn a_link_that_crosses_a_wrap_break_yields_one_row_per_part() {
        // The line wrapped into two ten-column rows; the link starts inside the
        // first one and runs past the break.
        let rows = vec![row("0123456789"), row("0123456789")];
        let links = vec![link("https://example.com", 0, 8, 14)];

        assert_eq!(
            link_rows(&links, 0, &rows, &[0], &[1], 10),
            vec![
                LinkRow {
                    url: "https://example.com".to_string(),
                    visual_line: 0,
                    start_col: 8,
                    end_col: 10,
                },
                LinkRow {
                    url: "https://example.com".to_string(),
                    visual_line: 1,
                    start_col: 0,
                    end_col: 4,
                },
            ]
        );
    }

    #[test]
    fn a_link_outside_the_canvas_resolves_to_nothing() {
        let rows = vec![row("See docs")];
        let links = vec![link("https://example.com", 7, 0, 3)];

        assert!(link_rows(&links, 0, &rows, &[0], &[0], 40).is_empty());
        assert!(link_rows(&links, 0, &rows, &[0], &[0], 0).is_empty());
    }
}

#[cfg(test)]
mod closure_reason_tests {
    use super::super::super::state::TurnStatus;
    use super::*;

    /// Flattens one rendered row into its text.
    fn row_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Index of the first row containing `needle`.
    fn row_of(lines: &[Line<'static>], needle: &str) -> Option<usize> {
        lines
            .iter()
            .position(|line| row_text(line).contains(needle))
    }

    fn closed_view(status: TurnStatus, reason: Option<&str>, applied: bool) -> ReplView {
        ReplView {
            status,
            edit_final_applied: Some(applied),
            edit_final_error: reason.map(str::to_string),
            ..ReplView::default()
        }
    }

    /// The reason of every kind of rejected closure is part of the response
    /// body, under the state badge, whatever the turn status is when the
    /// closure is drawn (c-6).
    #[test]
    fn every_closure_reason_is_visible_in_the_response_body() {
        let reasons = [
            "the model proposed no changes",
            "the turn was cancelled before the edits were applied",
            "the attempt was withdrawn and replaced by a retry",
            "the provider failed before the edits were applied",
            "the turn ended blocked by an unmet precondition",
            "the model response violated the protocol",
            "the turn was aborted by the host environment",
        ];
        let statuses = [
            TurnStatus::Responding,
            TurnStatus::Done,
            TurnStatus::Cancelled,
            TurnStatus::Error,
        ];

        for reason in reasons {
            for status in statuses {
                let mut model = closed_view(status, Some(reason), false);
                let lines = compute_model_lines(&mut model);

                let header = row_of(&lines, &crate::i18n::active_t("repl.proposed_edits"))
                    .expect("the proposed edits block is drawn");
                let badge = row_of(&lines, &crate::i18n::active_t("repl.edits_badge_rejected"))
                    .expect("a rejected closure shows its state badge");
                let reason_row = row_of(&lines, reason).unwrap_or_else(|| {
                    panic!("the reason {reason:?} must be rendered for status {status:?}")
                });

                assert_eq!(badge, header, "the badge sits on the block header");
                assert!(reason_row > header, "the reason renders below the badge");
                assert!(
                    row_text(&lines[reason_row])
                        .contains(&crate::i18n::active_t("repl.validation_error")),
                    "the reason row carries the catalog label"
                );
            }
        }
    }

    /// A successful closure carries no error, so no reason row is drawn.
    #[test]
    fn a_successful_closure_renders_no_reason_row() {
        let mut model = closed_view(TurnStatus::Done, None, true);
        let lines = compute_model_lines(&mut model);

        assert!(row_of(&lines, &crate::i18n::active_t("repl.proposed_edits")).is_some());
        assert!(
            row_of(&lines, &crate::i18n::active_t("repl.validation_error")).is_none(),
            "an applied closure has nothing to explain"
        );
    }
}
