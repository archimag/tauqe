use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::app::AppState;

use super::{compute_unified_plan_lines, status_style, VisiblePlanRow};

pub fn render_plans_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    main_area: Rect,
    info_area: Rect,
) {
    let block = Block::default().padding(Padding::horizontal(1));
    let inner = block.inner(main_area);
    let height = inner.height;
    let width = (inner.width as usize).max(10);
    state.plans_view.view_height = height;
    state.plans_view.content_width = width;

    let rows = state.plans_view.flatten_rows();
    state.plans_view.clamp_selection();

    let (lines, offsets) = if rows.is_empty() {
        (
            vec![
                Line::raw(""),
                Line::from(Span::styled(
                    "  No local engineering plans yet.",
                    Style::default().fg(Color::DarkGray).bold(),
                )),
                Line::from(Span::styled(
                    "  Ask Tauqe AI in Develop (Ctrl+1) to formulate a multi-step task plan, or press 'r' to refresh.",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            Vec::new(),
        )
    } else {
        compute_unified_plan_lines(
            &rows,
            state.plans_view.selected_index,
            width,
            state.plans_view.theme_mode,
        )
    };

    state.plans_view.rendered_lines = lines.len();

    if !rows.is_empty() && height > 0 {
        let sel = state.plans_view.selected_index.min(rows.len() - 1);
        let start = offsets.get(sel).copied().unwrap_or(0);
        let end = offsets.get(sel + 1).copied().unwrap_or(lines.len());
        let scroll = state.plans_view.scroll as usize;

        let new_scroll = if sel == 0 {
            0
        } else if start < scroll {
            start
        } else if end > scroll + height as usize {
            end.saturating_sub(height as usize).min(start)
        } else {
            scroll
        };
        state.plans_view.scroll = new_scroll as u16;
    }

    let max_scroll = (lines.len() as u16).saturating_sub(height);
    if state.plans_view.scroll > max_scroll {
        state.plans_view.scroll = max_scroll;
    }

    let tree_widget = Paragraph::new(lines)
        .block(block)
        .scroll((state.plans_view.scroll, 0));
    frame.render_widget(tree_widget, main_area);

    let mut status_spans = Vec::new();
    if let Some(msg) = &state.plans_view.status_message {
        status_spans.push(Span::styled(msg.clone(), Style::default().fg(Color::Gray)));
    } else {
        if let Some(row) = state.plans_view.selected_row() {
            status_spans.extend(describe_plan_row(&row));
        }
        let plans_count = state.plans_view.plans.len();
        let total_items: usize = state.plans_view.plans.iter().map(|p| p.count_stats().0).sum();
        let total_completed: usize = state.plans_view.plans.iter().map(|p| p.count_stats().1).sum();
        status_spans.push(Span::styled(
            format!(
                "  |  {} plan(s), {} item(s) ({} done)",
                plans_count, total_items, total_completed
            ),
            Style::default().fg(Color::DarkGray),
        ));
        if !rows.is_empty() {
            status_spans.push(Span::styled(
                format!(" | [Row {}/{}]", state.plans_view.selected_index + 1, rows.len()),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if state.plans_view.refining {
            status_spans.push(Span::styled(
                " | [REFINING: AI Architect is analyzing codebase...]",
                Style::default().fg(Color::Yellow).bold(),
            ));
        }
    }
    let status_line = Line::from(status_spans);

    let info_widget = Paragraph::new(vec![status_line])
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

fn describe_plan_row(row: &VisiblePlanRow) -> Vec<Span<'static>> {
    match row {
        VisiblePlanRow::PlanHeader {
            title,
            description,
            status,
            total,
            completed,
            ..
        } => {
            let mut spans = vec![
                Span::styled(
                    format!(" Plan: {}", title),
                    Style::default().fg(Color::White).bold(),
                ),
                Span::styled(format!(" [{}]", status), status_style(*status)),
                Span::styled(
                    format!("  {}/{} done", completed, total),
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
        VisiblePlanRow::ConceptHeader { count, .. } => vec![Span::styled(
            format!(" Concept: {} item(s) of architectural intent", count),
            Style::default().fg(Color::Magenta),
        )],
        VisiblePlanRow::ConceptItem {
            item_id,
            title,
            details,
            ..
        } => {
            let mut spans = vec![Span::styled(
                format!(" #{} {}", item_id, title),
                Style::default().fg(Color::LightMagenta).bold(),
            )];
            if let Some(desc) = first_content_line(details.as_deref()) {
                spans.push(Span::styled(
                    format!("  —  {}", desc),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            spans
        }
        VisiblePlanRow::ImplementationHeader {
            total,
            completed,
            ..
        } => vec![Span::styled(
            format!(" Execution blueprint: {}/{} done", completed, total),
            Style::default().fg(Color::Cyan),
        )],
        VisiblePlanRow::PlanItem {
            item_id,
            title,
            details,
            status,
            files,
            readonly_files,
            ..
        } => {
            let mut spans = vec![
                Span::styled(
                    format!(" #{} {}", item_id, title),
                    Style::default().fg(Color::White).bold(),
                ),
                Span::styled(format!(" [{}]", status), status_style(*status)),
            ];
            if let Some(desc) = first_content_line(details.as_deref()) {
                spans.push(Span::styled(
                    format!("  —  {}", desc),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if !files.is_empty() {
                spans.push(Span::styled(
                    format!("  files: {}", files.join(", ")),
                    Style::default().fg(Color::Cyan),
                ));
            }
            if !readonly_files.is_empty() {
                spans.push(Span::styled(
                    format!("  ref: {}", readonly_files.join(", ")),
                    Style::default().fg(Color::LightCyan),
                ));
            }
            spans
        }
    }
}
