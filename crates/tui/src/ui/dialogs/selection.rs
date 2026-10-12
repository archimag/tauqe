use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{role_badge, role_title, SelectionDialogKind, SelectionItem};

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

/// Row text for the model picker. Role rows carry their quick-select number and role badge.
fn selection_item_label(item: &SelectionItem, index: usize) -> String {
    match item {
        SelectionItem::Role(role, model) => {
            format!("[{}] {} {:<9} {}", index + 1, role_badge(*role), role_title(*role), model)
        }
        SelectionItem::Model(model) => model.to_string(),
        SelectionItem::Others => format!("[{}] Others...  browse all models", index + 1),
    }
}

/// Computes the number of visible item rows in the selection dialog body.
pub fn selection_dialog_visible_height(area: Rect) -> usize {
    (area.height.saturating_sub(6) as usize).max(1)
}

pub fn render_selection_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::SelectionDialogState,
) {
    let area = selection_dialog_area(frame.area(), dialog.items.len());
    frame.render_widget(Clear, area);

    let title = match dialog.kind {
        SelectionDialogKind::ModelRoles => " Select Model Role ".to_string(),
        SelectionDialogKind::ModelCatalog => format!(
            " Select Model ({}/{}) ",
            dialog.selected_index + 1,
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
        SelectionDialogKind::ModelRoles => " Semantic roles map to [models] in tauqe.toml",
        SelectionDialogKind::ModelCatalog => " All available models",
    };
    frame.render_widget(
        Paragraph::new(Span::styled(intro, Style::default().fg(Color::DarkGray))),
        chunks[0],
    );

    let inner_height = chunks[1].height as usize;
    let max_scroll = dialog.items.len().saturating_sub(inner_height);
    let scroll_offset = dialog.scroll_offset.min(max_scroll);

    let mut lines = Vec::new();
    let visible_items = dialog
        .items
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(inner_height);

    for (idx, item) in visible_items {
        let is_selected = idx == dialog.selected_index;
        let prefix = if is_selected { " ▶ " } else { "   " };
        let line_style = if is_selected {
            Style::default().bg(Color::DarkGray).fg(Color::White).bold()
        } else {
            Style::default().fg(Color::Gray)
        };

        lines.push(
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::Green).bold()),
                Span::styled(selection_item_label(item, idx), line_style),
            ])
            .style(line_style),
        );
    }

    let list_widget = Paragraph::new(lines);
    frame.render_widget(list_widget, chunks[1]);

    let actions = vec![
        Line::from(vec![
            Span::styled(" [ Select (Enter) ] ", Style::default().bg(Color::Green).fg(Color::Black).bold()),
            Span::raw("    "),
            Span::styled(" [ Cancel (Esc) ] ", Style::default().bg(Color::DarkGray).fg(Color::White)),
        ]),
        Line::from(Span::styled(
            match dialog.kind {
                SelectionDialogKind::ModelRoles => "1-5 quick select roles • 6 Others • n/p navigate • Esc close",
                SelectionDialogKind::ModelCatalog => "n/p navigate • Enter select • Esc/q dismiss",
            },
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let actions_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(actions_widget, chunks[2]);
}
