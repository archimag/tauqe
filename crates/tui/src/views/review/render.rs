use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};
use tauqe_protocol::ReviewItemStatus;

use crate::app::AppState;

use super::{
    compute_unified_review_lines, location, model_badge, severity_style, status_label,
    status_style, VisibleReviewRow,
};

pub fn render_review_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    main_area: Rect,
    info_area: Rect,
) {
    let engine = crate::theme::engine::current_engine();
    let palette = engine.palette();
    let block = Block::default().padding(Padding::horizontal(1));
    let inner = block.inner(main_area);
    let height = inner.height;
    let width = (inner.width as usize).max(10);
    state.review.view_height = height;
    state.review.content_width = width;
    // Mirror the authoritative discussion focus so the row builder can mark the
    // anchored element without re-reading global application state.
    let focus = state.discussion_focus.clone();
    state.review.discussion_focus = focus;

    let rows = state.review.flatten_rows();
    state.review.clamp_selection();

    let (lines, offsets) = if rows.is_empty() {
        let empty_lines = if state.review.running {
            vec![
                Line::raw(""),
                Line::from(Span::styled(
                    "  Review in progress...",
                    engine.fg(palette.status_warning).bold(),
                )),
                Line::from(Span::styled(
                    "  Streaming findings and summary in Develop view (Ctrl+1).",
                    engine.fg(palette.text_muted),
                )),
            ]
        } else {
            vec![
                Line::raw(""),
                Line::from(Span::styled(
                    "  No code reviews yet.",
                    engine.fg(palette.text_muted).bold(),
                )),
                Line::from(Span::styled(
                    "  Add files to Context (Ctrl+2), then press 'r' to run a code review.",
                    engine.fg(palette.text_muted),
                )),
            ]
        };
        (empty_lines, Vec::new())
    } else {
        compute_unified_review_lines(
            &rows,
            state.review.selected_index,
            width,
            state.review.theme_mode,
        )
    };

    state.review.rendered_lines = lines.len();

    if !rows.is_empty() && height > 0 {
        let sel = state.review.selected_index.min(rows.len() - 1);
        let start = offsets.get(sel).copied().unwrap_or(0);
        let end = offsets.get(sel + 1).copied().unwrap_or(lines.len());
        let scroll = state.review.scroll as usize;

        let new_scroll = if sel == 0 {
            0
        } else if start < scroll {
            start
        } else if end > scroll + height as usize {
            end.saturating_sub(height as usize).min(start)
        } else {
            scroll
        };
        state.review.scroll = new_scroll as u16;
    }

    let max_scroll = (lines.len() as u16).saturating_sub(height);
    if state.review.scroll > max_scroll {
        state.review.scroll = max_scroll;
    }

    let tree_widget = Paragraph::new(lines).block(block).scroll((state.review.scroll, 0));
    frame.render_widget(tree_widget, main_area);

    let effective = state.review.effective_sessions();
    let total_sessions = effective.len();
    let total_items: usize = effective.iter().map(|s| s.items.len()).sum();
    let fixed_items: usize = effective
        .iter()
        .map(|s| s.items.iter().filter(|i| i.status == ReviewItemStatus::Fixed).count())
        .sum();
    let skipped_items: usize = effective
        .iter()
        .map(|s| s.items.iter().filter(|i| i.status == ReviewItemStatus::Skipped).count())
        .sum();

    let mut status_spans = Vec::new();
    if let Some(err) = &state.review.error {
        status_spans.push(Span::styled(format!(" {}", err), engine.fg(palette.status_error).bold()));
    } else {
        if state.review.running {
            status_spans.push(Span::styled(
                " Review in progress... ",
                engine.fg(palette.status_warning).bold(),
            ));
        }
        if let Some(row) = state.review.selected_row() {
            status_spans.extend(describe_review_row(&row, &engine));
        }
        status_spans.push(Span::styled(
            format!(
                "  |  {} session(s), {} finding(s) ({} fixed, {} skipped)",
                total_sessions, total_items, fixed_items, skipped_items
            ),
            engine.fg(palette.text_muted),
        ));
        if state.review.hide_closed {
            status_spans.push(Span::styled(" | Filter: Open only", engine.fg(palette.status_warning)));
        }
        if state.review.refining.is_some() {
            status_spans.push(Span::styled(
                " | [Refining: Architect is decomposing the finding...]",
                engine.fg(palette.status_warning).bold(),
            ));
        }
        if !rows.is_empty() {
            status_spans.push(Span::styled(
                format!(" | [Row {}/{}]", state.review.selected_index + 1, rows.len()),
                engine.fg(palette.text_secondary),
            ));
        }
    }

    let info_widget = Paragraph::new(vec![Line::from(status_spans)])
        .block(Block::default().padding(Padding::horizontal(1)));
    frame.render_widget(info_widget, info_area);
}

fn first_content_line(raw: Option<&str>) -> Option<String> {
    raw.and_then(|text| {
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string)
    })
}

