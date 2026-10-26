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

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{ConfirmDialogButton, ModalActionsState};
use crate::command::dialog::{DialogCommand, DIALOG_COMMAND_MAP};
use crate::config::{active_meta_modifier, active_primary_modifier};
use crate::i18n::active_t;
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

/// One semantic group of the docked hint strip: the chords that reach an action
/// and the localized verb that names it.
///
/// The strip draws from independent sources — the shared dialog command map and
/// the per-dialog localizations — so those sources hand over typed groups
/// instead of a formatted sentence. The renderer therefore never recovers the
/// structure from text, and a verb containing a separator glyph can never split
/// into phantom chords (Conventions §6.5).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HintGroup {
    /// Chords reaching the action, rendered ` / `-separated and emphasized.
    pub chords: Vec<String>,
    /// Localized verb naming the action, rendered as one indivisible span.
    pub verb: String,
}

impl HintGroup {
    /// Creates a group that names no abstract chord, only a localized sentence.
    pub fn verb(verb: impl Into<String>) -> Self {
        Self {
            chords: Vec::new(),
            verb: verb.into(),
        }
    }
}

/// Wraps a localized, pre-formatted hint sentence into a single verb-only group.
///
/// A dialog whose hint is one authored catalog sentence has no chord list to
/// project, so the sentence reaches the strip as one indivisible span instead of
/// being re-parsed for keys it may not contain (Conventions §6.5).
pub fn verb_hint(sentence: impl Into<String>) -> Vec<HintGroup> {
    vec![HintGroup::verb(sentence)]
}

/// Projects one cognitive group of the shared dialog vocabulary.
///
/// Chords are listed in `actions` order and deduplicated, so a group reads
/// `Tab / Shift+Tab` instead of mirroring the registry declaration order. Every
/// chord is rendered through the configured `C-` / `M-` modifiers, so a footer
/// never hardcodes a modifier string (Conventions §4, §6).
fn dialog_hint_group(actions: &[DialogCommand], verb_key: &str) -> Option<HintGroup> {
    let primary = active_primary_modifier();
    let meta = active_meta_modifier();
    let mut chords: Vec<String> = Vec::new();
    for action in actions {
        for spec in DIALOG_COMMAND_MAP.specs() {
            if spec.action == *action {
                let key = spec.chord.display(primary, meta);
                if !chords.contains(&key) {
                    chords.push(key);
                }
            }
        }
    }
    // The verb is resolved from the canonical catalog, so a hot-swapped locale
    // reaches every dialog footer rendered without an `&AppState` (c-1, c-5).
    let verb = active_t(verb_key);
    (!chords.is_empty()).then_some(HintGroup { chords, verb })
}

