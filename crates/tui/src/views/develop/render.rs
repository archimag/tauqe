use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use tauqe_protocol::DiscussionFocus;

use crate::app::AppState;
use crate::ui::wrap_line;

use super::{compute_model_lines, DevelopFocus, SPINNER_FRAMES};

/// Maximum number of batch steps rendered in the Develop Plan Stepper.
const MAX_STEPPER_STEPS: usize = 8;

pub fn render_develop_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    model_area: Rect,
    input_area: Rect,
    prompt_lines: Vec<Line<'static>>,
    cursor_visual_line: usize,
) {
    // While a discussion focus is active, reserve the single row directly above
    // the prompt for its status banner. The banner is a pure indicator, so it is
    // carved out of the response area rather than the prompt itself
    // (c-visual-anchor).
    let (model_area, focus_banner_area) = if state.discussion_focus.is_active()
        && model_area.height > 1
    {
        let chunks =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(model_area);
        (chunks[0], Some(chunks[1]))
    } else {
        (model_area, None)
    };

    if let Some(lines) = plan_stepper_lines(state) {
        let body = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let desired = body.saturating_add(2);
        let max_allowed = model_area.height.saturating_sub(4);
        let stepper_height = desired.min(max_allowed);

        if stepper_height >= 3 {
            let chunks = Layout::vertical([
                Constraint::Length(stepper_height),
                Constraint::Min(0),
            ])
            .split(model_area);
            render_plan_stepper(frame, lines, chunks[0]);
            render_model_pane(frame, state, chunks[1]);
        } else {
            render_model_pane(frame, state, model_area);
        }
    } else {
        render_model_pane(frame, state, model_area);
    }

    if let Some(banner_area) = focus_banner_area {
        render_discussion_focus_banner(frame, state, banner_area);
    }

    render_prompt_input(frame, state, input_area, prompt_lines, cursor_visual_line);
}

/// Renders the active discussion focus as a one-line status banner above the prompt.
///
/// The banner is a pure indicator (Conventions.md §2): it names the focused
/// entity, its title, the number of targeted items, and the detach shortcut, but
/// carries no command of its own. The server owns the authoritative focus; this
/// only mirrors it for display.
fn render_discussion_focus_banner(frame: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let (badge, badge_bg, id, title, focused_count) = match &state.discussion_focus {
        DiscussionFocus::Plan {
            plan_id,
            focused_items,
        } => {
            let title = state
                .plans_view
                .plans
                .iter()
                .find(|plan| &plan.id == plan_id)
                .map(|plan| plan.title.clone())
                .unwrap_or_else(|| plan_id.clone());
            (
                "[PLAN]",
                Color::Cyan,
                plan_id.clone(),
                title,
                focused_items.len(),
            )
        }
        DiscussionFocus::Review {
            review_id,
            focused_items,
        } => {
            let title = state
                .review
                .sessions
                .iter()
                .find(|session| &session.id == review_id)
                .map(|session| session.title.clone())
                .unwrap_or_else(|| review_id.clone());
            (
                "[REVIEW]",
                Color::Magenta,
                review_id.clone(),
                title,
                focused_items.len(),
            )
        }
        DiscussionFocus::None => return,
    };

    let mut spans = vec![
        Span::styled(
            format!(" {} ", badge),
            Style::default().bg(badge_bg).fg(Color::Black).bold(),
        ),
        Span::raw(" "),
        Span::styled(id, Style::default().fg(Color::Cyan).bold()),
        Span::raw(" \u{2014} "),
        Span::styled(
            title.replace(['\r', '\n'], " "),
            Style::default().fg(Color::White),
        ),
    ];

    if focused_count > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("({} focused)", focused_count),
            Style::default().fg(Color::DarkGray),
        ));
    }

    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        "[Esc to detach]",
        Style::default().fg(Color::DarkGray).bold(),
    ));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Builds the compact Plan Stepper header for the active plan execution context.
