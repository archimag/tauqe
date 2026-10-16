//! Reusable three-tier modal frame: header/title, body, and actions footer.
//!
//! Every modal dialog shares this frame. It renders the bordered container,
//! splits it into a flexible body and a fixed actions bar, and styles the
//! confirm/cancel buttons according to the focused button and whether the action
//! is destructive. Views supply only the body content through a closure, keeping
//! the dialog chrome in one place (Conventions.md §7).
//!
//! All chrome draws from the active semantic palette: borders, surface, and
//! button styles are projected through [`crate::theme::engine`], so a modal
//! keeps its meaning under any terminal palette (`#c-1`, `#c-2`).

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{ConfirmDialogButton, ModalActionsState};
use crate::command::dialog::{DialogCommand, DIALOG_COMMAND_MAP};
use crate::config::{active_meta_modifier, active_primary_modifier};
use crate::theme::color::RgbColor;
use crate::theme::engine::{current_engine, ThemeEngine};

/// Semantic border tone of a modal frame.
///
/// Tones replace raw ANSI accents so a frame keeps its meaning regardless of the
/// terminal's host palette: the concrete color is resolved from the active theme
/// (`#c-1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModalTone {
    /// Informational chrome; the default.
    #[default]
    Info,
    /// Cautionary chrome for reversible or attention-needing actions.
    Warning,
    /// Destructive chrome for irreversible actions.
    Dangerous,
    /// Accent chrome for thematic dialogs (plans, reviews, refinement).
    Accent,
}

impl ModalTone {
    /// Resolves this tone into a border style from the active palette.
    pub fn border_style(self, engine: &ThemeEngine) -> Style {
        let palette = engine.palette();
        let role = match self {
            ModalTone::Info => palette.status_info,
            ModalTone::Warning => palette.status_warning,
            ModalTone::Dangerous => palette.status_error,
            ModalTone::Accent => palette.button_active_bg,
        };
        engine.fg(role)
    }
}

