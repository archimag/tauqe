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
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::AppState;
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
pub fn theme_selector_visible_height(term: Rect) -> usize {
    (theme_selector_area(term).height.saturating_sub(5) as usize).max(1)
}

/// Body area (the scrollable preset rows) of the selector, shared by rendering
/// and mouse hit-testing.
pub fn theme_selector_body_area(term: Rect) -> Rect {
    let area = theme_selector_area(term);
    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(1)])
        .split(inner);
    chunks[0]
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
    let inside = col >= area.x
        && col < area.x + area.width
        && row >= area.y
        && row < area.y + area.height;
    if !inside {
        let original = selector_state.original;
        st.apply_theme_preset(original);
        return true;
    }

    let body = theme_selector_body_area(term);
    let inside_body = col >= body.x
        && col < body.x + body.width
        && row >= body.y
        && row < body.y + body.height;
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
pub fn render_theme_selector(frame: &mut ratatui::Frame, state: &ThemeSelectorState) {
    let engine = current_engine();
    let palette = engine.palette();
    let area = theme_selector_area(frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(format!(" {} ", active_t("dialog.theme_select.title")))
        .borders(Borders::ALL)
        .border_style(engine.fg(palette.status_info))
        .style(ratatui::style::Style::default().bg(engine.color(palette.surface)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(1)])
        .split(inner);

    let active = engine.preset();
    // Render only the visible window of presets so the drawn rows always match
    // the rows the click and keyboard navigation address (`#c-4`).
    let visible = theme_selector_visible_height(frame.area());
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
    frame.render_widget(Paragraph::new(body), chunks[0]);

    let footer = Paragraph::new(Span::styled(
        active_t("dialog.theme_select.footer"),
        engine.fg(palette.text_muted),
    ))
    .alignment(Alignment::Center);
    frame.render_widget(footer, chunks[1]);
}

/// Commits a preset: records it in configuration, applies it live, and persists
/// it to `tui.toml`. Shared by the keyboard `Enter` path and the mouse click on
/// the highlighted row (`#c-4`).
fn commit_preset(st: &mut AppState, preset: ThemePreset) {
    st.tui_config.theme.name = Some(preset.as_str().to_string());
    st.apply_theme_preset(preset);
    match crate::config::save_theme_preset(preset.as_str()) {
        Ok(_) => st.notify_success(format!("Theme saved: {}", preset.as_str())),
        Err(err) => st.notify_error(format!("Failed to save theme: {}", err)),
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
        assert!(st.theme_selector.is_none(), "cancellation closes the dialog");
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
}
