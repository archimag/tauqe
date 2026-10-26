//! Three-way batch cancellation dialog.
//!
//! A running batch — a plan pipeline or a review queue — is stopped through an
//! explicit three-way choice instead of a binary confirm, so a stop is a
//! deliberate decision rather than an accidental dismissal: `Continue` keeps the
//! queue running, `Soft Stop` drains the current member, and `Hard Stop` aborts
//! it immediately (Conventions §6.3).

use ratatui::layout::{Alignment, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{AppState, BatchCancelButton};
use crate::components::modal::{
    dialog_dismiss_hint, dialog_navigation_hint, modal_geometry, render_dialog_footer, HintGroup,
};
use crate::i18n::active_t;
use crate::theme::engine::current_engine;

use super::ModalTone;

const BATCH_CANCEL_WIDTH: u16 = 64;
const BATCH_CANCEL_GAP: u16 = 2;

/// Action-button labels of the batch-cancellation dialog, in left-to-right
/// order, resolved from the locale. The geometry and the renderer derive their
/// layout from this single list so the drawn buttons are always the clickable
/// ones (Conventions §6, §9).
fn batch_cancel_labels() -> [String; 3] {
    [
        format!(" {} ", active_t("modal.continue")),
        format!(" {} ", active_t("modal.soft_stop")),
        format!(" {} ", active_t("modal.hard_stop")),
    ]
}

/// Body content of the batch-cancellation dialog.
///
/// The three actions are described explicitly so the choice between keeping the
/// queue, stopping after the current member, and aborting immediately is never
/// ambiguous (Conventions.md §7).
pub fn batch_cancel_body_lines() -> Vec<Line<'static>> {
    let engine = current_engine();
    let palette = engine.palette();
    vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.batch_cancel.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::raw(active_t("dialog.batch_cancel.continue_note")),
        Line::raw(active_t("dialog.batch_cancel.soft_stop_note")),
        Line::from(Span::styled(
            active_t("dialog.batch_cancel.hard_stop_note"),
            engine.fg(palette.text_muted),
        )),
    ]
}

/// Exact geometry and action-button bounds of the batch-cancellation dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchCancelModalGeometry {
    pub area: Rect,
    pub continue_button: Rect,
    pub soft_stop_button: Rect,
    pub hard_stop_button: Rect,
}

/// Computes the geometry and button bounds of the batch-cancellation dialog so
/// that rendering and mouse hit-testing agree on the same layout.
///
/// The card's bands come from the shared [`modal_geometry`] formula, so the
/// buttons sit on the padded action row the renderer draws into and the drawn
/// surface is always the clickable one (Conventions §6.6–§6.8).
pub fn batch_cancel_modal_geometry(term: Rect) -> BatchCancelModalGeometry {
    let body_lines_count = batch_cancel_body_lines().len();
    // The shared frame reserves six chrome rows — two borders, the top inset,
    // the pre-action spacer, the action row, and the docked hint strip — so the
    // card is sized from its body plus two rows of wrap slack.
    let needed_height = (body_lines_count as u16).saturating_add(8).max(10);
    let height_percent = (needed_height * 100 / term.height.max(1)).clamp(25, 90);
    let area = crate::ui::centered_rect(BATCH_CANCEL_WIDTH, height_percent, term);

    // The shared modal formula owns the top inset, the horizontal margins, the
    // pre-action spacer, and the docked hint row, so the buttons are placed on
    // the padded action bar the renderer draws into and hit-testing can never
    // drift from the rendered layout (Conventions §6.7, §6.8).
    let action_bar = modal_geometry(area).action_bar;
    let btn_y = action_bar.y;

    let labels = batch_cancel_labels();
    let widths: [u16; 3] = [
        UnicodeWidthStr::width(labels[0].as_str()) as u16,
        UnicodeWidthStr::width(labels[1].as_str()) as u16,
        UnicodeWidthStr::width(labels[2].as_str()) as u16,
    ];
    let total = widths[0] + widths[1] + widths[2] + BATCH_CANCEL_GAP * 2;
    let start = action_bar.x + action_bar.width.saturating_sub(total) / 2;

    let continue_button = Rect::new(start, btn_y, widths[0], 1);
    let soft_x = start + widths[0] + BATCH_CANCEL_GAP;
    let soft_stop_button = Rect::new(soft_x, btn_y, widths[1], 1);
    let hard_x = soft_x + widths[1] + BATCH_CANCEL_GAP;
    let hard_stop_button = Rect::new(hard_x, btn_y, widths[2], 1);

    BatchCancelModalGeometry {
        area,
        continue_button,
        soft_stop_button,
        hard_stop_button,
    }
}

