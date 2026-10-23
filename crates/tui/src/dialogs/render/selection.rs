use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::app::{compact_model_name, numbered_entry_label, SelectionDialogKind, SelectionItem};
use crate::components::modal::{
    modal_geometry, render_dialog_footer, render_modal_container_styled,
};
use crate::i18n::active_t;
use crate::theme::engine::current_engine;

/// Full drawn select-button label, resolved from the runtime locale.
///
/// Rendering and mouse hit-testing both measure this label, so the clickable
/// button can never drift from the drawn one (Conventions §6, §9, c-1).
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

/// Single source of truth for the model selection dialog geometry, shared by
/// rendering and mouse hit-testing.
///
/// The height budget is the chrome the shared frame owns plus one row per item,
/// so the item list is never clipped by the air around it
/// (Conventions §6.7, §6.8, c-1).
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

/// Vertical bands of the dialog body: the muted intro line on top and the item
/// list below it.
struct SelectionBody {
    /// Single row carrying the kind-specific intro sentence.
    intro: Rect,
    /// Item rows, reached through exactly one blank spacer under the intro.
    list: Rect,
}

/// Splits the air-aware dialog body into the intro line and the item list.
///
/// The spacer row belongs to the body renderer because only it knows where its
/// own sections begin and end (Conventions §6.7, c-1).
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

/// Row text for the model picker.
///
/// Primary rows reuse [`numbered_entry_label`], the single numbering and
/// presentation rule shared with the embedded model list, so their `[N]`
/// number, alias, provider, and reasoning effort cannot drift from the Review
/// and Refine dialogs. Catalog rows stay plain model names, matching the shared
/// model presentation without a number.
fn selection_item_label(item: &SelectionItem, index: usize) -> String {
    match item {
        SelectionItem::Primary(entry) => numbered_entry_label(index, entry),
        SelectionItem::Model(model) => compact_model_name(model).to_string(),
        SelectionItem::Catalog => {
            active_t("dialog.model_select.catalog_row").replacen("{}", &(index + 1).to_string(), 1)
        }
    }
}

/// The item-list rectangle of the selection dialog.
///
/// Rendering and mouse hit-testing both take their rows from it, so a click on
/// the air above the first row or below the last one never activates an item
/// (Conventions §6.6, §6.7, c-3).
pub fn selection_dialog_list_area(term: Rect, item_count: usize) -> Rect {
    selection_body(modal_geometry(selection_dialog_area(term, item_count)).body).list
}

/// Computes the number of visible item rows in the selection dialog body.
pub fn selection_dialog_visible_height(area: Rect) -> usize {
    (selection_body(modal_geometry(area).body).list.height as usize).max(1)
}

/// Returns the select and cancel button rectangles of the selection dialog.
///
/// Both derive from the shared action-bar band, so the buttons that are drawn
/// are always the buttons that are clickable (Conventions §6.6, c-3).
pub fn selection_dialog_buttons(term: Rect, item_count: usize) -> (Rect, Rect) {
    let action_bar = modal_geometry(selection_dialog_area(term, item_count)).action_bar;
    selection_buttons_in(action_bar)
}

