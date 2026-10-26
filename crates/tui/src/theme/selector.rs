//! Interactive theme preset selector.
//!
//! Surfaces the built-in calibrated presets (`#c-3`) and previews the
//! highlighted one live: moving the cursor applies the preset immediately, so a
//! scheme is judged against the real interface rather than a swatch. Confirming
//! persists the choice to `tui.toml`; cancelling restores the preset that was
//! active when the selector opened (`#c-4`).
//!
//! The dialog lives with the design-system subsystem because it is the single
//! surface where the palettes are offered to the developer.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::AppState;
use crate::components::modal::{
    modal_geometry, render_dialog_footer, render_modal_container, verb_hint, ModalTone,
};
use crate::components::ListSelector;
use crate::i18n::active_t;
use crate::theme::engine::current_engine;
use crate::theme::presets::ThemePreset;

/// State of the interactive theme preset selector.
#[derive(Debug, Clone)]
pub struct ThemeSelectorState {
    /// Scrollable cursor over [`ThemePreset::ALL`].
    pub selector: ListSelector,
    /// Preset active when the dialog opened, restored on cancellation.
    pub original: ThemePreset,
}

impl ThemeSelectorState {
    /// Creates a selector seeded on the preset currently in effect.
    pub fn new(active: ThemePreset) -> Self {
        let index = ThemePreset::ALL
            .iter()
            .position(|preset| *preset == active)
            .unwrap_or(0);
        Self {
            selector: ListSelector::new(ThemePreset::ALL.len(), index),
            original: active,
        }
    }

    /// The preset currently highlighted by the cursor.
    pub fn selected_preset(&self) -> ThemePreset {
        ThemePreset::ALL
            .get(self.selector.selected_index())
            .copied()
            .unwrap_or(self.original)
    }
}

/// Geometry of the theme selector dialog, shared by rendering and scroll math.
pub fn theme_selector_area(term: Rect) -> Rect {
    crate::ui::centered_rect(52, 60, term)
}

/// Number of preset rows visible in the selector body.
///
/// The live-preview caption occupies the first body row, so only the rows below
/// it carry presets.
pub fn theme_selector_visible_height(term: Rect) -> usize {
    selector_visible_rows(theme_selector_body_area(term))
}

/// Preset rows that fit under the live-preview caption of a body band.
fn selector_visible_rows(body: Rect) -> usize {
    (body.height.saturating_sub(1) as usize).max(1)
}

/// Body area (the scrollable preset rows) of the selector, shared by rendering
/// and mouse hit-testing.
///
/// The band is taken from the shared modal frame, so the selector inherits the
/// same top inset, horizontal padding, and pre-action spacer as every other
/// dialog instead of computing its own inner rectangle (`#c-4`,
/// Conventions §6.7, §6.8).
pub fn theme_selector_body_area(term: Rect) -> Rect {
    modal_geometry(theme_selector_area(term)).body
}

/// Resolves a left-click inside the theme selector.
///
/// A click on a preset row selects and previews it — the mouse equivalent of
/// navigating with `n`/`p`. A click outside the dialog cancels the selector and
/// restores the preset that was active when it opened, exactly like `Esc`
/// (`#c-4`). Returns `true` when the selector consumed the click.
pub fn handle_theme_selector_click(st: &mut AppState, col: u16, row: u16, term: Rect) -> bool {
    let Some(mut selector_state) = st.theme_selector.take() else {
        return false;
    };

    let area = theme_selector_area(term);
    let inside =
        col >= area.x && col < area.x + area.width && row >= area.y && row < area.y + area.height;
    if !inside {
        let original = selector_state.original;
        st.apply_theme_preset(original);
        return true;
    }

    let body = theme_selector_body_area(term);
    let inside_body =
        col >= body.x && col < body.x + body.width && row >= body.y && row < body.y + body.height;
    if inside_body {
        // The body's first row is the "Preview applies live." caption; preset
        // rows follow it. The visible window starts at the current scroll
        // offset, so the clicked visual row maps to the row actually drawn
        // (`#c-4`, Conventions §6.6).
        let body_row = (row - body.y) as usize;
        if body_row >= 1 {
            let index = selector_state.selector.scroll_offset() + (body_row - 1);
            if index < ThemePreset::ALL.len() {
                if index == selector_state.selector.selected_index() {
                    // A second click on the highlighted preset commits it,
                    // giving the mouse the same save path as `Enter`.
                    let preset = selector_state.selected_preset();
                    commit_preset(st, preset);
                    return true;
                }
                let visible = theme_selector_visible_height(term);
                selector_state.selector.select_index(index, visible);
                let preset = selector_state.selected_preset();
                st.apply_theme_preset(preset);
            }
        }
    }

    st.theme_selector = Some(selector_state);
    true
}

