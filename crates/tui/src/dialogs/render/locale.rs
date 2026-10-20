//! Interface-language (locale) dialog rendering.
//!
//! The dialog lets the developer request LLM-synthesized translations for an
//! arbitrary target language or reset the interface to the built-in English
//! (c-2, c-4). Rendering and mouse hit-testing share the geometry helpers below,
//! so the drawn action buttons are always the clickable ones (Conventions §6).
//!
//! Every visible phrase is resolved through the runtime locale
//! (`crate::i18n::active_t`), so a hot-swapped locale reaches the dialog without
//! touching the renderer (Conventions §9, c-1).

use ratatui::layout::{Alignment, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{AppState, LocaleDialogButton, LocaleDialogState};
use crate::components::modal::{
    modal_geometry, render_dialog_footer, render_modal_container, ModalTone,
};
use crate::i18n::active_t;
use crate::theme::engine::current_engine;

/// Action-button labels in left-to-right order, resolved from the runtime locale.
///
/// Geometry and rendering both derive their layout from this single list, so the
/// clickable rectangles can never drift from the drawn buttons.
fn locale_button_labels() -> [String; 3] {
    [
        format!(" {} ", active_t("modal.synthesize")),
        format!(" {} ", active_t("modal.reset_english")),
        format!(" {} ", active_t("modal.cancel")),
    ]
}

const LOCALE_DIALOG_WIDTH: u16 = 66;
const LOCALE_BUTTON_GAP: u16 = 2;
/// Fixed body rows the locale card always renders: a leading blank, the active
/// language line, a blank, the intro line, a blank, and the language field.
const LOCALE_BODY_LINES: u16 = 6;

/// Height percentage that lets the shared frame show the whole body.
///
/// The frame reserves six chrome rows — two borders, the top inset, the
/// pre-action spacer, the action row, and the docked hint strip — so the card is
/// sized from its body instead of a fixed percentage that would clip the
/// language field on a short terminal (Conventions §6.7).
fn locale_dialog_height_percent(term: Rect) -> u16 {
    let needed_height = LOCALE_BODY_LINES.saturating_add(8).max(10);
    (needed_height * 100 / term.height.max(1)).clamp(25, 90)
}

/// Exact geometry and action-button bounds of the locale dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocaleDialogGeometry {
    pub area: Rect,
    pub synthesize_button: Rect,
    pub reset_button: Rect,
    pub cancel_button: Rect,
}

/// Computes the geometry and button bounds of the locale dialog so rendering and
/// mouse hit-testing agree on the same layout.
pub fn locale_dialog_geometry(term: Rect) -> LocaleDialogGeometry {
    let area = crate::ui::centered_rect(
        LOCALE_DIALOG_WIDTH,
        locale_dialog_height_percent(term),
        term,
    );

    // The shared modal formula owns the top inset, the horizontal margins, the
    // pre-action spacer, and the docked hint row, so the buttons are placed on
    // the padded action bar the renderer draws into and hit-testing can never
    // drift from the rendered layout (Conventions §6.7, §6.8).
    let action_bar = modal_geometry(area).action_bar;

    // The labels are measured in characters, matching the localized chips the
    // renderer draws, so hit-testing never desynchronizes from the drawing.
    let labels = locale_button_labels();
    let widths: [u16; 3] = [
        UnicodeWidthStr::width(labels[0].as_str()) as u16,
        UnicodeWidthStr::width(labels[1].as_str()) as u16,
        UnicodeWidthStr::width(labels[2].as_str()) as u16,
    ];
    let total = widths[0] + widths[1] + widths[2] + LOCALE_BUTTON_GAP * 2;
    let start = action_bar.x + action_bar.width.saturating_sub(total) / 2;
    let y = action_bar.y;

    let synthesize_button = Rect::new(start, y, widths[0], 1);
    let reset_x = start + widths[0] + LOCALE_BUTTON_GAP;
    let reset_button = Rect::new(reset_x, y, widths[1], 1);
    let cancel_x = reset_x + widths[1] + LOCALE_BUTTON_GAP;
    let cancel_button = Rect::new(cancel_x, y, widths[2], 1);

    LocaleDialogGeometry {
        area,
        synthesize_button,
        reset_button,
        cancel_button,
    }
}