/// Builds an action-button style from a background and foreground palette role.
///
/// Backgrounds resolve through [`ThemeEngine::bg`], so the 16-color fallback can
/// still convey the button surface with an attribute when no color is available.
pub fn button_style(engine: &ThemeEngine, bg: RgbColor, fg: RgbColor, emphasized: bool) -> Style {
    let mut style = engine.bg(bg).fg(engine.color(fg));
    if emphasized {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

/// Projects one cognitive group of the shared dialog vocabulary.
///
/// Chords are listed in `actions` order and deduplicated, so a group reads
/// `Tab / Shift+Tab` instead of mirroring the registry declaration order. Every
/// chord is rendered through the configured `C-` / `M-` modifiers, so a footer
/// never hardcodes a modifier string (Conventions §4, §6).
fn dialog_hint_group(actions: &[DialogCommand], verb: &str) -> Option<String> {
    let primary = active_primary_modifier();
    let meta = active_meta_modifier();
    let mut keys: Vec<String> = Vec::new();
    for action in actions {
        for spec in DIALOG_COMMAND_MAP.specs() {
            if spec.action == *action {
                let key = spec.chord.display(primary, meta);
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
    }
    (!keys.is_empty()).then(|| format!("{} {}", keys.join(" / "), verb))
}

/// Navigation and activation hint: focus cycling and control activation.
///
/// Derived from the shared dialog command map, so the modal footers advertise
/// exactly the chords the dialogs intercept.
pub fn dialog_navigation_hint() -> String {
    let focus = dialog_hint_group(
        &[DialogCommand::FocusNext, DialogCommand::FocusPrev],
        "focus",
    );
    let activate = dialog_hint_group(&[DialogCommand::Activate], "activate");
    [focus, activate]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" • ")
}

/// The standard `y` / `n` fast-select hint.
pub fn dialog_fast_select_hint() -> String {
    dialog_hint_group(
        &[DialogCommand::ConfirmFast, DialogCommand::CancelFast],
        "fast select",
    )
    .unwrap_or_default()
}

/// The dismissal hint (`Esc` / `q`).
pub fn dialog_dismiss_hint() -> String {
    dialog_hint_group(&[DialogCommand::Dismiss], "dismiss").unwrap_or_default()
}

/// The full universal dialog footer hint, used when a dialog supplies no text
/// of its own.
pub fn dialog_footer_hint() -> String {
    [
        dialog_navigation_hint(),
        dialog_fast_select_hint(),
        dialog_dismiss_hint(),
    ]
    .into_iter()
    .filter(|group| !group.is_empty())
    .collect::<Vec<_>>()
    .join(" • ")
}

/// Splits a dialog hint into styled spans: each chord reads as an interactive
/// key, each group's verb as its description, and the `/` and `•` separators
/// recede into the muted role.
///
/// The input is the footer vocabulary produced by [`dialog_footer_hint`] and the
/// batch dialog: `•`-separated groups whose keys are ` / `-separated and which
/// end in a verb (for example `Tab / Shift+Tab focus`).
fn hint_spans(engine: &ThemeEngine, hint: &str) -> Vec<Span<'static>> {
    if hint.is_empty() {
        return Vec::new();
    }

    let palette = engine.palette();
    let key_style = engine.fg(palette.text_primary).add_modifier(Modifier::BOLD);
    let verb_style = engine.fg(palette.text_secondary);
    let sep_style = engine.fg(palette.text_muted);

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (group_index, group) in hint.split(" • ").enumerate() {
        if group_index > 0 {
            spans.push(Span::styled(" • ", sep_style));
        }
        // A group reads `CHORD1 / CHORD2 / ... / CHORDn VERB`: the chords are
        // single tokens joined by `" / "` and the trailing verb is a phrase, so
        // only the final `" / "` segment splits into its chord and the verb.
        let parts: Vec<&str> = group.split(" / ").collect();
        let last = parts.last().copied().unwrap_or("");
        let (last_chord, verb) = match last.split_once(' ') {
            Some((chord, verb)) => (chord, verb),
            None => (last, ""),
        };
        let mut chords: Vec<&str> = parts[..parts.len().saturating_sub(1)].to_vec();
        chords.push(last_chord);
        chords.retain(|chord| !chord.is_empty());
        for (key_index, key) in chords.iter().enumerate() {
            if key_index > 0 {
                spans.push(Span::styled(" / ", sep_style));
            }
            spans.push(Span::styled((*key).to_string(), key_style));
        }
        if !verb.is_empty() {
            spans.push(Span::styled(format!(" {}", verb), verb_style));
        }
    }
    spans
}

/// Renders the docked dialog footer strip.
///
/// The strip is a full-width row filled with `palette.overlay` that carries the
/// semantic hint vocabulary: chords read as interactive keys, their verbs as
/// descriptions, and the separators recede into the muted role. Docking the
/// hint to its own row separates it from the interactive controls and closes
/// the dialog card visually (`#c-1`, `#c-2`).
pub fn render_dialog_footer(frame: &mut ratatui::Frame, area: Rect, hint: &str) {
    let engine = current_engine();
    let palette = engine.palette();
    let footer = Paragraph::new(Line::from(hint_spans(&engine, hint)))
        .alignment(Alignment::Center)
        .style(Style::default().bg(engine.color(palette.overlay)));
    frame.render_widget(footer, area);
}

/// Renders a cleared, surface-filled, bordered modal container with an explicit border style.
///
/// Returns the inner [`Rect`] inside the border for content rendering.
/// This guarantees consistent background clearing (`Clear`), palette surface
/// fill (`palette.surface`), and border styling across all dialogs without code duplication.
pub fn render_modal_container_styled(
    frame: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    border_style: Style,
) -> Rect {
    let engine = current_engine();
    let palette = engine.palette();

    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(format!(" {} ", title.trim()))
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(engine.color(palette.surface)));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    inner_area
}

/// Renders a cleared, surface-filled, bordered modal container using a semantic [`ModalTone`].
///
/// Returns the inner [`Rect`] inside the border for content rendering.
pub fn render_modal_container(
    frame: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    tone: ModalTone,
) -> Rect {
    let engine = current_engine();
    render_modal_container_styled(frame, area, title, tone.border_style(&engine))
}

/// Parameters for the standardized three-tier modal frame.
pub struct ModalFrame<'a> {
    pub title: &'a str,
    /// Semantic border accent of the frame.
    pub tone: ModalTone,
    pub area: Rect,
    pub confirm_label: &'a str,
    pub cancel_label: &'a str,
    pub actions_state: ModalActionsState,
    pub is_actions_focused: bool,
    pub destructive: bool,
    pub hint_text: Option<&'a str>,
}

/// Historical alias for [`ModalFrame`] kept for existing call sites.
pub type ModalFrameParams<'a> = ModalFrame<'a>;