/// Builds one rendered preset row, marking the cursor position and the preset
/// currently painted on screen.
fn preset_row(preset: ThemePreset, is_selected: bool, is_active: bool) -> Line<'static> {
    let engine = current_engine();
    let palette = engine.palette();

    let marker = if is_selected { " ▶ " } else { "   " };
    let marker_style = if is_selected {
        engine.fg(palette.status_info).bold()
    } else {
        engine.fg(palette.text_muted)
    };
    let name_style = if is_selected {
        engine
            .bg(palette.overlay)
            .fg(engine.color(palette.text_primary))
            .bold()
    } else {
        engine.fg(palette.text_primary)
    };

    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::styled(preset.as_str().to_string(), name_style),
    ];
    if is_active {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            active_t("dialog.theme_select.active_badge"),
            engine.fg(palette.status_success).bold(),
        ));
    }
    Line::from(spans)
}

/// Renders the theme selector dialog using the active semantic palette.
///
/// The shared modal frame owns the card chrome: clearing the area, filling the
/// `surface` background, the border tone, the top inset, the horizontal margin,
/// the pre-action spacer, and the docked hint row. This renderer only draws the
/// preset list into its own padded body band (`#c-1`, `#c-2`, Conventions §6.7,
/// §6.8).
pub fn render_theme_selector(frame: &mut ratatui::Frame, state: &ThemeSelectorState) {
    let engine = current_engine();
    let palette = engine.palette();
    let area = theme_selector_area(frame.area());
    let title = active_t("dialog.theme_select.title");
    render_modal_container(frame, area, &title, ModalTone::Info);

    let geometry = modal_geometry(area);

    let active = engine.preset();
    // Render only the visible window of presets so the drawn rows always match
    // the rows the click and keyboard navigation address (`#c-4`).
    let visible = selector_visible_rows(geometry.body);
    let start = state.selector.scroll_offset();
    let mut body: Vec<Line<'static>> = Vec::with_capacity(visible + 1);
    body.push(Line::from(Span::styled(
        active_t("dialog.theme_select.preview_note"),
        engine.fg(palette.text_muted),
    )));
    for (idx, preset) in ThemePreset::ALL
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
    {
        body.push(preset_row(
            *preset,
            idx == state.selector.selected_index(),
            *preset == active,
        ));
    }
    frame.render_widget(Paragraph::new(body), geometry.body);

    // The hint vocabulary is docked to the bottom inner row as its own filled
    // strip, separated from the preset list by the action band.
    render_dialog_footer(
        frame,
        geometry.hint_row,
        &verb_hint(active_t("dialog.theme_select.footer")),
    );
}

/// Commits a preset: records it in configuration, applies it live, and persists
/// it to `tui.toml`. Shared by the keyboard `Enter` path and the mouse click on
/// the highlighted row (`#c-4`).
fn commit_preset(st: &mut AppState, preset: ThemePreset) {
    st.tui_config.theme.name = Some(preset.as_str().to_string());
    st.apply_theme_preset(preset);
    match crate::config::save_theme_preset(preset.as_str()) {
        Ok(_) => {
            st.notify_success(active_t("notify.theme_saved").replacen("{}", preset.as_str(), 1))
        }
        Err(err) => st.notify_error(active_t("notify.theme_save_failed").replacen(
            "{}",
            &err.to_string(),
            1,
        )),
    }
}

