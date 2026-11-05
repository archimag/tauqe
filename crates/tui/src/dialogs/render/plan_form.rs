//! Shared rendering of the plan-synthesis dialog family.
//!
//! The plan creation form and the plan refinement form stack the same bands:
//! a header, the Planner ladder, the Critic ladder with its disable switch, the
//! instructions editor, the action bar, and the docked hint strip. Drawing them
//! from one implementation keeps the two surfaces identical and keeps the
//! geometry the mouse hit-tester reads in lockstep with the drawn card
//! (Conventions §6.6–§6.8, c-typed-plan-creation-dialog,
//! c-typed-plan-mutation-dialog).

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::components::modal::{render_dialog_footer, verb_hint};
use crate::components::model_selector::ModelSelector;
use crate::editor::InputEditor;
use crate::i18n::active_t;
use crate::theme::engine::current_engine;

use super::editor_lines;

/// Most rows one Architect ladder draws at once before it scrolls.
const LIST_MAX_ROWS: u16 = 3;
/// Smallest instructions editor the card keeps alive.
const EDITOR_MIN_HEIGHT: u16 = 3;
/// One blank row between two heterogeneous bands (Conventions §6.7).
const SPACER_HEIGHT: u16 = 1;

/// Rectangles shared by the plan-synthesis renderers and the mouse hit-tester.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlanFormGeometry {
    /// Whole dialog frame.
    pub(crate) area: Rect,
    /// Header block above the ladders.
    pub(crate) header: Rect,
    /// Planner ladder block, borders included.
    pub(crate) planner: Rect,
    /// Inner rows of the Planner ladder.
    pub(crate) planner_rows: Rect,
    /// Critic ladder block, borders included.
    pub(crate) critic: Rect,
    /// Inner rows of the Critic ladder.
    pub(crate) critic_rows: Rect,
    /// Instructions editor block, borders included.
    pub(crate) editor_area: Rect,
    /// Action bar holding the confirm and cancel buttons.
    pub(crate) action_bar: Rect,
    /// One-row docked hint strip pinned to the card's bottom edge.
    pub(crate) footer: Rect,
    /// Confirm button.
    pub(crate) confirm_button: Rect,
    /// Cancel button.
    pub(crate) cancel_button: Rect,
}

/// Largest card the developer can reach on `term`, never below a readable floor.
///
/// Two Architect ladders plus an editor need more vertical room than a
/// single-list picker, so the percentage sizing of short terminals is widened
/// to the readable minimum instead of collapsing the Critic band.
pub(crate) fn plan_form_area(term: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let area = crate::ui::centered_rect(width_percent, height_percent, term);
    let min_width = 60.min(term.width);
    let min_height = 22.min(term.height);
    if area.width >= min_width && area.height >= min_height {
        return area;
    }
    let width = area.width.max(min_width).min(term.width);
    let height = area.height.max(min_height).min(term.height);
    Rect::new(
        term.x + term.width.saturating_sub(width) / 2,
        term.y + term.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

/// Height of one bordered ladder block for `entry_count` entries.
fn list_block_height(entry_count: usize) -> u16 {
    let rows = match entry_count.min(usize::from(LIST_MAX_ROWS)) {
        0 => 1,
        rows => u16::try_from(rows).unwrap_or(LIST_MAX_ROWS),
    };
    rows.saturating_add(2)
}

/// Splits the card into its stacked bands.
pub(crate) fn plan_form_geometry(
    area: Rect,
    planner_count: usize,
    critic_count: usize,
    header_height: u16,
    confirm_label: &str,
    cancel_label: &str,
) -> PlanFormGeometry {
    let geometry = crate::components::modal::modal_geometry(area);
    let body = geometry.body;
    let mut remaining = body.height;
    let mut cursor = body.y;

    let header_h = header_height.min(remaining);
    let header = Rect::new(body.x, cursor, body.width, header_h);
    cursor = cursor.saturating_add(header_h);
    remaining = remaining.saturating_sub(header_h);

    let gap = SPACER_HEIGHT.min(remaining);
    cursor = cursor.saturating_add(gap);
    remaining = remaining.saturating_sub(gap);

    let reserved = SPACER_HEIGHT
        .saturating_mul(2)
        .saturating_add(EDITOR_MIN_HEIGHT);
    let planner_h = list_block_height(planner_count).min(remaining.saturating_sub(reserved));
    let planner = Rect::new(body.x, cursor, body.width, planner_h);
    cursor = cursor.saturating_add(planner_h);
    remaining = remaining.saturating_sub(planner_h);

    let gap = SPACER_HEIGHT.min(remaining);
    cursor = cursor.saturating_add(gap);
    remaining = remaining.saturating_sub(gap);

    let critic_h = list_block_height(critic_count)
        .min(remaining.saturating_sub(SPACER_HEIGHT.saturating_add(EDITOR_MIN_HEIGHT)));
    let critic = Rect::new(body.x, cursor, body.width, critic_h);
    cursor = cursor.saturating_add(critic_h);
    remaining = remaining.saturating_sub(critic_h);

    let gap = SPACER_HEIGHT.min(remaining);
    cursor = cursor.saturating_add(gap);
    remaining = remaining.saturating_sub(gap);

    let editor_area = Rect::new(body.x, cursor, body.width, remaining);

    let (confirm_button, cancel_button) =
        super::picker_button_rects(geometry.action_bar, confirm_label, cancel_label);

    PlanFormGeometry {
        area,
        header,
        planner,
        planner_rows: super::inset(planner),
        critic,
        critic_rows: super::inset(critic),
        editor_area,
        action_bar: geometry.action_bar,
        footer: geometry.hint_row,
        confirm_button,
        cancel_button,
    }
}

/// Everything one plan-synthesis card draws.
pub(crate) struct PlanFormView<'a> {
    pub(crate) area: Rect,
    pub(crate) title: String,
    pub(crate) header_height: u16,
    pub(crate) header_lines: Vec<Line<'static>>,
    /// Block title used verbatim; the localized catalog strings carry their own
    /// surrounding spaces.
    pub(crate) planner_title: String,
    pub(crate) planner: &'a ModelSelector,
    pub(crate) planner_focused: bool,
    pub(crate) critic_title: String,
    pub(crate) critic: &'a ModelSelector,
    pub(crate) critic_focused: bool,
    /// Raw editor label; the renderer wraps it in surrounding spaces.
    pub(crate) editor_title: String,
    pub(crate) editor_placeholder: String,
    pub(crate) editor: &'a InputEditor,
    pub(crate) editor_focused: bool,
    pub(crate) confirm_label: String,
    pub(crate) cancel_label: String,
    pub(crate) confirm_selected: bool,
    pub(crate) actions_focused: bool,
    pub(crate) hint: String,
}

/// Draws one bordered Architect ladder with its scrollable rows.
fn render_ladder(
    frame: &mut ratatui::Frame,
    area: Rect,
    title: String,
    selector: &ModelSelector,
    focused: bool,
) {
    let engine = current_engine();
    let palette = engine.palette();
    let border = if focused {
        engine.fg(palette.status_info).bold()
    } else {
        engine.fg(palette.border)
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border)
        .style(Style::default().bg(engine.color(palette.surface)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if area.height < 3 || area.width < 3 {
        return;
    }
    if selector.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                active_t("dialog.plan_form.no_models"),
                engine.fg(palette.text_muted),
            ))),
            inner,
        );
        return;
    }
    let rows = selector.render_list_lines(inner.height as usize);
    frame.render_widget(Paragraph::new(rows), inner);
}

