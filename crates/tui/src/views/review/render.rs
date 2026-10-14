use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};
use tauqe_protocol::ReviewStatus;

use crate::app::AppState;
use crate::ui::wrap_lines;

use super::{
    compute_unified_review_lines, location, model_badge, severity_style, status_label,
    status_style, ReviewViewState, VisibleReviewRow,
};

fn streaming_lines(review: &ReviewViewState, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        " Reviewing active context...",
        Style::default().fg(Color::Yellow).bold(),
    ))];
    if !review.reasoning.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(review.reasoning.render_lines());
        lines.push(Line::raw(""));
    }
    if !review.content.is_empty() {
        lines.push(Line::from(Span::styled(
            " Executive Summary:",
            Style::default().fg(Color::Green).bold(),
        )));
        let md = crate::markdown::render_markdown(
            &review.content,
            &crate::markdown::MarkdownTheme::answer_themed(review.theme_mode),
        );
        lines.extend(md);
    }
    wrap_lines(lines, width)
}

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

    if state.review.running {
        let lines = streaming_lines(&state.review, width);
        state.review.rendered_lines = lines.len();
        let max_scroll = (lines.len() as u16).saturating_sub(height);
        if state.review.scroll > max_scroll {
            state.review.scroll = max_scroll;
        }
        let paragraph = Paragraph::new(lines).block(block).scroll((state.review.scroll, 0));
        frame.render_widget(paragraph, main_area);

        let status_line = Line::from(vec![
            Span::styled(" Reviewing... ", Style::default().fg(Color::Yellow).bold()),
        ]);
        let info = Paragraph::new(vec![status_line]).block(Block::default().padding(Padding::horizontal(1)));
        frame.render_widget(info, info_area);
        return;
    }

    let rows = state.review.flatten_rows();
    state.review.clamp_selection();

    let (lines, offsets) = if rows.is_empty() {
        (
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
            ],
            Vec::new(),
        )
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
        .map(|s| s.items.iter().filter(|i| i.status == ReviewStatus::Fixed).count())
        .sum();

    let mut status_spans = Vec::new();
    if let Some(err) = &state.review.error {
        status_spans.push(Span::styled(format!(" {}", err), Style::default().fg(Color::Red).bold()));
    } else {
        if let Some(row) = state.review.selected_row() {
            status_spans.extend(describe_review_row(&row));
        }
        status_spans.push(Span::styled(
            format!(
                "  |  {} session(s), {} finding(s) ({} fixed)",
                total_sessions, total_items, fixed_items
            ),
            Style::default().fg(Color::DarkGray),
        ));
        if state.review.hide_closed {
            status_spans.push(Span::styled(" | Filter: Open only", Style::default().fg(Color::Yellow)));
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
                        "  {}/{} fixed ({} todo, {} prog, {} disc, {} rej)",
                        fixed, total, todo, in_progress, discussion, rejected
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
        VisibleReviewRow::ReviewItem {
            item_id,
            title,
            body,
            severity,
            status,
            model,
            file_path,
            line_range,
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
            if let Some(loc) = location(file_path.as_deref(), *line_range) {
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
    use ratatui::text::Line;

    fn line_texts(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn streaming_lines_show_review_summary_header_with_markdown() {
        let review = ReviewViewState {
            content: "## Overview\n\nThe architecture is sound.\n\n- Point one\n- Point two"
                .to_string(),
            ..Default::default()
        };

        let lines = streaming_lines(&review, 80);
        let texts = line_texts(&lines);

        assert!(
            texts.iter().any(|t| t.contains("Executive Summary:")),
            "expected the Executive Summary header, got: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("Live Findings Draft:")),
            "the legacy JSON draft header must be gone"
        );
        assert!(
            texts.iter().any(|t| t.contains("The architecture is sound.")),
            "expected the rendered Markdown summary body, got: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("Point one")),
            "expected Markdown list items to render, got: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains('`') || t.contains('{')),
            "raw JSON syntax must never leak into the streamed summary"
        );
    }

    #[test]
    fn streaming_lines_omit_summary_header_without_content() {
        let review = ReviewViewState::default();

        let lines = streaming_lines(&review, 80);
        let texts = line_texts(&lines);

        assert!(
            !texts.iter().any(|t| t.contains("Executive Summary:")),
            "the summary header must not appear without content"
        );
    }

    #[test]
    fn describe_review_row_surfaces_session_description() {
        let row = VisibleReviewRow::SessionHeader {
            session_id: "rev-1".to_string(),
            title: "Architecture audit".to_string(),
            description: Some("High level summary line.\nMore detail.".to_string()),
            created_at: 0,
            model: "reviewer-model".to_string(),
            total: 2,
            fixed: 0,
            todo: 2,
            in_progress: 0,
            discussion: 0,
            rejected: 0,
            is_expanded: true,
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
    fn executive_summary_remains_visible_after_stream_completes() {
        use tauqe_protocol::{ReviewItem, ReviewSeverity, ReviewSession, ReviewStatus};

        let mut review = ReviewViewState {
            running: true,
            ..Default::default()
        };
        review.content.push_str("Streaming the summary...");

        // The server finalizes the review session; the TUI stores it and leaves
        // the streaming mode, so the persistent tree view takes over.
        let session = ReviewSession {
            id: "rev-1".to_string(),
            title: "Audit".to_string(),
            created_at: 0,
            model: "reviewer".to_string(),
            description: Some("## Executive overview\n\nThe architecture is sound.".to_string()),
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![ReviewItem {
                id: 1,
                title: "Finding".to_string(),
                model: None,
                severity: ReviewSeverity::Warning,
                status: ReviewStatus::Todo,
                file_path: None,
                line_range: None,
                body: String::new(),
            }],
            raw_markdown: String::new(),
        };
        review.set_session(session);

        assert!(!review.running, "streaming must stop once the session is set");

        let rows = review.flatten_rows();
        let (lines, _offsets) =
            compute_unified_review_lines(&rows, review.selected_index, 80, review.theme_mode);
        let texts: Vec<String> = lines.iter().map(|line| line.to_string()).collect();

        assert!(
            texts.iter().any(|t| t.contains("Executive Summary:")),
            "the Executive Summary block must remain visible after streaming, got: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("The architecture is sound.")),
            "the summary body must render in the session header, got: {texts:?}"
        );
    }
}
