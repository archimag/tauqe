use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::app::AppState;
use crate::i18n::active_t;

use super::{architect_badge, compute_unified_plan_lines, status_style, VisiblePlanRow};

pub fn render_plans_view(
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
    state.plans_view.view_height = height;
    state.plans_view.content_width = width;
    // Mirror the authoritative discussion focus so the row builder can mark the
    // anchored element without re-reading global application state.
    let focus = state.discussion_focus.clone();
    state.plans_view.discussion_focus = focus;

    let rows = state.plans_view.flatten_rows();
    state.plans_view.clamp_selection();

    let (lines, offsets) = if rows.is_empty() {
        (
            vec![
                Line::raw(""),
                Line::from(Span::styled(
                    format!("  {}", active_t("plans.empty_title")),
                    engine.fg(palette.text_muted).bold(),
                )),
                Line::from(Span::styled(
                    format!("  {}", active_t("plans.empty_hint")),
                    engine.fg(palette.text_muted),
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
        status_spans.push(Span::styled(msg.clone(), engine.fg(palette.text_secondary)));
    } else {
        if let Some(row) = state.plans_view.selected_row() {
            status_spans.extend(describe_plan_row(&row, &engine));
        }
        let plans_count = state.plans_view.plans.len();
        let total_items: usize = state
            .plans_view
            .plans
            .iter()
            .map(|p| p.count_stats().0)
            .sum();
        let total_completed: usize = state
            .plans_view
            .plans
            .iter()
            .map(|p| p.count_stats().1)
            .sum();
        status_spans.push(Span::styled(
            format!(
                "  |  {} plan(s), {} item(s) ({} done)",
                plans_count, total_items, total_completed
            ),
            engine.fg(palette.text_muted),
        ));
        if !rows.is_empty() {
            status_spans.push(Span::styled(
                format!(
                    " | [Row {}/{}]",
                    state.plans_view.selected_index + 1,
                    rows.len()
                ),
                engine.fg(palette.text_muted),
            ));
        }
        if state.plans_view.refining {
            status_spans.push(Span::styled(
                format!(" | {}", active_t("plans.refining")),
                engine.fg(palette.status_warning).bold(),
            ));
        }
    }
    let status_line = Line::from(status_spans);

    let info_widget =
        Paragraph::new(vec![status_line]).block(Block::default().padding(Padding::horizontal(1)));
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

fn describe_plan_row(
    row: &VisiblePlanRow,
    engine: &crate::theme::engine::ThemeEngine,
) -> Vec<Span<'static>> {
    let palette = engine.palette();
    match row {
        VisiblePlanRow::PlanHeader {
            title,
            description,
            status,
            total,
            completed,
            architect,
            ..
        } => {
            let mut spans = vec![
                Span::styled(
                    format!(" {} {}", active_t("plans.plan_label"), title),
                    engine.fg(palette.text_primary).bold(),
                ),
                Span::styled(format!(" [{}]", status), status_style(*status)),
                Span::styled(
                    format!("  {}/{} done", completed, total),
                    engine.fg(palette.text_secondary),
                ),
            ];
            if let Some(desc) = first_content_line(description.as_deref()) {
                spans.push(Span::styled(
                    format!("  —  {}", desc),
                    engine.fg(palette.text_muted),
                ));
            }
            if let Some(tier) = architect {
                spans.push(architect_badge(tier));
            }
            spans
        }
        VisiblePlanRow::ConceptHeader { count, .. } => vec![Span::styled(
            format!(
                " {}",
                active_t("plans.concept_summary").replacen("{}", &count.to_string(), 1)
            ),
            engine.fg(palette.button_active_bg),
        )],
        VisiblePlanRow::ConceptItem {
            item_id,
            title,
            details,
            ..
        } => {
            let mut spans = vec![Span::styled(
                format!(" #{} {}", item_id, title),
                engine.fg(palette.button_active_bg).bold(),
            )];
            if let Some(desc) = first_content_line(details.as_deref()) {
                spans.push(Span::styled(
                    format!("  —  {}", desc),
                    engine.fg(palette.text_muted),
                ));
            }
            spans
        }
        VisiblePlanRow::ImplementationHeader {
            total, completed, ..
        } => vec![Span::styled(
            format!(
                " {}",
                active_t("plans.execution_blueprint")
                    .replacen("{}", &completed.to_string(), 1)
                    .replacen("{}", &total.to_string(), 1)
            ),
            engine.fg(palette.status_info),
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
                    engine.fg(palette.text_primary).bold(),
                ),
                Span::styled(format!(" [{}]", status), status_style(*status)),
            ];
            if let Some(desc) = first_content_line(details.as_deref()) {
                spans.push(Span::styled(
                    format!("  —  {}", desc),
                    engine.fg(palette.text_muted),
                ));
            }
            if !files.is_empty() {
                spans.push(Span::styled(
                    format!("  {} {}", active_t("plans.files_prefix"), files.join(", ")),
                    engine.fg(palette.status_info),
                ));
            }
            if !readonly_files.is_empty() {
                spans.push(Span::styled(
                    format!(
                        "  {} {}",
                        active_t("plans.ref_prefix"),
                        readonly_files.join(", ")
                    ),
                    engine.fg(palette.text_secondary),
                ));
            }
            spans
        }
    }
}
