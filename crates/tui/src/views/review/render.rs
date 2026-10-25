use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};
use tauqe_protocol::IssueStatus;

use crate::app::AppState;
use crate::i18n::active_t;
use crate::views::plans::architect_badge;

use super::{
    compute_unified_review_lines, format_created_at, issue_status_label, issue_status_style,
    issue_type_badge, issue_type_style, location, model_badge, plan_status_style, VisibleReviewRow,
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

    let (lines, _offsets) = if rows.is_empty() {
        let empty_lines = if state.review.running {
            vec![
                Line::raw(""),
                Line::from(Span::styled(
                    format!("  {}", active_t("review.in_progress")),
                    engine.fg(palette.status_warning).bold(),
                )),
                Line::from(Span::styled(
                    format!("  {}", active_t("review.running_hint")),
                    engine.fg(palette.text_muted),
                )),
            ]
        } else {
            vec![
                Line::raw(""),
                Line::from(Span::styled(
                    format!("  {}", active_t("review.empty_title")),
                    engine.fg(palette.text_muted).bold(),
                )),
                Line::from(Span::styled(
                    format!("  {}", active_t("review.empty_hint")),
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
            &mut state.review.markdown_cache,
        )
    };

    state.review.rendered_lines = lines.len();

    // The renderer only bounds the viewport by the document length. Free
    // scrolling (wheel, paging) belongs to the developer, so re-anchoring the
    // window on the cursor every frame would snap it back (c-1). Positioning
    // the viewport is `scroll_to_selected`'s job and runs only when the
    // selection moves (c-2).
    let max_scroll = (lines.len() as u16).saturating_sub(height);
    if state.review.scroll > max_scroll {
        state.review.scroll = max_scroll;
    }

    let tree_widget = Paragraph::new(lines)
        .block(block)
        .scroll((state.review.scroll, 0));
    frame.render_widget(tree_widget, main_area);

    let effective = state.review.effective_sessions();
    let total_sessions = effective.len();
    let total_items: usize = effective.iter().map(|s| s.items.len()).sum();
    let fixed_items: usize = effective
        .iter()
        .map(|s| {
            s.items
                .iter()
                .filter(|i| i.status == IssueStatus::Fixed)
                .count()
        })
        .sum();
    let skipped_items: usize = effective
        .iter()
        .map(|s| {
            s.items
                .iter()
                .filter(|i| i.status == IssueStatus::Skipped)
                .count()
        })
        .sum();

    let mut status_spans = Vec::new();
    if let Some(err) = &state.review.error {
        status_spans.push(Span::styled(
            format!(" {}", err),
            engine.fg(palette.status_error).bold(),
        ));
    } else {
        if state.review.running {
            status_spans.push(Span::styled(
                format!(" {} ", active_t("review.in_progress")),
                engine.fg(palette.status_warning).bold(),
            ));
        }
        // The tree was already flattened for this frame, so the status line
        // reads the selected row out of that list instead of flattening and
        // cloning the whole tree a second time (c-2).
        if let Some(row) = rows.get(state.review.selected_index) {
            status_spans.extend(describe_review_row(row, &engine));
        }
        status_spans.push(Span::styled(
            format!(
                "  |  {}",
                active_t("review.stats_summary")
                    .replacen("{}", &total_sessions.to_string(), 1)
                    .replacen("{}", &total_items.to_string(), 1)
                    .replacen("{}", &fixed_items.to_string(), 1)
                    .replacen("{}", &skipped_items.to_string(), 1)
            ),
            engine.fg(palette.text_muted),
        ));
        if state.review.hide_closed {
            status_spans.push(Span::styled(
                format!(" | {}", active_t("review.filter_open")),
                engine.fg(palette.status_warning),
            ));
        }
        if state.review.refining.is_some() {
            status_spans.push(Span::styled(
                format!(" | {}", active_t("review.refining")),
                engine.fg(palette.status_warning).bold(),
            ));
        }
        if !rows.is_empty() {
            status_spans.push(Span::styled(
                format!(
                    " | {}",
                    active_t("ui.row_counter")
                        .replacen("{}", &(state.review.selected_index + 1).to_string(), 1)
                        .replacen("{}", &rows.len().to_string(), 1)
                ),
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
            created_at,
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
                    format!(" {} {}", active_t("review.session_label"), title),
                    engine.fg(palette.text_primary).bold(),
                ),
                Span::styled(format!(" ({})", model), engine.fg(palette.text_muted)),
                Span::styled(
                    format!("  {} ", format_created_at(*created_at)),
                    engine.fg(palette.text_muted),
                ),
                Span::styled(
                    format!(
                        "  {}",
                        active_t("review.stats_breakdown")
                            .replacen("{}", &fixed.to_string(), 1)
                            .replacen("{}", &total.to_string(), 1)
                            .replacen("{}", &todo.to_string(), 1)
                            .replacen("{}", &in_progress.to_string(), 1)
                            .replacen("{}", &discussion.to_string(), 1)
                            .replacen("{}", &skipped.to_string(), 1)
                            .replacen("{}", &rejected.to_string(), 1)
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
                    " {}{}",
                    active_t("review.executive_summary"),
                    if *is_expanded {
                        String::new()
                    } else {
                        active_t("review.folded_suffix")
                    }
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
            description,
            issue_type,
            status,
            model,
            files,
            subplan_architect,
            ..
        } => {
            let mut spans = vec![
                Span::styled(format!(" #{} ", item_id), engine.fg(palette.text_secondary)),
                Span::styled(
                    format!(" [{}] ", issue_type_badge(*issue_type)),
                    issue_type_style(*issue_type),
                ),
                Span::styled(
                    format!(" {} ", issue_status_label(*status)),
                    issue_status_style(*status),
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
            if let Some(desc) = first_content_line(Some(description.as_str())) {
                spans.push(Span::styled(
                    format!("  —  {}", desc),
                    engine.fg(palette.text_muted),
                ));
            }
            if let Some(tier) = subplan_architect {
                spans.push(architect_badge(tier));
            }
            spans
        }
        VisibleReviewRow::SubplanItem {
            item_id,
            step_id,
            title,
            status,
            files,
            details,
            ..
        } => {
            let mut spans = vec![
                Span::styled(format!(" #{} ", item_id), engine.fg(palette.text_secondary)),
                Span::styled(
                    format!(
                        " {} ",
                        active_t("review.step_label").replacen("{}", step_id.as_str(), 1)
                    ),
                    engine.fg(palette.text_secondary),
                ),
                Span::styled(format!(" {} ", status), plan_status_style(*status)),
                Span::styled(title.clone(), engine.fg(palette.text_primary).bold()),
            ];
            if let Some(loc) = location(files) {
                spans.push(Span::styled(
                    format!("  {}", loc),
                    engine.fg(palette.status_info),
                ));
            }
            if let Some(desc) = first_content_line(details.as_deref()) {
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
        use tauqe_protocol::{IssueStatus, IssueType, ReviewIssue, ReviewSession};

        let mut review = ReviewViewState::default();

        let session = ReviewSession {
            id: "rev-1".to_string(),
            title: "Audit".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("reviewer"),
            description: Some("## Executive overview\n\nThe architecture is sound.".to_string()),
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![ReviewIssue {
                id: 1,
                title: "Finding".to_string(),
                model: None,
                issue_type: IssueType::Architecture,
                status: IssueStatus::Todo,
                description: String::new(),
                recommendations: Vec::new(),
                files: Vec::new(),
                readonly_files: Vec::new(),
                plan: Vec::new(),
                note: None,
            }
            .into()],
            raw_markdown: String::new(),
        };
        review.set_session(session);

        let rows = review.flatten_rows();
        let (lines, _offsets) = compute_unified_review_lines(
            &rows,
            review.selected_index,
            80,
            review.theme_mode,
            &mut review.markdown_cache,
        );
        let texts: Vec<String> = lines.iter().map(|line| line.to_string()).collect();

        assert!(
            texts.iter().any(|t| t.contains("Executive Summary:")),
            "the Executive Summary block must remain visible after setting session, got: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("The architecture is sound.")),
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
        state.model.status = crate::views::repl::TurnStatus::Responding;
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

    /// A redraw must never pull a freely scrolled viewport back to the cursor
    /// (c-1): the renderer bounds the scroll by the document length only.
    #[test]
    fn render_review_view_preserves_free_scroll_across_frames() {
        use tauqe_protocol::{IssueStatus, IssueType, ReviewIssue, ReviewItem, ReviewSession};

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = crate::app::test_state();
        let items: Vec<ReviewItem> = (1..=40)
            .map(|id| {
                ReviewItem::from(ReviewIssue {
                    id,
                    title: format!("Finding {id}"),
                    model: None,
                    issue_type: IssueType::Architecture,
                    status: IssueStatus::Todo,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    plan: Vec::new(),
                    note: None,
                })
            })
            .collect();
        state.review.sessions = vec![ReviewSession {
            id: "rev-scroll".to_string(),
            title: "Scroll".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items,
            raw_markdown: String::new(),
        }];
        // The cursor rests on the first row while the developer scrolls away.
        state.review.selected_index = 0;
        state.review.scroll = 5;

        terminal
            .draw(|f| {
                let area = f.area();
                let main_area = Rect::new(0, 0, area.width, area.height.saturating_sub(1));
                let info_area = Rect::new(0, area.height.saturating_sub(1), area.width, 1);
                render_review_view(f, &mut state, main_area, info_area);
            })
            .unwrap();

        assert!(
            state.review.rendered_lines > 24,
            "the document must be longer than the viewport"
        );
        assert_eq!(
            state.review.scroll, 5,
            "a redraw must not snap the viewport back to the selected row"
        );
    }

    /// The renderer bounds the viewport by the document length and nothing
    /// else: a free scroll deep inside a tall expanded subplan survives the
    /// redraw, and only an offset past the last screenful is clamped (c-1).
    #[test]
    fn render_review_view_keeps_a_deep_free_scroll_and_clamps_only_the_tail() {
        use tauqe_protocol::{
            IssueStatus, IssueType, PlanItem, PlanItemStatus, ReviewIssue, ReviewItem,
            ReviewSession,
        };

        fn render_once(terminal: &mut Terminal<TestBackend>, state: &mut crate::app::AppState) {
            terminal
                .draw(|f| {
                    let area = f.area();
                    let main_area = Rect::new(0, 0, area.width, area.height.saturating_sub(1));
                    let info_area = Rect::new(0, area.height.saturating_sub(1), area.width, 1);
                    render_review_view(f, state, main_area, info_area);
                })
                .unwrap();
        }

        let long_details = (0..120)
            .map(|line| format!("Invariant line {line}."))
            .collect::<Vec<_>>()
            .join("\n");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut state = crate::app::test_state();
        state.review.sessions = vec![ReviewSession {
            id: "rev-tall".to_string(),
            title: "Tall".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![ReviewItem::from(ReviewIssue {
                id: 1,
                title: "Finding".to_string(),
                model: None,
                issue_type: IssueType::Architecture,
                status: IssueStatus::Todo,
                description: String::new(),
                recommendations: Vec::new(),
                files: Vec::new(),
                readonly_files: Vec::new(),
                plan: vec![PlanItem {
                    id: "1.1".to_string(),
                    title: "Tall step".to_string(),
                    details: Some(long_details),
                    status: PlanItemStatus::Todo,
                    ..Default::default()
                }],
                note: None,
            })],
            raw_markdown: String::new(),
        }];
        state.review.toggle_item_expanded("rev-tall", 1);
        state
            .review
            .toggle_subplan_item_expanded("rev-tall", 1, "1.1");
        // The cursor rests on the session header while the developer reads deep
        // inside the step, so no redraw may re-anchor the viewport on row zero.
        state.review.selected_index = 0;
        state.review.scroll = 40;

        render_once(&mut terminal, &mut state);
        assert!(
            state.review.rendered_lines > 40 + state.review.view_height as usize,
            "the expanded step must extend past the free scroll offset"
        );
        assert_eq!(
            state.review.scroll, 40,
            "a redraw must not snap a deep free scroll back to the cursor"
        );

        let max = u16::try_from(state.review.rendered_lines)
            .unwrap_or(u16::MAX)
            .saturating_sub(state.review.view_height);
        state.review.scroll = u16::MAX;
        render_once(&mut terminal, &mut state);
        assert_eq!(
            state.review.scroll, max,
            "the renderer clamps the viewport by the document length only"
        );
    }
}
