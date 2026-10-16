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
            .constraints([Constraint::Min(2), Constraint::Length(3)])
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

        let hint = self
            .hint_text
            .unwrap_or("Tab / ← / → focus • Enter/Space execute • y / n fast select • Esc cancel");

        let actions_lines = vec![
            Line::from(vec![
                Span::styled(format!(" [ {} ] ", self.confirm_label), confirm_style),
                Span::raw("    "),
                Span::styled(format!(" [ {} ] ", self.cancel_label), cancel_style),
            ]),
            Line::from(Span::styled(hint, engine.fg(palette.text_secondary))),
        ];
        let actions_widget = Paragraph::new(actions_lines).alignment(Alignment::Center);
        frame.render_widget(actions_widget, chunks[1]);
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
    // The frame splits its interior into a flexible body and a fixed three-row
    // action bar; the first of those rows carries the buttons.
    let action_y = inner.y + inner.height.saturating_sub(3);

    let confirm_w = (confirm_label.chars().count() as u16).saturating_add(6);
    let cancel_w = (cancel_label.chars().count() as u16).saturating_add(6);
    let gap = 4u16;
    let total = confirm_w + gap + cancel_w;
    let start_x = inner.x + inner.width.saturating_sub(total) / 2;

    (
        Rect::new(start_x, action_y, confirm_w, 1),
        Rect::new(start_x + confirm_w + gap, action_y, cancel_w, 1),
    )
}
