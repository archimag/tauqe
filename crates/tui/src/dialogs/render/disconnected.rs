//! Server-disconnection modal rendering.
//!
//! The card reuses the shared modal container and docked-footer components, so
//! its chrome matches every other dialog, and exposes
//! [`disconnected_button_geometry`] so mouse hit-testing lands on the drawn
//! quit button (Conventions.md §6, §8).

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::components::modal::{render_dialog_footer, render_modal_container, ModalTone};
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

/// Computes the geometry and quit-button bounds of the disconnection modal.
///
/// The card is split into a flexible body, a two-row action zone, and a one-row
/// docked hint strip, mirroring the shared modal frame so the button row cannot
/// drift from the rendered layout.
pub fn disconnected_button_geometry(term: Rect) -> DisconnectedModalGeometry {
    let area = centered_rect(64, 30, term);
    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(2),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner);
    let action_bar = chunks[1];

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
    let inner_area = render_modal_container(frame, geom.area, &title, ModalTone::Dangerous);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(2),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner_area);

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
    frame.render_widget(body, chunks[0]);

    let quit_line = Line::from(Span::styled(
        disconnected_quit_label(),
        engine
            .bg(palette.status_error)
            .fg(engine.color(palette.text_inverse))
            .bold(),
    ));
    let actions = Paragraph::new(quit_line).alignment(Alignment::Center);
    frame.render_widget(actions, chunks[1]);

    render_dialog_footer(frame, chunks[2], &active_t("dialog.server_disconnected.footer"));
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
        let buffer = terminal.backend().buffer();
        let text: String = (geom.quit_button.x..geom.quit_button.x + geom.quit_button.width)
            .map(|x| buffer[(x, geom.quit_button.y)].symbol())
            .collect();
        assert!(text.contains("Quit"), "quit button text: {text:?}");
    }
}