///
/// Returns `None` for ad-hoc turns (no plan context) or when the referenced
/// plan is not loaded, so Develop keeps its standard layout with zero overhead.
fn plan_stepper_lines(state: &AppState) -> Option<Vec<Line<'static>>> {
    let plan_ctx = state.model.plan_context.as_ref()?;
    let plan = state
        .plans_view
        .plans
        .iter()
        .find(|plan| plan.id == plan_ctx.plan_id)?;

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Plan: ", Style::default().fg(Color::DarkGray).bold()),
        Span::styled(plan.title.clone(), Style::default().fg(Color::Cyan).bold()),
    ]));

    let step_ids: Vec<&str> = if plan_ctx.batch_step_ids.is_empty() {
        vec![plan_ctx.step_id.as_str()]
    } else {
        plan_ctx.batch_step_ids.iter().map(String::as_str).collect()
    };

    for id in step_ids.into_iter().take(MAX_STEPPER_STEPS) {
        let Some(item) = plan.find_item(id) else {
            continue;
        };
        let is_active = id == plan_ctx.step_id;

        let (badge, badge_style) = if is_active {
            (
                SPINNER_FRAMES[state.model.spinner_frame % SPINNER_FRAMES.len()],
                Style::default().fg(Color::Yellow).bold(),
            )
        } else {
            plan_status_badge(item.status)
        };

        let title_style = if is_active {
            Style::default().fg(Color::White).bold()
        } else {
            Style::default().fg(Color::Gray)
        };

        lines.push(Line::from(vec![
            Span::styled(
                if is_active { "> " } else { "  " },
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(badge.to_string(), badge_style),
            Span::raw(" "),
            Span::styled(
                format!("#{} ", item.id),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(item.title.clone(), title_style),
        ]));
    }

    Some(lines)
}

fn plan_status_badge(status: tauqe_protocol::PlanItemStatus) -> (&'static str, Style) {
    match status {
        tauqe_protocol::PlanItemStatus::Done => ("✓", Style::default().fg(Color::Green).bold()),
        tauqe_protocol::PlanItemStatus::InProgress => {
            ("▶", Style::default().fg(Color::Yellow).bold())
        }
        tauqe_protocol::PlanItemStatus::Todo => ("○", Style::default().fg(Color::DarkGray)),
        tauqe_protocol::PlanItemStatus::Blocked => ("!", Style::default().fg(Color::Red).bold()),
        tauqe_protocol::PlanItemStatus::Cancelled => ("✗", Style::default().fg(Color::Red)),
        tauqe_protocol::PlanItemStatus::Discussion => {
            ("?", Style::default().fg(Color::Magenta).bold())
        }
    }
}

fn render_plan_stepper(frame: &mut ratatui::Frame, lines: Vec<Line<'static>>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1));
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn render_model_pane(frame: &mut ratatui::Frame, state: &mut AppState, area: Rect) {
    let model_block = Block::default().padding(Padding::horizontal(1));
    let inner = model_block.inner(area);

    let content_height = inner.height;
    let content_width = (inner.width as usize).max(10);
    state.last_model_height = content_height;
    state.model.content_rect = (
        inner.x,
        inner.y,
        inner.width,
        content_height,
    );

    if state.model.content_width != content_width {
        state.model.content_width = content_width;
        state.model.reasoning.max_width = Some(content_width);
        state.model.render_markdown_now();
        if !state.model.reasoning.is_empty() {
            state.model.reasoning.update_markdown();
        }
    }

    if let Some((_, inst)) = state.model.copy_flash {
        if inst.elapsed().as_secs_f32() >= 2.0 {
            state.model.copy_flash = None;
            state.model.update_markdown();
        }
    }

    let raw_lines = compute_model_lines(&state.model);

    let prefix_count = (state.model.error.is_some() as usize)
        + (state.model.toolchain.is_some() as usize);

    let mut raw_to_visual_start = Vec::with_capacity(raw_lines.len());
    let mut raw_to_visual_end = Vec::with_capacity(raw_lines.len());
    let mut model_lines = Vec::with_capacity(raw_lines.len());
    let mut current_visual = 0;
    for line in raw_lines {
        let wrapped = wrap_line(line, content_width);
        let wrapped_count = wrapped.len();
        raw_to_visual_start.push(current_visual);
        raw_to_visual_end.push(current_visual + wrapped_count.saturating_sub(1));
        current_visual += wrapped_count;
        model_lines.extend(wrapped);
    }

    for block in &mut state.model.code_blocks {
        let b_raw_start = prefix_count + block.raw_start_line;
        let b_raw_end = prefix_count + block.raw_end_line;
        if b_raw_start < raw_to_visual_start.len() && b_raw_end < raw_to_visual_end.len() {
            block.visual_start_line = raw_to_visual_start[b_raw_start];
            block.visual_end_line = raw_to_visual_end[b_raw_end];
        }
    }

    let total_lines = model_lines.len() as u16;
    state.model.rendered_lines_count = model_lines.len();

    let max_scroll = total_lines.saturating_sub(content_height);
    if state.model.auto_scroll || state.model.scroll > max_scroll {
        state.model.scroll = max_scroll;
    }

    let scroll_start = state.model.scroll as usize;
    let visible_count = content_height as usize;
    let visible_lines: Vec<Line<'static>> = model_lines
        .into_iter()
        .skip(scroll_start)
        .take(visible_count)
        .collect();

    let model_paragraph = Paragraph::new(visible_lines).block(model_block);
    frame.render_widget(model_paragraph, area);
}

/// Computes the prompt editor scroll offset so the cursor stays visible,
/// moving the viewport only when the cursor leaves the visible window.
///
/// The offset is retained across frames: recomputing it from scratch pinned
/// the cursor to the bottom row and made upward navigation jitter and stick
/// at the lower edge. Scrolling now happens only at the window boundaries
/// (visibility hysteresis), matching the Emacs-style `recenter` behavior
/// users expect from a multiline prompt.
fn ensure_visible_input_scroll(
    current_scroll: usize,
    cursor_visual_line: usize,
    visible_input_lines: usize,
    total_lines: usize,
) -> usize {
    if visible_input_lines == 0 || total_lines <= visible_input_lines {
        return 0;
    }

    let max_scroll = total_lines - visible_input_lines;
    let mut scroll = current_scroll.min(max_scroll);

    if cursor_visual_line < scroll {
        scroll = cursor_visual_line;
    } else if cursor_visual_line >= scroll + visible_input_lines {
        scroll = cursor_visual_line + 1 - visible_input_lines;
    }

    scroll.min(max_scroll)
}

fn render_prompt_input(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    area: Rect,
    input_lines: Vec<Line<'static>>,
    cursor_visual_line: usize,
) {
    let is_busy = state.model.is_busy();
    let visible_input_lines = area.height.saturating_sub(2) as usize;

    // Keep the prompt scroll stateful: the viewport moves only when the
    // cursor leaves the visible window, instead of snapping to the bottom
    // row on every frame (c-prompt-input-scroll-hysteresis).
    let input_scroll = ensure_visible_input_scroll(
        state.model.input_scroll as usize,
        cursor_visual_line,
        visible_input_lines,
        input_lines.len(),
    ) as u16;
    state.model.input_scroll = input_scroll;

    let in_viewport = state.model.focus == DevelopFocus::Viewport;
    let border_style = if is_busy {
        Style::default().fg(Color::Yellow)
    } else if in_viewport {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };

    let mut input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    if in_viewport {
        input_block = input_block.title(" [VIEWPORT] ");
    }

    let input_paragraph = Paragraph::new(input_lines)
        .block(input_block)
        .scroll((input_scroll, 0));
    frame.render_widget(input_paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::ensure_visible_input_scroll;

    #[test]
    fn moving_within_the_window_keeps_the_scroll_stable() {
        // Moving the cursor up inside the visible window must not shift the view.
        assert_eq!(ensure_visible_input_scroll(10, 14, 5, 40), 10);
        assert_eq!(ensure_visible_input_scroll(10, 10, 5, 40), 10);
    }

    #[test]
    fn cursor_above_the_window_scrolls_up_to_reveal_it() {
        assert_eq!(ensure_visible_input_scroll(10, 7, 5, 40), 7);
        assert_eq!(ensure_visible_input_scroll(10, 0, 5, 40), 0);
    }

    #[test]
    fn cursor_below_the_window_scrolls_down_just_enough() {
        assert_eq!(ensure_visible_input_scroll(0, 5, 5, 40), 1);
        assert_eq!(ensure_visible_input_scroll(3, 20, 5, 40), 16);
    }

    #[test]
    fn short_content_or_empty_window_pins_scroll_to_zero() {
        assert_eq!(ensure_visible_input_scroll(4, 2, 10, 6), 0);
        assert_eq!(ensure_visible_input_scroll(4, 2, 0, 10), 0);
        assert_eq!(ensure_visible_input_scroll(0, 0, 5, 5), 0);
    }

    #[test]
    fn scroll_never_exceeds_maximum() {
        // A stale offset larger than the content clamps to the last page.
        assert_eq!(ensure_visible_input_scroll(100, 39, 5, 40), 35);
    }

    #[test]
    fn upward_navigation_is_monotonic_without_jitter() {
        let visible = 5usize;
        let total = 20usize;
        let mut scroll = 0usize;

        for cursor in 0..total {
            scroll = ensure_visible_input_scroll(scroll, cursor, visible, total);
        }
        assert_eq!(scroll, total - visible);

        let mut observed = Vec::new();
        for cursor in (0..total).rev() {
            scroll = ensure_visible_input_scroll(scroll, cursor, visible, total);
            observed.push(scroll);
        }

        // While the cursor stays inside the window the scroll must not move;
        // it only follows once the cursor rises above the top row.
        assert_eq!(observed[0], total - visible);
        assert_eq!(observed[1], total - visible);
        assert!(observed.iter().all(|&s| s <= total - visible));
        assert_eq!(*observed.last().unwrap(), 0);
    }
}
