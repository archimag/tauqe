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
            " Live Findings Draft:",
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
