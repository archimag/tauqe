use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::components::modal::{
    modal_geometry, render_dialog_footer, render_modal_container_styled, verb_hint,
};
use crate::i18n::active_t;
use crate::theme::engine::current_engine;

/// Full drawn select-button label, resolved from the runtime locale.
///
/// Rendering and mouse hit-testing both measure this label, so the clickable
/// button can never drift from the drawn one (Conventions.md §6, §9).
fn select_button_label() -> String {
    format!(" [ {} ] ", active_t("dialog.button.select"))
}

/// Full drawn cancel-button label, resolved from the runtime locale.
fn cancel_button_label() -> String {
    format!(" [ {} ] ", active_t("modal.cancel"))
}

/// Spacing drawn between the two action buttons.
const BUTTON_GAP: &str = "    ";

/// Rows of card chrome the shared modal frame reserves around the content: the
/// two border rows, the top inset, the intro line, its inter-section spacer, the
/// pre-action spacer, the action bar, and the docked hint strip.
const SELECTION_DIALOG_CHROME_ROWS: u16 = 8;

/// Single source of truth for the model picker geometry, shared by rendering
/// and mouse hit-testing.
///
/// The height budget is the chrome the shared frame owns plus one row per rung,
/// so the list is never clipped by the air around it
/// (Conventions.md §6.7, §6.8).
pub fn selection_dialog_area(term: Rect, item_count: usize) -> Rect {
    let items = u16::try_from(item_count).unwrap_or(u16::MAX);
    let height = items
        .saturating_add(SELECTION_DIALOG_CHROME_ROWS)
        .clamp(11, 24)
        .min(term.height);
    let width = (term.width.saturating_mul(70) / 100)
        .clamp(40, 80)
        .min(term.width);
    Rect::new(
        term.x + (term.width - width) / 2,
        term.y + (term.height - height) / 2,
        width,
        height,
    )
}

/// Vertical bands of the dialog body: the muted intro line on top and the rung
/// list below it.
struct SelectionBody {
    /// Single row carrying the kind-specific intro sentence.
    intro: Rect,
    /// Rung rows, reached through exactly one blank spacer under the intro.
    list: Rect,
}

/// Splits the air-aware dialog body into the intro line and the rung list.
///
/// The spacer row belongs to the body renderer because only it knows where its
/// own sections begin and end (Conventions.md §6.7).
fn selection_body(body: Rect) -> SelectionBody {
    let intro_h = body.height.min(1);
    let spacer_h = body.height.saturating_sub(intro_h).min(1);
    let list_y = body.y.saturating_add(intro_h).saturating_add(spacer_h);
    let list_h = body.height.saturating_sub(intro_h).saturating_sub(spacer_h);
    SelectionBody {
        intro: Rect::new(body.x, body.y, body.width, intro_h),
        list: Rect::new(body.x, list_y, body.width, list_h),
    }
}

/// The rung-list rectangle of the picker.
///
/// Rendering and mouse hit-testing both take their rows from it, so a click on
/// the air above the first rung or below the last one never activates a model
/// (Conventions.md §6.6, §6.7).
pub fn selection_dialog_list_area(term: Rect, item_count: usize) -> Rect {
    selection_body(modal_geometry(selection_dialog_area(term, item_count)).body).list
}

/// Computes the number of visible rung rows in the picker body.
pub fn selection_dialog_visible_height(area: Rect) -> usize {
    (selection_body(modal_geometry(area).body).list.height as usize).max(1)
}

/// Returns the select and cancel button rectangles of the picker.
///
/// Both derive from the shared action-bar band, so the buttons that are drawn
/// are always the buttons that are clickable (Conventions.md §6.6).
pub fn selection_dialog_buttons(term: Rect, item_count: usize) -> (Rect, Rect) {
    let action_bar = modal_geometry(selection_dialog_area(term, item_count)).action_bar;
    selection_buttons_in(action_bar)
}

/// Centers the drawn localized button labels inside an action-bar row.
fn selection_buttons_in(action_bar: Rect) -> (Rect, Rect) {
    let select = select_button_label();
    let cancel = cancel_button_label();
    let select_w = Span::raw(select.as_str()).width() as u16;
    let cancel_w = Span::raw(cancel.as_str()).width() as u16;
    let gap_w = Span::raw(BUTTON_GAP).width() as u16;
    let total = select_w + gap_w + cancel_w;
    let start_x = action_bar.x + action_bar.width.saturating_sub(total) / 2;

    (
        Rect::new(start_x, action_bar.y, select_w, 1),
        Rect::new(start_x + select_w + gap_w, action_bar.y, cancel_w, 1),
    )
}