/// Navigation and activation hint: focus cycling and control activation.
///
/// Derived from the shared dialog command map, so the modal footers advertise
/// exactly the chords the dialogs intercept.
pub fn dialog_navigation_hint() -> Vec<HintGroup> {
    [
        dialog_hint_group(
            &[DialogCommand::FocusNext, DialogCommand::FocusPrev],
            "dialog.action.focus",
        ),
        dialog_hint_group(&[DialogCommand::Activate], "dialog.action.activate"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The standard `y` / `n` fast-select hint.
pub fn dialog_fast_select_hint() -> Vec<HintGroup> {
    dialog_hint_group(
        &[DialogCommand::ConfirmFast, DialogCommand::CancelFast],
        "dialog.action.fast_select",
    )
    .into_iter()
    .collect()
}

/// The dismissal hint (`Esc` / `q`).
pub fn dialog_dismiss_hint() -> Vec<HintGroup> {
    dialog_hint_group(&[DialogCommand::Dismiss], "dialog.action.dismiss")
        .into_iter()
        .collect()
}

/// The universal dialog footer vocabulary, used when a dialog supplies no hint
/// of its own: focus cycling, activation, fast selection, and dismissal.
pub fn dialog_footer_hint() -> Vec<HintGroup> {
    let mut groups = dialog_navigation_hint();
    groups.extend(dialog_fast_select_hint());
    groups.extend(dialog_dismiss_hint());
    groups
}

/// Builds the styled spans of a dialog hint: every chord reads as an
/// interactive key, every verb as its description, and the ` / ` and ` • `
/// separators recede into the muted role.
///
/// The groups are consumed exactly as they are, so the styling is derived from
/// the same structure that produced the hint and never from a re-parsed string
/// (Conventions §6.5).
fn hint_spans(engine: &ThemeEngine, groups: &[HintGroup]) -> Vec<Span<'static>> {
    let palette = engine.palette();
    let key_style = engine.fg(palette.text_primary).add_modifier(Modifier::BOLD);
    let verb_style = engine.fg(palette.text_secondary);
    let sep_style = engine.fg(palette.text_muted);

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (group_index, group) in groups.iter().enumerate() {
        if group_index > 0 {
            spans.push(Span::styled(" • ", sep_style));
        }
        for (key_index, chord) in group.chords.iter().enumerate() {
            if key_index > 0 {
                spans.push(Span::styled(" / ", sep_style));
            }
            spans.push(Span::styled(chord.clone(), key_style));
        }
        if group.verb.is_empty() {
            continue;
        }
        // A group without chords is a whole authored sentence, so it carries no
        // leading space of its own.
        let verb = if group.chords.is_empty() {
            group.verb.clone()
        } else {
            format!(" {}", group.verb)
        };
        spans.push(Span::styled(verb, verb_style));
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
pub fn render_dialog_footer(frame: &mut ratatui::Frame, area: Rect, hint: &[HintGroup]) {
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

/// Vertical bands of a standard modal card, with the air separators already
/// applied.
///
/// Every dialog shares one vertical formula: a one-row top inset under the
/// border title, the flexible body, a one-row pre-action spacer, the action bar,
/// and the docked hint strip pinned to the bottom border. The body and the
/// action bar are additionally padded one column off each vertical border, so
/// content never presses against the card frame (Conventions §6.7, §6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModalGeometry {
    /// Content column, inset from the top border and padded off both vertical
    /// borders.
    pub body: Rect,
    /// Row holding the confirm and cancel controls, padded like the body and
    /// separated from the body by one blank row.
    pub action_bar: Rect,
    /// Docked, non-interactive hint strip on the bottom inner row.
    pub hint_row: Rect,
}

/// Insets a rectangle by the one-cell card border.
fn modal_inner_area(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

/// Keeps one column of horizontal air between content and each vertical border
/// (Conventions §6.8).
fn with_horizontal_padding(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        area.height,
    )
}

/// Splits a modal card area into the shared air-aware vertical bands.
///
/// Dialogs render their content into [`ModalGeometry::body`] and never compute
/// the top inset, the spacer, or the padding offsets by hand, so every card
/// breathes the same way (Conventions §6.7, §6.8).
pub fn modal_geometry(area: Rect) -> ModalGeometry {
    modal_geometry_from_inner(modal_inner_area(area))
}

/// Splits the inner area returned by [`render_modal_container`] into the shared
/// bands.
fn modal_geometry_from_inner(inner: Rect) -> ModalGeometry {
    let top_inset_h = inner.height.min(1);
    let mut cursor = inner.y.saturating_add(top_inset_h);
    let mut remaining = inner.height.saturating_sub(top_inset_h);

    // The bottom chrome is reserved first, so a short card still shows its
    // controls instead of clipping them behind an overgrown body.
    let hint_h = remaining.min(1);
    remaining = remaining.saturating_sub(hint_h);
    let action_h = remaining.min(1);
    remaining = remaining.saturating_sub(action_h);
    let spacer_h = remaining.min(1);
    let body_h = remaining.saturating_sub(spacer_h);

    let body = with_horizontal_padding(Rect::new(inner.x, cursor, inner.width, body_h));
    cursor = cursor.saturating_add(body_h).saturating_add(spacer_h);
    let action_bar = with_horizontal_padding(Rect::new(inner.x, cursor, inner.width, action_h));
    cursor = cursor.saturating_add(action_h);
    let hint_row = Rect::new(inner.x, cursor, inner.width, hint_h);

    ModalGeometry {
        body,
        action_bar,
        hint_row,
    }
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
    /// Structured hint vocabulary; `None` renders [`dialog_footer_hint`].
    pub hint_text: Option<&'a [HintGroup]>,
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
        // The frame owns the whole vertical formula: the top inset, the padded
        // body, the pre-action spacer, the button row, and the docked hint
        // strip, so a body can neither collide with the border title nor press
        // the buttons against its own text (Conventions §6.7, §6.8).
        let geometry = modal_geometry_from_inner(inner_area);

        render_body(frame, geometry.body);

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
        frame.render_widget(actions_widget, geometry.action_bar);

        // The navigation vocabulary is docked to the very bottom row as its own
        // strip, separated from the action buttons, so it reads as persistent
        // chrome rather than a second line of buttons.
        let hint = self
            .hint_text
            .map(<[HintGroup]>::to_vec)
            .unwrap_or_else(dialog_footer_hint);
        render_dialog_footer(frame, geometry.hint_row, &hint);
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
    modal_action_buttons_in(modal_geometry(area).action_bar, confirm_label, cancel_label)
}

/// Hit rectangles of the confirm and cancel buttons inside an action bar.
///
/// Dialogs that lay out their own action bar (pickers stacking a model list and
/// an editor above the buttons) hand that row here instead of recomputing the
/// centering, so the drawn buttons are always the clickable ones and every card
/// keeps the same button geometry (Conventions §6.3, §6.6).
pub fn modal_action_buttons_in(
    action_bar: Rect,
    confirm_label: &str,
    cancel_label: &str,
) -> (Rect, Rect) {
    let confirm_w = (UnicodeWidthStr::width(confirm_label) as u16).saturating_add(6);
    let cancel_w = (UnicodeWidthStr::width(cancel_label) as u16).saturating_add(6);
    let gap = 4u16;
    let total = confirm_w.saturating_add(gap).saturating_add(cancel_w);
    let start_x = action_bar
        .x
        .saturating_add(action_bar.width.saturating_sub(total) / 2);

    (
        Rect::new(start_x, action_bar.y, confirm_w, 1),
        Rect::new(
            start_x.saturating_add(confirm_w).saturating_add(gap),
            action_bar.y,
            cancel_w,
            1,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::{hint_spans, modal_action_buttons, modal_geometry, HintGroup};
    use crate::theme::engine::current_engine;

    fn contents(groups: &[HintGroup]) -> Vec<String> {
        let engine = current_engine();
        hint_spans(&engine, groups)
            .into_iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    /// A multi-word verb must stay with its group instead of leaking into the
    /// chord list, so the fast-select hint keeps exactly its two keys.
    #[test]
    fn hint_spans_keeps_a_multi_word_verb_with_its_group() {
        let group = HintGroup {
            chords: vec!["y".to_string(), "n".to_string()],
            verb: "fast select".to_string(),
        };

        assert_eq!(contents(&[group]), vec!["y", " / ", "n", " fast select"]);
    }

    #[test]
    fn hint_spans_keeps_single_word_verbs_and_separators() {
        let groups = [
            HintGroup {
                chords: vec!["Tab".to_string(), "Shift+Tab".to_string()],
                verb: "focus".to_string(),
            },
            HintGroup {
                chords: vec!["Enter".to_string()],
                verb: "activate".to_string(),
            },
        ];

        assert_eq!(
            contents(&groups),
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

    /// A verb is opaque text: a separator glyph inside it must never split the
    /// verb into phantom chords styled as keys (Conventions §6.5).
    #[test]
    fn hint_spans_never_parses_separator_glyphs_out_of_a_verb() {
        assert_eq!(
            contents(&[HintGroup::verb("j / k or ↑ / ↓ scroll")]),
            vec!["j / k or ↑ / ↓ scroll"],
        );

        let group = HintGroup {
            chords: vec!["Esc".to_string()],
            verb: "cancel / dismiss • close".to_string(),
        };
        assert_eq!(contents(&[group]), vec!["Esc", " cancel / dismiss • close"],);
    }

    /// Every chord of a multi-chord group is its own emphasized span, so a
    /// chord list can never collapse into a single styled token.
    #[test]
    fn hint_spans_keeps_every_chord_of_a_multi_chord_group() {
        let group = HintGroup {
            chords: vec![
                "j".to_string(),
                "k".to_string(),
                "↑".to_string(),
                "↓".to_string(),
            ],
            verb: "scroll".to_string(),
        };

        assert_eq!(
            contents(&[group]),
            vec!["j", " / ", "k", " / ", "↑", " / ", "↓", " scroll"],
        );
    }

    /// The body starts below the border and the one-row top inset, and stays one
    /// column away from each vertical border (Conventions §6.7, §6.8).
    #[test]
    fn body_is_inset_from_the_top_and_padded_off_the_borders() {
        let term = ratatui::layout::Rect::new(0, 0, 80, 24);
        let area = crate::ui::centered_rect(60, 70, term);
        let geometry = modal_geometry(area);

        assert_eq!(geometry.body.y, area.y + 2, "border + top inset");
        assert_eq!(geometry.body.x, area.x + 2, "border + horizontal padding");
        assert_eq!(geometry.body.width, area.width - 4);
    }

    /// One blank row separates the body from the action bar, and the hint strip
    /// sits on the bottom inner row (Conventions §6.7).
    #[test]
    fn action_bar_follows_one_blank_row_and_hint_row_is_docked() {
        let term = ratatui::layout::Rect::new(0, 0, 80, 24);
        let area = crate::ui::centered_rect(60, 70, term);
        let geometry = modal_geometry(area);

        assert_eq!(
            geometry.action_bar.y,
            geometry.body.y + geometry.body.height + 1,
            "pre-action spacer"
        );
        assert_eq!(
            geometry.hint_row.y + geometry.hint_row.height,
            area.y + area.height - 1,
            "hint strip is docked above the bottom border"
        );
    }

    /// Hit-testing derives its row from the same formula the render path uses.
    #[test]
    fn buttons_land_on_the_rendered_action_bar() {
        let term = ratatui::layout::Rect::new(0, 0, 80, 24);
        let area = crate::ui::centered_rect(60, 70, term);
        let (confirm, cancel) = modal_action_buttons(area, "Confirm", "Cancel");
        let action_bar = modal_geometry(area).action_bar;

        assert_eq!(confirm.y, action_bar.y);
        assert_eq!(cancel.y, action_bar.y);
        assert!(confirm.x >= action_bar.x);
        assert!(cancel.x + cancel.width <= action_bar.x + action_bar.width);
    }

    /// Button hit rectangles must follow terminal display width, so a CJK label
    /// (two columns per scalar) stays aligned with the drawn button
    /// (Conventions §6, §9).
    #[test]
    fn modal_action_buttons_align_with_rendered_cjk_labels() {
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::Terminal;

        use crate::app::ModalActionsState;

        use super::{render_modal_frame, ModalFrame, ModalTone};

        let term = Rect::new(0, 0, 100, 30);
        let area = crate::ui::centered_rect(60, 40, term);
        let confirm_label = "确认";
        let cancel_label = "取消";

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|f| {
                render_modal_frame(
                    f,
                    ModalFrame {
                        title: "Test",
                        tone: ModalTone::Info,
                        area,
                        confirm_label,
                        cancel_label,
                        actions_state: ModalActionsState::default(),
                        is_actions_focused: true,
                        destructive: false,
                        hint_text: Some(&[]),
                    },
                    |_frame, _body| {},
                );
            })
            .unwrap();

        let (confirm_btn, cancel_btn) = modal_action_buttons(area, confirm_label, cancel_label);
        let buffer = terminal.backend().buffer();
        let read = |rect: Rect| -> String {
            (rect.x..rect.x + rect.width)
                .map(|x| buffer[(x, rect.y)].symbol())
                .collect()
        };

        let confirm_row = read(confirm_btn);
        let cancel_row = read(cancel_btn);
        assert!(confirm_row.contains('确'), "confirm row: {confirm_row:?}");
        assert!(cancel_row.contains('取'), "cancel row: {cancel_row:?}");
    }

    /// A body that fills its whole area must still leave the row above the
    /// buttons empty, so text and controls never sit on adjacent rows
    /// (Conventions §6.7).
    #[test]
    fn blank_separator_row_survives_a_body_that_fills_its_area() {
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::text::Line;
        use ratatui::widgets::Paragraph;
        use ratatui::Terminal;

        use crate::app::ModalActionsState;

        use super::{render_modal_frame, ModalFrame, ModalTone};

        let term = Rect::new(0, 0, 80, 24);
        let area = crate::ui::centered_rect(60, 70, term);
        let confirm_label = "Confirm";
        let cancel_label = "Cancel";

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|f| {
                render_modal_frame(
                    f,
                    ModalFrame {
                        title: "Test",
                        tone: ModalTone::Info,
                        area,
                        confirm_label,
                        cancel_label,
                        actions_state: ModalActionsState::default(),
                        is_actions_focused: true,
                        destructive: false,
                        hint_text: Some(&[]),
                    },
                    |frame, body| {
                        let filler = "X".repeat(body.width as usize);
                        let lines: Vec<Line> = (0..body.height)
                            .map(|_| Line::from(filler.clone()))
                            .collect();
                        frame.render_widget(Paragraph::new(lines), body);
                    },
                );
            })
            .unwrap();

        let (confirm_btn, _) = modal_action_buttons(area, confirm_label, cancel_label);
        let buffer = terminal.backend().buffer();
        let separator_y = confirm_btn.y.saturating_sub(1);
        let separator: String = (area.x..area.x + area.width)
            .map(|x| buffer[(x, separator_y)].symbol())
            .collect();
        assert!(
            !separator.contains('X'),
            "separator row must stay blank: {separator:?}"
        );
    }
}