/// Handles a key event while the theme selector owns the keyboard.
///
/// Navigation previews the highlighted preset immediately. `Enter` commits the
/// choice and persists it to `tui.toml`; `Esc` restores the preset that was
/// active when the dialog opened. Returns `true` when the event was consumed.
pub fn handle_theme_selector_key(key: KeyEvent, st: &mut AppState, visible_height: usize) -> bool {
    let Some(mut selector_state) = st.theme_selector.take() else {
        return false;
    };

    if selector_state.selector.handle_nav_key(key, visible_height) {
        let preset = selector_state.selected_preset();
        st.apply_theme_preset(preset);
        st.theme_selector = Some(selector_state);
        return true;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            let original = selector_state.original;
            st.apply_theme_preset(original);
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let preset = selector_state.selected_preset();
            commit_preset(st, preset);
        }
        _ => {
            st.theme_selector = Some(selector_state);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_state;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn selector_opens_seeded_on_the_active_preset() {
        let state = ThemeSelectorState::new(ThemePreset::Catppuccin);
        assert_eq!(state.original, ThemePreset::Catppuccin);
        assert_eq!(state.selected_preset(), ThemePreset::Catppuccin);
    }

    #[test]
    fn navigation_previews_and_escape_restores_the_original() {
        let mut st = test_state();
        st.active_theme_preset = ThemePreset::TauqeDark;
        st.open_theme_selector();

        handle_theme_selector_key(key(KeyCode::Char('n')), &mut st, 5);
        assert_eq!(st.active_theme_preset, ThemePreset::TauqeLight);
        assert!(
            st.theme_selector.is_some(),
            "the dialog stays open while previewing"
        );

        handle_theme_selector_key(key(KeyCode::Esc), &mut st, 5);
        assert_eq!(st.active_theme_preset, ThemePreset::TauqeDark);
        assert!(
            st.theme_selector.is_none(),
            "cancellation closes the dialog"
        );
    }

    #[test]
    fn unhandled_key_keeps_the_dialog_open() {
        let mut st = test_state();
        st.open_theme_selector();
        let before = st.active_theme_preset;

        handle_theme_selector_key(key(KeyCode::Char('x')), &mut st, 5);
        assert!(st.theme_selector.is_some());
        assert_eq!(st.active_theme_preset, before);
    }

    #[test]
    fn handler_is_inert_without_an_open_dialog() {
        let mut st = test_state();
        assert!(!handle_theme_selector_key(key(KeyCode::Enter), &mut st, 5));
    }

    /// The body band keeps one row of air below the border title and one column
    /// of margin off each vertical border (Conventions §6.7, §6.8).
    #[test]
    fn body_area_keeps_the_top_inset_and_the_horizontal_margin() {
        let term = Rect::new(0, 0, 80, 24);
        let area = theme_selector_area(term);
        let body = theme_selector_body_area(term);

        assert_eq!(body.y, area.y + 2, "border + top inset");
        assert_eq!(body.x, area.x + 2, "border + horizontal padding");
        assert_eq!(body.width, area.width - 4, "both vertical margins");
    }

    /// The hint strip is docked to the bottom inner row of the card, so it never
    /// floats loose between the presets and the border (Conventions §6.4).
    #[test]
    fn hint_row_is_docked_above_the_bottom_border() {
        let term = Rect::new(0, 0, 80, 24);
        let area = theme_selector_area(term);
        let geometry = modal_geometry(area);

        assert_eq!(
            geometry.hint_row.y + geometry.hint_row.height,
            area.y + area.height - 1,
            "hint strip is docked above the bottom border"
        );
        assert_eq!(geometry.hint_row.height, 1);
    }

    /// Clicking a preset row selects exactly the preset painted on that row, so
    /// the drawn list and the hit-tested list can never drift apart
    /// (`#c-4`, Conventions §6.6).
    #[test]
    fn clicking_a_rendered_preset_row_selects_the_preset_drawn_on_it() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let term = Rect::new(0, 0, 80, 24);
        let mut st = test_state();
        // Seed the cursor on the first preset so the clicked row is not the
        // already-highlighted one, which would commit instead of selecting.
        st.active_theme_preset = ThemePreset::TauqeDark;
        st.open_theme_selector();
        let selector_state = st.theme_selector.clone().expect("selector opens");

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|frame| render_theme_selector(frame, &selector_state))
            .unwrap();

        let body = theme_selector_body_area(term);
        // Row 0 of the body is the live-preview caption; preset rows follow it,
        // so body row 3 carries preset index 2 at zero scroll offset.
        let target_row = body.y + 3;
        let expected = ThemePreset::ALL[2];
        let drawn: String = (body.x..body.x + body.width)
            .map(|x| terminal.backend().buffer()[(x, target_row)].symbol())
            .collect();
        assert!(
            drawn.contains(expected.as_str()),
            "row {target_row} draws {drawn:?}, expected {}",
            expected.as_str()
        );

        assert!(handle_theme_selector_click(
            &mut st,
            body.x + 5,
            target_row,
            term
        ));
        let selected = st
            .theme_selector
            .expect("selection keeps the dialog open")
            .selected_preset();
        assert_eq!(selected, expected);
        assert_eq!(st.active_theme_preset, expected);
    }
}
