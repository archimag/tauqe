use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{
    compact_model_name, numbered_entry_label, SelectionDialogKind, SelectionItem,
};
use crate::components::modal::{render_dialog_footer, render_modal_container_styled};
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

/// Single source of truth for the model selection dialog geometry,
/// shared by rendering and mouse hit-testing.
pub fn selection_dialog_area(term: Rect, item_count: usize) -> Rect {
    let height = (item_count as u16 + 6).clamp(9, 21).min(term.height);
    let width = (term.width.saturating_mul(70) / 100).clamp(40, 80).min(term.width);
    Rect::new(
        term.x + (term.width - width) / 2,
        term.y + (term.height - height) / 2,
        width,
        height,
    )
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
        SelectionItem::Catalog => active_t("dialog.model_select.catalog_row")
            .replacen("{}", &(index + 1).to_string(), 1),
    }
}

/// Computes the number of visible item rows in the selection dialog body.
pub fn selection_dialog_visible_height(area: Rect) -> usize {
    (area.height.saturating_sub(6) as usize).max(1)
}

/// Returns the select and cancel button rectangles of the selection dialog,
/// reproducing the centered action bar drawn by [`render_selection_dialog`] so
/// mouse hit-testing cannot drift from the rendered buttons.
pub fn selection_dialog_buttons(term: Rect, item_count: usize) -> (Rect, Rect) {
    let area = selection_dialog_area(term, item_count);
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(2),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner);
    let bar = chunks[2];

    let select_w = select_button_label().chars().count() as u16;
    let cancel_w = cancel_button_label().chars().count() as u16;
    let gap_w = BUTTON_GAP.chars().count() as u16;
    let total = select_w + gap_w + cancel_w;
    let start_x = bar.x + bar.width.saturating_sub(total) / 2;

    (
        Rect::new(start_x, bar.y, select_w, 1),
        Rect::new(start_x + select_w + gap_w, bar.y, cancel_w, 1),
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

    let inner_area = render_modal_container_styled(
        frame,
        area,
        &title,
        engine.fg(palette.status_success),
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(2),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner_area);

    let intro = match dialog.kind {
        SelectionDialogKind::PrimaryModels => active_t("dialog.model_select.intro_primary"),
        SelectionDialogKind::ModelCatalog => active_t("dialog.model_select.intro_catalog"),
    };
    frame.render_widget(
        Paragraph::new(Span::styled(intro, engine.fg(palette.text_muted))),
        chunks[0],
    );

    let inner_height = chunks[1].height as usize;
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
        let marker = if is_selected { " ▶ " } else { "   " };
        let marker_style = if is_selected {
            engine.fg(palette.status_success).bold()
        } else {
            engine.fg(palette.text_muted)
        };
        let line_style = if is_selected {
            engine.bg(palette.overlay).fg(engine.color(palette.text_primary)).bold()
        } else {
            engine.fg(palette.text_secondary)
        };

        lines.push(Line::from(vec![
            Span::styled(marker, marker_style),
            Span::styled(selection_item_label(item, idx), line_style),
        ]));
    }

    let list_widget = Paragraph::new(lines);
    frame.render_widget(list_widget, chunks[1]);

    let actions = Line::from(vec![
        Span::styled(
            select_button_label(),
            engine.bg(palette.button_active_bg).fg(engine.color(palette.text_inverse)).bold(),
        ),
        Span::raw(BUTTON_GAP),
        Span::styled(
            cancel_button_label(),
            engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)),
        ),
    ]);
    let actions_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(actions_widget, chunks[2]);

    // The navigation hint is docked to the card's bottom edge as its own strip
    // filled with `palette.overlay`, so it reads as persistent chrome instead
    // of a second line of buttons (c-1, c-2).
    let footer_hint = match dialog.kind {
        SelectionDialogKind::PrimaryModels => active_t("dialog.model_select.footer_primary"),
        SelectionDialogKind::ModelCatalog => active_t("dialog.model_select.footer_catalog"),
    };
    render_dialog_footer(frame, chunks[3], &footer_hint);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Mouse hit-testing must compute the same button rectangles the renderer
    /// draws for the selection dialog action bar.
    #[test]
    fn selection_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 30);
        let dialog = crate::app::SelectionDialogState::new(
            SelectionDialogKind::ModelCatalog,
            vec![
                SelectionItem::Model(tauqe_protocol::ModelRef::openrouter("model-1")),
                SelectionItem::Model(tauqe_protocol::ModelRef::openrouter("model-2")),
                SelectionItem::Model(tauqe_protocol::ModelRef::openrouter("model-3")),
            ],
            0,
        );

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
}