/// Centers the drawn localized button labels inside an action-bar row.
fn selection_buttons_in(action_bar: Rect) -> (Rect, Rect) {
    let select_w = UnicodeWidthStr::width(select_button_label().as_str()) as u16;
    let cancel_w = UnicodeWidthStr::width(cancel_button_label().as_str()) as u16;
    let gap_w = UnicodeWidthStr::width(BUTTON_GAP) as u16;
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
    let area = selection_dialog_area(frame.area(), dialog.items.len());

    let title = match dialog.kind {
        SelectionDialogKind::PrimaryModels => active_t("dialog.model_select.title_primary"),
        SelectionDialogKind::ModelCatalog => active_t("dialog.model_select.title_catalog")
            .replacen("{}", &(dialog.selector.selected_index() + 1).to_string(), 1)
            .replacen("{}", &dialog.items.len().to_string(), 1),
    };

    let engine = current_engine();
    let palette = engine.palette();

    render_modal_container_styled(frame, area, &title, engine.fg(palette.status_success));

    // The shared frame owns the vertical formula (top inset, pre-action spacer,
    // docked hint strip) and the horizontal padding, so the picker breathes
    // exactly like every other card and its geometry stays the single source of
    // truth for the mouse hit-testing (Conventions §6.6–§6.8, c-1, c-2, c-3).
    let geometry = modal_geometry(area);
    let body = selection_body(geometry.body);

    let intro = match dialog.kind {
        SelectionDialogKind::PrimaryModels => active_t("dialog.model_select.intro_primary"),
        SelectionDialogKind::ModelCatalog => active_t("dialog.model_select.intro_catalog"),
    };
    frame.render_widget(
        Paragraph::new(Span::styled(intro, engine.fg(palette.text_muted))),
        body.intro,
    );

    let inner_height = body.list.height as usize;
    let scroll_offset = dialog.selector.scroll_offset();

    let mut lines = Vec::new();
    let visible_items = dialog
        .items
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(inner_height);

    for (idx, item) in visible_items {
        let is_selected = idx == dialog.selector.selected_index();
        // The marker and row styles mirror the embedded model list row, so the
        // `▶` selection indicator reads identically across all three dialogs.
        // The marker carries its own leading space, so no row presses against
        // the card border (Conventions §6.8, c-2).
        let marker = if is_selected { " ▶ " } else { "   " };
        let marker_style = if is_selected {
            engine.fg(palette.status_success).bold()
        } else {
            engine.fg(palette.text_muted)
        };
        let line_style = if is_selected {
            engine
                .bg(palette.overlay)
                .fg(engine.color(palette.text_primary))
                .bold()
        } else {
            engine.fg(palette.text_secondary)
        };

        lines.push(Line::from(vec![
            Span::styled(marker, marker_style),
            Span::styled(selection_item_label(item, idx), line_style),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), body.list);

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
    // filled with `palette.overlay`, so it reads as persistent chrome instead
    // of a second line of buttons (c-1, c-2).
    let footer_hint = match dialog.kind {
        SelectionDialogKind::PrimaryModels => active_t("dialog.model_select.footer_primary"),
        SelectionDialogKind::ModelCatalog => active_t("dialog.model_select.footer_catalog"),
    };
    render_dialog_footer(frame, geometry.hint_row, &footer_hint);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn catalog_dialog(count: usize) -> crate::app::SelectionDialogState {
        let items = (0..count)
            .map(|index| {
                SelectionItem::Model(tauqe_protocol::ModelRef::openrouter(format!(
                    "model-{index}"
                )))
            })
            .collect();
        crate::app::SelectionDialogState::new(SelectionDialogKind::ModelCatalog, items, 0)
    }

    /// Reads the interior columns of a card row, skipping the two vertical
    /// border glyphs, so a blank interior row reads as empty text instead of a
    /// pair of `│` frame characters (Conventions §6.8).
    fn interior_row_text(terminal: &Terminal<TestBackend>, area: Rect, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        let start = area.x.saturating_add(1);
        let end = area.x.saturating_add(area.width).saturating_sub(1);
        (start..end).map(|x| buffer[(x, y)].symbol()).collect()
    }

    /// Mouse hit-testing must compute the same button rectangles the renderer
    /// draws for the selection dialog action bar.
    #[test]
    fn selection_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 30);
        let dialog = catalog_dialog(3);

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|f| render_selection_dialog(f, &dialog))
            .unwrap();

        let (select_btn, cancel_btn) = selection_dialog_buttons(term, dialog.items.len());
        let buffer = terminal.backend().buffer();

        let select_text: String = (select_btn.x..select_btn.x + select_btn.width)
            .map(|x| buffer[(x, select_btn.y)].symbol())
            .collect();
        assert!(
            select_text.contains("Select"),
            "select row text: {select_text:?}"
        );

        let cancel_text: String = (cancel_btn.x..cancel_btn.x + cancel_btn.width)
            .map(|x| buffer[(x, cancel_btn.y)].symbol())
            .collect();
        assert!(
            cancel_text.contains("Cancel"),
            "cancel row text: {cancel_text:?}"
        );
    }

    /// Primary rows must render through the shared `numbered_entry_label`, so
    /// the Review, Refine, and Primary pickers share one numbering and one
    /// presentation rule for aliases, provider, and reasoning effort.
    #[test]
    fn primary_rows_reuse_shared_numbered_label() {
        let entry = tauqe_protocol::ModelEntry::new(
            tauqe_protocol::ModelRef::openrouter("anthropic/claude-3-7-sonnet"),
            Some(tauqe_protocol::ReasoningEffort::High),
            Some("Claude Auditor".to_string()),
        );

        let label = selection_item_label(&SelectionItem::Primary(entry.clone()), 0);

        assert_eq!(label, crate::app::numbered_entry_label(0, &entry));
        assert!(label.starts_with("[1] "), "label: {label:?}");
        assert!(label.contains("Claude Auditor"));
        assert!(label.contains("[high]"));
    }

    /// The card keeps one blank row under the border title, one blank row
    /// between the intro and the list, one blank row above the buttons, and one
    /// column of air on each side (Conventions §6.7, §6.8).
    #[test]
    fn selection_dialog_keeps_air_between_its_sections() {
        let term = Rect::new(0, 0, 100, 30);
        let area = selection_dialog_area(term, 3);
        let geometry = modal_geometry(area);
        let body = selection_body(geometry.body);

        assert_eq!(body.intro.y, area.y + 2, "border + top inset");
        assert_eq!(body.list.y, body.intro.y + 2, "inter-section spacer");
        assert_eq!(body.list.x, area.x + 2, "border + horizontal padding");
        assert_eq!(
            geometry.action_bar.y,
            geometry.body.y + geometry.body.height + 1,
            "pre-action spacer"
        );
        assert!(
            body.list.height >= 3,
            "three items must fit without clipping: {}",
            body.list.height
        );
    }

    /// The hit-tested list rectangle covers exactly the rows the picker may
    /// draw, so scrolling and clicking agree (Conventions §6.6, c-3).
    #[test]
    fn list_area_matches_visible_height() {
        let term = Rect::new(0, 0, 100, 30);
        let area = selection_dialog_area(term, 3);

        assert_eq!(
            selection_dialog_list_area(term, 3).height as usize,
            selection_dialog_visible_height(area)
        );
    }

    /// The rendered intro and the first item never share a row: the spacer
    /// between them is drawn blank (Conventions §6.7).
    #[test]
    fn intro_and_first_row_are_separated_by_a_blank_line() {
        let term = Rect::new(0, 0, 100, 30);
        let dialog = catalog_dialog(3);

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|f| render_selection_dialog(f, &dialog))
            .unwrap();

        let area = selection_dialog_area(term, dialog.items.len());
        let body = selection_body(modal_geometry(area).body);

        assert!(
            !interior_row_text(&terminal, area, body.intro.y)
                .trim()
                .is_empty(),
            "the intro must be drawn under the top inset"
        );
        assert!(
            interior_row_text(&terminal, area, body.intro.y + 1)
                .trim()
                .is_empty(),
            "the inter-section spacer must stay blank"
        );
        assert!(
            !interior_row_text(&terminal, area, body.list.y)
                .trim()
                .is_empty(),
            "the first item must be drawn under the spacer"
        );
    }
}
