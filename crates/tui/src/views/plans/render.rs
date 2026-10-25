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

    let (lines, _offsets) = if rows.is_empty() {
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
            &mut state.plans_view.markdown_cache,
        )
    };

    state.plans_view.rendered_lines = lines.len();

    // The renderer only bounds the viewport by the document length. Free
    // scrolling (wheel, paging) belongs to the developer, so re-anchoring the
    // window on the cursor every frame would snap it back (c-render-scroll-decoupling).
    // Positioning the viewport is `scroll_to_selected`'s job and runs only when
    // the selection moves.
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
        // The tree was already flattened for this frame, so the status line
        // reads the selected row out of that list instead of flattening and
        // cloning the whole tree a second time (c-2).
        if let Some(row) = rows.get(state.plans_view.selected_index) {
            status_spans.extend(describe_plan_row(row, &engine));
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
                "  |  {}",
                active_t("plans.stats_summary")
                    .replacen("{}", &plans_count.to_string(), 1)
                    .replacen("{}", &total_items.to_string(), 1)
                    .replacen("{}", &total_completed.to_string(), 1)
            ),
            engine.fg(palette.text_muted),
        ));
        if !rows.is_empty() {
            status_spans.push(Span::styled(
                format!(
                    " | {}",
                    active_t("ui.row_counter")
                        .replacen("{}", &(state.plans_view.selected_index + 1).to_string(), 1)
                        .replacen("{}", &rows.len().to_string(), 1)
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
                    format!(
                        "  {}",
                        active_t("plans.done_progress")
                            .replacen("{}", &completed.to_string(), 1)
                            .replacen("{}", &total.to_string(), 1)
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tauqe_protocol::{Plan, PlanItem, PlanItemStatus};

    fn render_once(terminal: &mut Terminal<TestBackend>, state: &mut crate::app::AppState) {
        terminal
            .draw(|f| {
                let area = f.area();
                let main_area = Rect::new(0, 0, area.width, area.height.saturating_sub(1));
                let info_area = Rect::new(0, area.height.saturating_sub(1), area.width, 1);
                render_plans_view(f, state, main_area, info_area);
            })
            .unwrap();
    }

    /// A plan whose only step carries a body far taller than the viewport, so
    /// the tree can be scrolled far beyond the first screenful.
    fn tall_plan_state() -> crate::app::AppState {
        let mut state = crate::app::test_state();
        state.plans_view.plans = vec![Plan {
            id: "tall-plan".to_string(),
            title: "Tall Plan".to_string(),
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Tall step".to_string(),
                details: Some(
                    (0..120)
                        .map(|line| format!("Invariant line {line}."))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                status: PlanItemStatus::Todo,
                ..Default::default()
            }],
            ..Default::default()
        }];
        state.plans_view.toggle_plan_expanded("tall-plan");
        state.plans_view.toggle_item_expanded("tall-plan", "1");
        state
    }

    /// A redraw must never pull a freely scrolled viewport back to the cursor
    /// (c-render-scroll-decoupling): the renderer bounds the scroll by the
    /// document length only.
    #[test]
    fn render_plans_view_preserves_free_scroll_across_frames() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut state = tall_plan_state();
        // The cursor rests on the plan header while the developer reads deep
        // inside the expanded step.
        state.plans_view.selected_index = 0;
        state.plans_view.scroll = 40;

        render_once(&mut terminal, &mut state);

        assert!(
            state.plans_view.rendered_lines > 40 + state.plans_view.view_height as usize,
            "the expanded step must extend past the free scroll offset"
        );
        assert_eq!(
            state.plans_view.scroll, 40,
            "a redraw must not snap the viewport back to the selected row"
        );

        let max = u16::try_from(state.plans_view.rendered_lines)
            .unwrap_or(u16::MAX)
            .saturating_sub(state.plans_view.view_height);
        state.plans_view.scroll = u16::MAX;
        render_once(&mut terminal, &mut state);
        assert_eq!(
            state.plans_view.scroll, max,
            "the renderer clamps the viewport by the document length only"
        );
    }
}