pub fn render_selection_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::SelectionDialogState,
) {
    let area = selection_dialog_area(frame.area(), dialog.selector.len());

    // The picker presents strictly the configured primary tiers, so its chrome
    // has a single presentation instead of a per-source branch (c-2).
    let title = active_t("dialog.model_select.title_primary");

    let engine = current_engine();
    let palette = engine.palette();

    render_modal_container_styled(frame, area, &title, engine.fg(palette.status_success));

    // The shared frame owns the vertical formula (top inset, pre-action spacer,
    // docked hint strip) and the horizontal padding, so the picker breathes
    // exactly like every other card and its geometry stays the single source of
    // truth for the mouse hit-testing (Conventions.md §6.6–§6.8).
    let geometry = modal_geometry(area);
    let body = selection_body(geometry.body);

    let intro = active_t("dialog.model_select.intro_primary");
    frame.render_widget(
        Paragraph::new(Span::styled(intro, engine.fg(palette.text_muted))),
        body.intro,
    );

    // The rows come straight from the shared model selector, so the picker draws
    // exactly what the four role dialogs draw and its scroll window follows the
    // highlighted rung without a second copy of the arithmetic (c-1).
    frame.render_widget(
        Paragraph::new(dialog.selector.render_list_lines(body.list.height as usize)),
        body.list,
    );

    let (select_btn, cancel_btn) = selection_buttons_in(geometry.action_bar);
    frame.render_widget(
        Paragraph::new(Span::styled(
            select_button_label(),
            engine
                .bg(palette.button_active_bg)
                .fg(engine.color(palette.text_inverse))
                .bold(),
        )),
        select_btn,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            cancel_button_label(),
            engine
                .bg(palette.button_bg)
                .fg(engine.color(palette.button_fg)),
        )),
        cancel_btn,
    );

    // The navigation hint is docked to the card's bottom edge as its own strip
    // filled with `palette.overlay`, so it reads as persistent chrome instead of
    // a second line of buttons.
    render_dialog_footer(
        frame,
        geometry.hint_row,
        &verb_hint(active_t("dialog.model_select.footer_primary")),
    );
}

#[cfg(test)]
mod tests {
    use tauqe_protocol::{ModelEntry, ModelRef, ModelRole};

    use crate::app::SelectionDialogState;
    use crate::components::model_selector::ModelSelector;

    use super::*;

    /// Every row the pointer can reach inside the list area resolves through the
    /// selector's own window to a configured rung, and the intro line plus its
    /// spacer above the list stay unclickable (c-1, Conventions §6.6).
    #[test]
    fn every_clickable_row_maps_to_a_configured_rung() {
        let term = Rect::new(0, 0, 100, 30);
        let count = 12;
        let entries: Vec<ModelEntry> = (0..count)
            .map(|i| ModelEntry::new(ModelRef::openrouter(format!("model-{i}")), None))
            .collect();
        let mut selector = ModelSelector::new(ModelRole::Primary, entries, 0);

        let area = selection_dialog_area(term, count);
        let list_area = selection_dialog_list_area(term, count);
        let visible = selection_dialog_visible_height(area);

        assert_eq!(list_area.height as usize, visible);

        // A highlight deep in the list keeps its row inside the window, and every
        // drawn row still resolves to a real entry.
        selector.select_index(count - 1);
        let start = selector.visible_start(visible);
        for row in 0..list_area.height as usize {
            assert!(start + row < count);
        }

        // The intro line and the inter-section spacer are not rung rows.
        let body = selection_body(modal_geometry(area).body);
        assert!(list_area.y >= body.intro.y + 2);
    }

    /// The number of rows the selector draws for the list area equals the number
    /// of rows the pointer can hit, so no drawn rung is unclickable and no empty
    /// row is clickable (c-1, Conventions §6.6).
    #[test]
    fn drawn_rungs_fill_exactly_the_clickable_list_area() {
        let term = Rect::new(0, 0, 100, 30);
        let entries: Vec<ModelEntry> = (0..4)
            .map(|i| ModelEntry::new(ModelRef::openrouter(format!("model-{i}")), None))
            .collect();
        let dialog = SelectionDialogState::new(entries, 1);
        let list_area = selection_dialog_list_area(term, 4);

        let lines = dialog.selector.render_list_lines(list_area.height as usize);
        assert_eq!(lines.len(), list_area.height as usize);
    }

    #[test]
    fn dialog_area_keeps_air_between_its_sections() {
        let term = Rect::new(0, 0, 100, 30);
        let area = selection_dialog_area(term, 3);
        let geometry = modal_geometry(area);
        let body = selection_body(geometry.body);

        assert_eq!(body.intro.y, area.y + 2, "border + top inset");
        assert_eq!(body.list.y, body.intro.y + 2, "inter-section spacer");
        assert!(body.list.x > area.x + 1, "horizontal padding keeps air");
        assert_eq!(
            geometry.action_bar.y,
            geometry.body.y + geometry.body.height + 1,
            "pre-action spacer"
        );
        assert!(body.list.height >= 3, "three rungs fit without clipping");
    }

    #[test]
    fn list_area_matches_visible_height() {
        let term = Rect::new(0, 0, 100, 30);
        let area = selection_dialog_area(term, 3);

        assert_eq!(
            selection_dialog_list_area(term, 3).height as usize,
            selection_dialog_visible_height(area)
        );
    }

    #[test]
    fn buttons_stay_inside_the_action_bar() {
        let term = Rect::new(0, 0, 100, 30);
        let item_count = 3;
        let action_bar = modal_geometry(selection_dialog_area(term, item_count)).action_bar;
        let (select_btn, cancel_btn) = selection_dialog_buttons(term, item_count);

        assert_eq!(select_btn.y, action_bar.y);
        assert_eq!(cancel_btn.y, action_bar.y);
        assert!(select_btn.x >= action_bar.x);
        assert!(cancel_btn.x + cancel_btn.width <= action_bar.x + action_bar.width);
    }
}
