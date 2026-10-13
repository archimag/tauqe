use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

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

    render_prompt_input(frame, state, input_area, prompt_lines, cursor_visual_line);
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

fn render_prompt_input(
    frame: &mut ratatui::Frame,
    state: &AppState,
    area: Rect,
    input_lines: Vec<Line<'static>>,
    cursor_visual_line: usize,
) {
    let is_busy = state.model.is_busy();
    let visible_input_lines = area.height.saturating_sub(2);
    let input_scroll =
        if visible_input_lines > 0 && cursor_visual_line >= visible_input_lines as usize {
            (cursor_visual_line + 1 - visible_input_lines as usize) as u16
        } else {
            0
        };

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