fn describe_review_row(
    row: &VisibleReviewRow,
    engine: &crate::theme::engine::ThemeEngine,
) -> Vec<Span<'static>> {
    let palette = engine.palette();
    match row {
        VisibleReviewRow::SessionHeader {
            title,
            description,
            model,
            total,
            fixed,
            todo,
            in_progress,
            discussion,
            skipped,
            rejected,
            ..
        } => {
            let mut spans = vec![
                Span::styled(
                    format!(" Review: {}", title),
                    engine.fg(palette.text_primary).bold(),
                ),
                Span::styled(
                    format!(" ({})", model),
                    engine.fg(palette.text_muted),
                ),
                Span::styled(
                    format!(
                        "  {}/{} fixed ({} todo, {} prog, {} disc, {} skip, {} rej)",
                        fixed, total, todo, in_progress, discussion, skipped, rejected
                    ),
                    engine.fg(palette.text_secondary),
                ),
            ];
            if let Some(desc) = first_content_line(description.as_deref()) {
                spans.push(Span::styled(
                    format!("  —  {}", desc),
                    engine.fg(palette.text_muted),
                ));
            }
            spans
        }
        VisibleReviewRow::SessionSummary {
            description,
            is_expanded,
            ..
        } => {
            let mut spans = vec![Span::styled(
                format!(
                    " Executive Summary{}",
                    if *is_expanded { "" } else { " (folded)" }
                ),
                engine.fg(palette.status_success).bold(),
            )];
            if let Some(desc) = first_content_line(Some(description.as_str())) {
                spans.push(Span::styled(
                    format!("  —  {}", desc),
                    engine.fg(palette.text_muted),
                ));
            }
            spans
        }
        VisibleReviewRow::ReviewItem {
            item_id,
            title,
            body,
            severity,
            status,
            model,
            files,
            ..
        } => {
            let mut spans = vec![
                Span::styled(
                    format!(" #{} ", item_id),
                    engine.fg(palette.text_secondary),
                ),
                Span::styled(format!("[{}]", severity), severity_style(*severity)),
                Span::raw(" "),
                Span::styled(
                    format!(" {} ", status_label(*status)),
                    status_style(*status),
                ),
                Span::styled(title.clone(), engine.fg(palette.text_primary).bold()),
            ];
            if let Some(selection) = model {
                spans.push(model_badge(selection));
            }
            if let Some(loc) = location(files) {
                spans.push(Span::styled(
                    format!("  {}", loc),
                    engine.fg(palette.status_info),
                ));
            }
            if let Some(desc) = first_content_line(Some(body.as_str())) {
                spans.push(Span::styled(
                    format!("  —  {}", desc),
                    engine.fg(palette.text_muted),
                ));
            }
            spans
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::review::ReviewViewState;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn describe_review_row_surfaces_session_description() {
        let row = VisibleReviewRow::SessionHeader {
            session_id: "rev-1".to_string(),
            title: "Architecture audit".to_string(),
            description: Some("High level summary line.\nMore detail.".to_string()),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("reviewer-model"),
            total: 2,
            fixed: 0,
            todo: 2,
            in_progress: 0,
            discussion: 0,
            skipped: 0,
            rejected: 0,
            is_expanded: true,
            focused: false,
        };

        let engine = crate::theme::engine::current_engine();
        let text: String = describe_review_row(&row, &engine)
            .iter()
            .map(|span| span.content.to_string())
            .collect();

        assert!(
            text.contains("High level summary line."),
            "expected the first description line in the info area, got: {text}"
        );
    }

    #[test]
    fn executive_summary_remains_visible_after_session_set() {
        use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSeverity, ReviewSession};

        let mut review = ReviewViewState::default();

        let session = ReviewSession {
            id: "rev-1".to_string(),
            title: "Audit".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("reviewer"),
            description: Some("## Executive overview\n\nThe architecture is sound.".to_string()),
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![ReviewItem {
                id: 1,
                title: "Finding".to_string(),
                model: None,
                severity: ReviewSeverity::Warning,
                status: ReviewItemStatus::Todo,
                files: Vec::new(),
                subplan: Vec::new(),
                resolution_notes: None,
                body: String::new(),
            }],
            raw_markdown: String::new(),
        };
        review.set_session(session);

        let rows = review.flatten_rows();
        let (lines, _offsets) =
            compute_unified_review_lines(&rows, review.selected_index, 80, review.theme_mode);
        let texts: Vec<String> = lines.iter().map(|line| line.to_string()).collect();

        assert!(
            texts.iter().any(|t| t.contains("Executive Summary:")),
            "the Executive Summary block must remain visible after setting session, got: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("The architecture is sound.")),
            "the summary body must render in the session header, got: {texts:?}"
        );
    }

    #[test]
    fn render_review_view_shows_in_progress_indicator_when_review_running() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = crate::app::test_state();
        state.review.running = true;

        terminal
            .draw(|f| {
                let area = f.area();
                let main_area = Rect::new(0, 0, area.width, area.height.saturating_sub(1));
                let info_area = Rect::new(0, area.height.saturating_sub(1), area.width, 1);
                render_review_view(f, &mut state, main_area, info_area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(
            content.contains("Review in progress..."),
            "expected 'Review in progress...' indicator in buffer"
        );
    }

    #[test]
    fn render_review_view_does_not_show_in_progress_when_unrelated_model_busy() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = crate::app::test_state();
        state.model.status = crate::views::develop::TurnStatus::Responding;
        state.review.running = false;

        terminal
            .draw(|f| {
                let area = f.area();
                let main_area = Rect::new(0, 0, area.width, area.height.saturating_sub(1));
                let info_area = Rect::new(0, area.height.saturating_sub(1), area.width, 1);
                render_review_view(f, &mut state, main_area, info_area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(
            !content.contains("Review in progress..."),
            "did not expect 'Review in progress...' when review is not running"
        );
    }
}