/// Renders the whole plan-synthesis card.
pub(crate) fn render_plan_form(frame: &mut ratatui::Frame, view: &PlanFormView<'_>) {
    let engine = current_engine();
    let palette = engine.palette();
    let dim = engine.fg(palette.text_muted);
    frame.render_widget(Clear, view.area);

    let outer_block = Block::default()
        .title(format!(" {} ", view.title))
        .borders(Borders::ALL)
        .border_style(engine.fg(palette.status_info))
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(outer_block, view.area);

    let geometry = plan_form_geometry(
        view.area,
        view.planner.len(),
        view.critic.len(),
        view.header_height,
        &view.confirm_label,
        &view.cancel_label,
    );

    frame.render_widget(Paragraph::new(view.header_lines.clone()), geometry.header);

    render_ladder(
        frame,
        geometry.planner,
        view.planner_title.clone(),
        view.planner,
        view.planner_focused,
    );
    render_ladder(
        frame,
        geometry.critic,
        view.critic_title.clone(),
        view.critic,
        view.critic_focused,
    );

    // Instructions editor. The cursor is drawn only when the editor owns focus.
    let show_cursor = view.editor_focused;
    let mut editor_content = Vec::new();
    if view.editor.is_empty() {
        let mut spans = vec![Span::raw("> ")];
        if show_cursor {
            spans.push(Span::styled("█", engine.fg(palette.status_warning)));
        }
        spans.push(Span::styled(view.editor_placeholder.clone(), dim));
        editor_content.push(Line::from(spans));
    } else {
        for line in editor_lines(view.editor, show_cursor) {
            let mut spans = vec![Span::raw("> ")];
            spans.extend(line.spans);
            editor_content.push(Line::from(spans));
        }
    }
    let editor_border = if view.editor_focused {
        engine.fg(palette.status_success).bold()
    } else {
        engine.fg(palette.border)
    };
    let editor_block = Block::default()
        .title(format!(" {} ", view.editor_title))
        .borders(Borders::ALL)
        .border_style(editor_border)
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(
        Paragraph::new(editor_content)
            .block(editor_block)
            .wrap(Wrap { trim: false }),
        geometry.editor_area,
    );

    // Action bar with focus-aware styling.
    let (confirm_style, cancel_style) = if view.actions_focused {
        if view.confirm_selected {
            (
                engine
                    .bg(palette.status_success)
                    .fg(engine.color(palette.text_inverse))
                    .bold(),
                engine
                    .bg(palette.button_bg)
                    .fg(engine.color(palette.button_fg)),
            )
        } else {
            (
                engine
                    .bg(palette.button_bg)
                    .fg(engine.color(palette.button_fg)),
                engine
                    .bg(palette.button_active_bg)
                    .fg(engine.color(palette.text_inverse))
                    .bold(),
            )
        }
    } else {
        (
            engine
                .bg(palette.button_bg)
                .fg(engine.color(palette.button_fg)),
            engine
                .bg(palette.button_bg)
                .fg(engine.color(palette.button_fg)),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(view.confirm_label.clone(), confirm_style),
            Span::raw("    "),
            Span::styled(view.cancel_label.clone(), cancel_style),
        ]))
        .alignment(Alignment::Center),
        geometry.action_bar,
    );

    // The hint is docked to the card's bottom edge as its own strip filled with
    // `palette.overlay`, so it reads as persistent chrome instead of a second
    // row of buttons (Conventions §6.4).
    render_dialog_footer(frame, geometry.footer, &verb_hint(view.hint.as_str()));
}