/// Footer hint of the batch-cancellation dialog.
///
/// Navigation and dismissal are the shared dialog vocabulary, projected through
/// the configured modifiers; the fast-select accelerators are the batch's own
/// `Continue` / `Soft Stop` / `Hard Stop` keys, so they are declared here as a
/// structured group rather than spliced into a formatted string.
fn batch_cancel_hint() -> Vec<HintGroup> {
    let mut groups = dialog_navigation_hint();
    groups.push(HintGroup {
        chords: vec!["c".to_string(), "s".to_string(), "h".to_string()],
        verb: active_t("dialog.action.fast_select"),
    });
    groups.extend(dialog_dismiss_hint());
    groups
}

/// Renders the three-way batch-cancellation dialog.
pub fn render_confirm_batch_cancel_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let engine = current_engine();
    let palette = engine.palette();
    let geom = batch_cancel_modal_geometry(frame.area());

    let title = active_t("dialog.batch_cancel.title");
    crate::components::modal::render_modal_container(frame, geom.area, &title, ModalTone::Warning);

    // The container is drawn from the shared frame, so the body, the action bar,
    // and the docked hint strip are read from the same air-aware formula the
    // button hit-testing uses (Conventions §6.6–§6.8).
    let geometry = modal_geometry(geom.area);

    let body = Paragraph::new(batch_cancel_body_lines())
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false });
    frame.render_widget(body, geometry.body);

    let [l0, l1, l2] = batch_cancel_labels();
    let focused = state.batch_cancel_button;
    let button_style = |button: BatchCancelButton| {
        if button == focused {
            engine
                .bg(palette.button_active_bg)
                .fg(engine.color(palette.text_inverse))
                .bold()
        } else {
            engine
                .bg(palette.button_bg)
                .fg(engine.color(palette.button_fg))
        }
    };

    let actions_line = Line::from(vec![
        Span::styled(l0, button_style(BatchCancelButton::Continue)),
        Span::raw("  "),
        Span::styled(l1, button_style(BatchCancelButton::SoftStop)),
        Span::raw("  "),
        Span::styled(l2, button_style(BatchCancelButton::HardStop)),
    ]);
    let actions_widget = Paragraph::new(actions_line).alignment(Alignment::Center);
    frame.render_widget(actions_widget, geometry.action_bar);

    render_dialog_footer(frame, geometry.hint_row, &batch_cancel_hint());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn batch_cancel_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 30);
        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        let mut state = crate::app::test_state();
        state.confirm_batch_cancel = true;

        terminal
            .draw(|f| render_confirm_batch_cancel_popup(f, &state))
            .unwrap();

        let geom = batch_cancel_modal_geometry(term);
        let geometry = modal_geometry(geom.area);

        assert_eq!(geom.continue_button.y, geometry.action_bar.y);
        assert_eq!(geom.soft_stop_button.y, geometry.action_bar.y);
        assert_eq!(geom.hard_stop_button.y, geometry.action_bar.y);
        assert!(geom.continue_button.x >= geometry.action_bar.x);
        assert!(
            geom.hard_stop_button.x + geom.hard_stop_button.width
                <= geometry.action_bar.x + geometry.action_bar.width
        );
        assert_eq!(geometry.body.y, geom.area.y + 2, "border + top inset");
        assert_eq!(
            geometry.body.x,
            geom.area.x + 2,
            "border + horizontal margin"
        );
        assert_eq!(
            geometry.hint_row.y + geometry.hint_row.height,
            geom.area.y + geom.area.height - 1,
            "hint strip is docked above the bottom border"
        );

        let buffer = terminal.backend().buffer();
        let read = |rect: Rect| -> String {
            (rect.x..rect.x + rect.width)
                .map(|x| buffer[(x, rect.y)].symbol())
                .collect()
        };

        assert!(read(geom.continue_button).contains("Continue"));
        assert!(read(geom.soft_stop_button).contains("Soft Stop"));
        assert!(read(geom.hard_stop_button).contains("Hard Stop"));
    }
}
