use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{
    compact_model_name, numbered_entry_label, SelectionDialogKind, SelectionItem,
};

/// Select button label as drawn by [`render_selection_dialog`], including its
/// surrounding `[ ]` decoration.
const SELECT_BUTTON_LABEL: &str = " [ Select (Enter) ] ";
/// Cancel button label decoration.
const CANCEL_BUTTON_LABEL: &str = " [ Cancel (Esc) ] ";
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
        SelectionItem::Catalog => {
            format!("[{}] Others... (full model catalog)", index + 1)
        }
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
            Constraint::Length(3),
        ])
        .split(inner);
    let bar = chunks[2];

    let select_w = SELECT_BUTTON_LABEL.chars().count() as u16;
    let cancel_w = CANCEL_BUTTON_LABEL.chars().count() as u16;
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
    frame.render_widget(Clear, area);

    let title = match dialog.kind {
        SelectionDialogKind::PrimaryModels => " Select Primary Model ".to_string(),
        SelectionDialogKind::ModelCatalog => format!(
            " Select Model ({}/{}) ",
            dialog.selector.selected_index() + 1,
            dialog.items.len()
        ),
    };

    let outer_block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(2),
            Constraint::Length(3),
        ])
        .split(inner_area);

    let intro = match dialog.kind {
        SelectionDialogKind::PrimaryModels => " Models configured under [models.primary] in tauqe.toml",
        SelectionDialogKind::ModelCatalog => " All available models",
    };
    frame.render_widget(
        Paragraph::new(Span::styled(intro, Style::default().fg(Color::DarkGray))),
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
            Style::default().fg(Color::Green).bold()
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let line_style = if is_selected {
            Style::default().bg(Color::DarkGray).fg(Color::White).bold()
        } else {
            Style::default().fg(Color::Gray)
        };

        lines.push(Line::from(vec![
            Span::styled(marker, marker_style),
            Span::styled(selection_item_label(item, idx), line_style),
        ]));
    }

    let list_widget = Paragraph::new(lines);
    frame.render_widget(list_widget, chunks[1]);

    let actions = vec![
        Line::from(vec![
            Span::styled(SELECT_BUTTON_LABEL, Style::default().bg(Color::Green).fg(Color::Black).bold()),
            Span::raw(BUTTON_GAP),
            Span::styled(CANCEL_BUTTON_LABEL, Style::default().bg(Color::DarkGray).fg(Color::White)),
        ]),
        Line::from(Span::styled(
            match dialog.kind {
                SelectionDialogKind::PrimaryModels => "1-9 quick select • n/p navigate • Enter select • Esc close",
                SelectionDialogKind::ModelCatalog => "n/p navigate • Enter select • Esc/q dismiss",
            },
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let actions_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(actions_widget, chunks[2]);
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