/// Renders the locale dialog using the active semantic palette.
pub fn render_locale_dialog(frame: &mut ratatui::Frame, state: &AppState) {
    let Some(dialog) = state.locale_dialog.as_ref() else {
        return;
    };
    let geom = locale_dialog_geometry(frame.area());

    let title = active_t("dialog.locale.title");
    render_modal_container(frame, geom.area, &title, ModalTone::Accent);

    // The container is drawn from the shared frame, so the body, the action bar,
    // and the docked hint strip are read from the same air-aware formula the
    // button hit-testing uses (Conventions §6.6–§6.8).
    let geometry = modal_geometry(geom.area);

    render_locale_body(frame, geometry.body, dialog, state);
    render_locale_actions(frame, geometry.action_bar, dialog);
    render_dialog_footer(frame, geometry.hint_row, &locale_footer_hint(dialog));
}

/// Renders the body: the active language and the target-language field.
fn render_locale_body(
    frame: &mut ratatui::Frame,
    area: Rect,
    dialog: &LocaleDialogState,
    state: &AppState,
) {
    let engine = current_engine();
    let palette = engine.palette();
    let is_editor_focused = dialog.focus.is_editor();

    let active = match state.i18n.language() {
        Some(language) => active_t("dialog.locale.active_custom").replacen("{}", language, 1),
        None => active_t("dialog.locale.active_english"),
    };

    let mut lines: Vec<Line<'static>> = vec![
        Line::raw(""),
        Line::from(Span::styled(active, engine.fg(palette.status_info).bold())),
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.locale.intro"),
            engine.fg(palette.text_secondary),
        )),
        Line::raw(""),
    ];

    if dialog.generating {
        lines.push(Line::from(Span::styled(
            active_t("dialog.locale.generating").replacen(
                "{}",
                dialog.language_editor.get_text().trim(),
                1,
            ),
            engine.fg(palette.status_warning).bold(),
        )));
    } else if dialog.language_editor.is_empty() {
        let cursor = if is_editor_focused { "█" } else { " " };
        lines.push(Line::from(vec![
            Span::styled(cursor, engine.fg(palette.status_warning)),
            Span::styled(
                format!(" {}", active_t("dialog.locale.target_placeholder")),
                engine.fg(palette.text_muted),
            ),
        ]));
    } else {
        lines.extend(super::editor_lines(
            &dialog.language_editor,
            is_editor_focused,
        ));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Renders the centered action bar, emphasizing the focused button.
fn render_locale_actions(frame: &mut ratatui::Frame, area: Rect, dialog: &LocaleDialogState) {
    let engine = current_engine();
    let palette = engine.palette();

    let style_for = |button: LocaleDialogButton| {
        if dialog.focus.is_actions() && dialog.button == button {
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

    let labels = locale_button_labels();
    let synthesize_label = if dialog.generating {
        format!(" {} ", active_t("dialog.locale.synthesizing"))
    } else {
        labels[0].clone()
    };
    let actions = Line::from(vec![
        Span::styled(synthesize_label, style_for(LocaleDialogButton::Synthesize)),
        Span::raw("  "),
        Span::styled(
            labels[1].clone(),
            style_for(LocaleDialogButton::ResetEnglish),
        ),
        Span::raw("  "),
        Span::styled(labels[2].clone(), style_for(LocaleDialogButton::Cancel)),
    ]);
    frame.render_widget(Paragraph::new(actions).alignment(Alignment::Center), area);
}

/// Footer hint for the focused section, projected through the semantic span renderer.
fn locale_footer_hint(dialog: &LocaleDialogState) -> String {
    if dialog.generating {
        active_t("dialog.locale.footer_generating")
    } else if dialog.focus.is_editor() {
        active_t("dialog.locale.footer_editor")
    } else {
        active_t("dialog.locale.footer_actions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The geometry used for mouse hit-testing must land exactly on the drawn
    /// action buttons.
    #[test]
    fn locale_action_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 30);
        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        let mut state = crate::app::test_state();
        state.locale_dialog = Some(LocaleDialogState::default());

        terminal.draw(|f| render_locale_dialog(f, &state)).unwrap();

        let geom = locale_dialog_geometry(term);
        let geometry = modal_geometry(geom.area);

        assert_eq!(geom.synthesize_button.y, geometry.action_bar.y);
        assert_eq!(geom.reset_button.y, geometry.action_bar.y);
        assert_eq!(geom.cancel_button.y, geometry.action_bar.y);
        assert!(geom.synthesize_button.x >= geometry.action_bar.x);
        assert!(
            geom.cancel_button.x + geom.cancel_button.width
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

        assert!(read(geom.synthesize_button).contains("Synthesize"));
        assert!(read(geom.reset_button).contains("Reset"));
        assert!(read(geom.cancel_button).contains("Cancel"));
    }
}
