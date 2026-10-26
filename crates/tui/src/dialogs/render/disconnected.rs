//! Server-disconnection modal rendering.
//!
//! The card reuses the shared modal container and docked-footer components, so
//! its chrome matches every other dialog, and exposes
//! [`disconnected_button_geometry`] so mouse hit-testing lands on the drawn
//! quit button (Conventions.md §6, §8).

use ratatui::layout::{Alignment, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::components::modal::{
    modal_geometry, render_dialog_footer, render_modal_container, verb_hint, ModalTone,
};
use crate::i18n::active_t;
use crate::theme::engine::current_engine;
use crate::ui::centered_rect;

/// Exact geometry and quit-button bounds of the disconnection modal.
///
/// Rendering and mouse hit-testing derive both from a single layout, so the
/// drawn button is always the clickable one (Conventions §6, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisconnectedModalGeometry {
    pub area: Rect,
    pub quit_button: Rect,
}

/// Body rows the disconnection card always renders: a leading blank, the
/// termination notice, a blank, and the server message.
const DISCONNECTED_BODY_LINES: u16 = 4;

/// Height percentage that lets the shared frame show the whole card.
///
/// The frame reserves six chrome rows — two borders, the top inset, the
/// pre-action spacer, the action row, and the docked hint strip — so the card is
/// sized from its body instead of a fixed percentage that would clip the server
/// message (Conventions §6.7).
fn disconnected_height_percent(term: Rect) -> u16 {
    let needed_height = DISCONNECTED_BODY_LINES.saturating_add(8).max(10);
    (needed_height * 100 / term.height.max(1)).clamp(25, 90)
}

/// Computes the geometry and quit-button bounds of the disconnection modal.
///
/// The card's bands come from the shared [`modal_geometry`] formula — top inset,
/// padded body, pre-action spacer, action row, and docked hint strip — so the
/// button row cannot drift from the rendered layout.
pub fn disconnected_button_geometry(term: Rect) -> DisconnectedModalGeometry {
    let area = centered_rect(64, disconnected_height_percent(term), term);

    // The shared modal formula owns the padded action row, so the quit button is
    // placed on the row the renderer draws it in and hit-testing can never drift
    // from the rendered layout (Conventions §6.6–§6.8).
    let action_bar = modal_geometry(area).action_bar;

    let quit_len = UnicodeWidthStr::width(disconnected_quit_label().as_str()) as u16;
    let start_x = action_bar.x + action_bar.width.saturating_sub(quit_len) / 2;

    DisconnectedModalGeometry {
        area,
        quit_button: Rect::new(start_x, action_bar.y, quit_len, 1),
    }
}

/// Label of the quit button, resolved from the locale so the hit-test geometry
/// and the drawn button stay in lockstep (Conventions §6, §9).
fn disconnected_quit_label() -> String {
    format!(" [ {} (q / Ctrl+Q) ] ", active_t("dialog.button.quit"))
}

/// Renders the disconnection modal with a docked exit panel.
pub fn render_disconnected_popup(frame: &mut ratatui::Frame, message: &str) {
    let engine = current_engine();
    let palette = engine.palette();
    let geom = disconnected_button_geometry(frame.area());

    let title = active_t("dialog.server_disconnected.title");
    render_modal_container(frame, geom.area, &title, ModalTone::Dangerous);

    // The container is drawn from the shared frame, so the body, the action row,
    // and the docked hint strip are read from the same air-aware formula the
    // quit-button hit-testing uses (Conventions §6.6–§6.8).
    let geometry = modal_geometry(geom.area);

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.server_disconnected.terminated"),
            engine.fg(palette.status_error).bold(),
        )),
        Line::raw(""),
        Line::from(Span::styled(message, engine.fg(palette.text_primary))),
    ];
    let body = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false });
    frame.render_widget(body, geometry.body);

    let quit_line = Line::from(Span::styled(
        disconnected_quit_label(),
        engine
            .bg(palette.status_error)
            .fg(engine.color(palette.text_inverse))
            .bold(),
    ));
    let actions = Paragraph::new(quit_line).alignment(Alignment::Center);
    frame.render_widget(actions, geometry.action_bar);

    render_dialog_footer(
        frame,
        geometry.hint_row,
        &verb_hint(active_t("dialog.server_disconnected.footer")),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The hit-test geometry must land exactly on the drawn quit button.
    #[test]
    fn quit_button_matches_hit_geometry() {
        let term = Rect::new(0, 0, 100, 30);
        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|f| render_disconnected_popup(f, "Server exited unexpectedly."))
            .unwrap();

        let geom = disconnected_button_geometry(term);
        let geometry = modal_geometry(geom.area);

        assert_eq!(geom.quit_button.y, geometry.action_bar.y);
        assert!(geom.quit_button.x >= geometry.action_bar.x);
        assert!(
            geom.quit_button.x + geom.quit_button.width
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
        let text: String = (geom.quit_button.x..geom.quit_button.x + geom.quit_button.width)
            .map(|x| buffer[(x, geom.quit_button.y)].symbol())
            .collect();
        assert!(text.contains("Quit"), "quit button text: {text:?}");
    }
}