impl ModalFrame<'_> {
    /// Renders the frame, delegating the central body area to `render_body`.
    pub fn render<F>(&self, frame: &mut ratatui::Frame, render_body: F)
    where
        F: FnOnce(&mut ratatui::Frame, Rect),
    {
        let engine = current_engine();
        let palette = engine.palette();

        let inner_area = render_modal_container(frame, self.area, self.title, self.tone);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(2),
                Constraint::Length(2),
                Constraint::Length(1),
            ])
            .split(inner_area);

        render_body(frame, chunks[0]);

        let resting = button_style(&engine, palette.button_bg, palette.button_fg, false);
        let active = button_style(
            &engine,
            palette.button_active_bg,
            palette.text_inverse,
            true,
        );
        let dangerous = button_style(&engine, palette.status_error, palette.text_inverse, true);
        let confirm_active = if self.destructive { dangerous } else { active };

        let (confirm_style, cancel_style) = if self.is_actions_focused {
            match self.actions_state.focused_button {
                ConfirmDialogButton::Confirm => (confirm_active, resting),
                ConfirmDialogButton::Cancel => (resting, active),
            }
        } else {
            (resting, resting)
        };

        let actions_line = Line::from(vec![
            Span::styled(format!(" [ {} ] ", self.confirm_label), confirm_style),
            Span::raw("    "),
            Span::styled(format!(" [ {} ] ", self.cancel_label), cancel_style),
        ]);
        let actions_widget = Paragraph::new(actions_line).alignment(Alignment::Center);
        frame.render_widget(actions_widget, chunks[1]);

        // The navigation vocabulary is docked to the very bottom row as its own
        // strip, separated from the action buttons, so it reads as persistent
        // chrome rather than a second line of buttons.
        let hint = match self.hint_text {
            Some(text) => text.to_string(),
            None => dialog_footer_hint(),
        };
        render_dialog_footer(frame, chunks[2], &hint);
    }
}

/// Free-function form of [`ModalFrame::render`] for views that build the
/// parameter bundle inline.
pub fn render_modal_frame<F>(frame: &mut ratatui::Frame, params: ModalFrame<'_>, render_body: F)
where
    F: FnOnce(&mut ratatui::Frame, Rect),
{
    params.render(frame, render_body);
}

/// Action-button rectangles of a [`ModalFrame`], derived from the frame area the
/// caller passes to [`ModalFrame::render`].
///
/// Rendering and mouse hit-testing share this geometry, so the drawn
/// confirm/cancel buttons are always the clickable ones (Conventions §6, §7).
pub fn modal_action_buttons(area: Rect, confirm_label: &str, cancel_label: &str) -> (Rect, Rect) {
    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    // Mirror the frame's own split (flexible body, two-row button zone, one-row
    // docked hint strip) so hit-testing lands on the row the buttons are drawn
    // in and can never drift from the rendered layout (Conventions §6, §7).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(2),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner);
    let action_bar = chunks[1];

    let confirm_w = (confirm_label.chars().count() as u16).saturating_add(6);
    let cancel_w = (cancel_label.chars().count() as u16).saturating_add(6);
    let gap = 4u16;
    let total = confirm_w + gap + cancel_w;
    let start_x = action_bar.x + action_bar.width.saturating_sub(total) / 2;

    (
        Rect::new(start_x, action_bar.y, confirm_w, 1),
        Rect::new(start_x + confirm_w + gap, action_bar.y, cancel_w, 1),
    )
}

#[cfg(test)]
mod tests {
    use super::hint_spans;
    use crate::theme::engine::current_engine;

    fn contents(hint: &str) -> Vec<String> {
        let engine = current_engine();
        hint_spans(&engine, hint)
            .into_iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    /// A multi-word verb must stay with its group instead of leaking into the
    /// chord list, so the fast-select hint keeps exactly its two keys.
    #[test]
    fn hint_spans_keeps_a_multi_word_verb_with_its_group() {
        assert_eq!(
            contents("y / n fast select"),
            vec!["y", " / ", "n", " fast select"],
        );
    }

    #[test]
    fn hint_spans_keeps_single_word_verbs_and_separators() {
        assert_eq!(
            contents("Tab / Shift+Tab focus • Enter activate"),
            vec![
                "Tab",
                " / ",
                "Shift+Tab",
                " focus",
                " • ",
                "Enter",
                " activate",
            ],
        );
    }
}
