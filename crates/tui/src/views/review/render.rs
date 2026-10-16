use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
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
        let empty_lines = if state.model.is_busy() {
            vec![
                Line::raw(""),
                Line::from(Span::styled(
                    "  Review in progress...",
                    Style::default().fg(Color::Yellow).bold(),
                )),
                Line::from(Span::styled(
                    "  Streaming findings and summary in Develop view (Ctrl+1).",
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        } else {
            vec![
                Line::raw(""),
                Line::from(Span::styled(
                    "  No code reviews yet.",
                    Style::default().fg(Color::DarkGray).bold(),
                )),
                Line::from(Span::styled(
                    "  Add files to Context (Ctrl+2), then press 'r' to run a code review.",
                    Style::default().fg(Color::DarkGray),
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
        status_spans.push(Span::styled(format!(" {}", err), Style::default().fg(Color::Red).bold()));
    } else {
        if state.model.is_busy() {
            status_spans.push(Span::styled(
                " Review in progress... ",
                Style::default().fg(Color::Yellow).bold(),
            ));
        }
        if let Some(row) = state.review.selected_row() {
            status_spans.extend(describe_review_row(&row));
        }
        status_spans.push(Span::styled(
            format!(
                "  |  {} session(s), {} finding(s) ({} fixed, {} skipped)",
                total_sessions, total_items, fixed_items, skipped_items
            ),
            Style::default().fg(Color::DarkGray),
        ));
        if state.review.hide_closed {
            status_spans.push(Span::styled(" | Filter: Open only", Style::default().fg(Color::Yellow)));
        }
        if state.review.refining.is_some() {
            status_spans.push(Span::styled(
                " | [Refining: Architect is decomposing the finding...]",
                Style::default().fg(Color::Yellow).bold(),
            ));
        }
        if !rows.is_empty() {
            status_spans.push(Span::styled(
                format!(" | [Row {}/{}]", state.review.selected_index + 1, rows.len()),
                Style::default().fg(Color::Gray),
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

fn describe_review_row(row: &VisibleReviewRow) -> Vec<Span<'static>> {
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
                    Style::default().fg(Color::White).bold(),
                ),
                Span::styled(format!(" ({})", model), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "  {}/{} fixed ({} todo, {} prog, {} disc, {} skip, {} rej)",
                        fixed, total, todo, in_progress, discussion, skipped, rejected
                    ),
                    Style::default().fg(Color::Gray),
                ),
            ];
            if let Some(desc) = first_content_line(description.as_deref()) {
                spans.push(Span::styled(
                    format!("  —  {}", desc),
                    Style::default().fg(Color::DarkGray),
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
                Style::default().fg(Color::Green).bold(),
            )];
            if let Some(desc) = first_content_line(Some(description.as_str())) {
                spans.push(Span::styled(
                    format!("  —  {}", desc),
                    Style::default().fg(Color::DarkGray),
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
                Span::styled(format!(" #{} ", item_id), Style::default().fg(Color::Gray)),
                Span::styled(format!("[{}]", severity), severity_style(*severity)),
                Span::raw(" "),
                Span::styled(
                    format!(" {} ", status_label(*status)),
                    status_style(*status),
                ),
                Span::styled(title.clone(), Style::default().fg(Color::White).bold()),
            ];
            if let Some(selection) = model {
                spans.push(model_badge(selection));
            }
            if let Some(loc) = location(files) {
                spans.push(Span::styled(
                    format!("  {}", loc),
                    Style::default().fg(Color::Cyan),
                ));
            }
            if let Some(desc) = first_content_line(Some(body.as_str())) {
                spans.push(Span::styled(
                    format!("  —  {}", desc),
                    Style::default().fg(Color::DarkGray),
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

        let text: String = describe_review_row(&row)
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
    fn render_review_view_shows_in_progress_indicator_when_model_busy() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = crate::app::test_state();
        state.model.status = crate::views::develop::TurnStatus::Responding;

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
}
