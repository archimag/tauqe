use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::components::modal::render_dialog_footer;
use crate::config::PrimaryModifier;
use crate::theme::engine::current_engine;
use crate::ui::centered_rect;
use super::editor_lines;

/// Width of the review launch dialog, as a percentage of the terminal width.
pub const REVIEW_DIALOG_WIDTH: u16 = 64;
/// Height of the review launch dialog, as a percentage of the terminal height.
pub const REVIEW_DIALOG_HEIGHT: u16 = 54;
/// Action-bar label for starting a review.
pub const REVIEW_DIALOG_CONFIRM_LABEL: &str = "[ Start Review (C-Enter) ]";
/// Action-bar label for cancelling the review dialog.
pub const REVIEW_DIALOG_CANCEL_LABEL: &str = "[ Cancel ]";

/// Height of the header block (summary and target metadata).
const REVIEW_HEADER_HEIGHT: u16 = 6;
/// Height of the footer block: the action bar (2 rows) plus the docked hint
/// strip (1 row).
const REVIEW_FOOTER_HEIGHT: u16 = 3;
/// Minimum height kept for the instruction editor on short terminals.
const REVIEW_EDITOR_MIN_HEIGHT: u16 = 3;
/// Maximum reviewer rows drawn before the list scrolls.
const REVIEW_MODEL_LIST_MAX_ROWS: u16 = 6;
/// Height of the target-paths field: a single-line input inside a border.
const REVIEW_PATH_HEIGHT: u16 = 3;

/// Number of reviewer rows drawn for `entry_count` entries, before borders.
fn review_model_list_rows(entry_count: usize) -> u16 {
    (entry_count.max(1) as u16).min(REVIEW_MODEL_LIST_MAX_ROWS)
}

/// Rectangles of the review dialog, shared by the renderer and the mouse
/// hit-tester so the surface that is drawn is the surface that is clicked
/// (Conventions §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewDialogGeometry {
    /// Whole dialog frame.
    pub area: Rect,
    /// Inner rows of the reviewer model list (one row per entry).
    pub model_rows: Rect,
    /// Confirm button.
    pub confirm_button: Rect,
    /// Cancel button.
    pub cancel_button: Rect,
}

/// Insets a rectangle by a one-cell border on every side.
fn inset(area: Rect) -> Rect {
    Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

/// Splits the dialog interior into
/// `[header, reviewer list, path field, editor, buttons, footer]`.
///
/// The action bar, list, path field, and header are sized first so the dialog
/// stays usable on short terminals; the instruction editor absorbs whatever
/// height remains. The path field keeps its reserved single-line height and the
/// header yields before it, so the target directory or glob stays visible. The
/// footer block splits into the interactive action bar and a one-row docked
/// hint strip pinned to the card's bottom edge.
fn review_dialog_chunks(area: Rect, entry_count: usize) -> [Rect; 6] {
    let inner = inset(area);

    let footer_h = REVIEW_FOOTER_HEIGHT.min(inner.height);
    let after_footer = inner.height - footer_h;

    // The list never takes more than half of the room left after the footer, so
    // the header and editors always keep space on short terminals.
    let list_h = (review_model_list_rows(entry_count) + 2).min(after_footer / 2);
    let after_list = after_footer - list_h;

    let path_h = REVIEW_PATH_HEIGHT.min(after_list.saturating_sub(REVIEW_EDITOR_MIN_HEIGHT));
    let after_path = after_list - path_h;

    let header_h = REVIEW_HEADER_HEIGHT.min(after_path.saturating_sub(REVIEW_EDITOR_MIN_HEIGHT));
    let editor_h = after_path - header_h;

    let header = Rect::new(inner.x, inner.y, inner.width, header_h);
    let list = Rect::new(inner.x, inner.y + header_h, inner.width, list_h);
    let path = Rect::new(inner.x, list.y + list_h, inner.width, path_h);
    let editor = Rect::new(inner.x, path.y + path_h, inner.width, editor_h);

    // Split the footer into the action buttons and the docked hint strip so the
    // hint reads as persistent chrome rather than a second row of buttons (c-1).
    let docked_h = 1.min(footer_h);
    let buttons_h = footer_h - docked_h;
    let buttons = Rect::new(inner.x, editor.y + editor_h, inner.width, buttons_h);
    let footer = Rect::new(inner.x, buttons.y + buttons_h, inner.width, docked_h);
    [header, list, path, editor, buttons, footer]
}

/// Computes the dialog frame, the reviewer-row area, and the action buttons
/// from the same layout the renderer uses.
pub fn review_dialog_geometry(term: Rect, entry_count: usize) -> ReviewDialogGeometry {
    let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, term);
    let chunks = review_dialog_chunks(area, entry_count);
    let model_rows = inset(chunks[1]);

    let buttons = chunks[4];
    let confirm_w = REVIEW_DIALOG_CONFIRM_LABEL.len() as u16 + 2;
    let cancel_w = REVIEW_DIALOG_CANCEL_LABEL.len() as u16 + 2;
    let gap = 4u16;
    let total = confirm_w + gap + cancel_w;
    let start_x = buttons.x + (buttons.width.saturating_sub(total)) / 2;
    let confirm_button = Rect::new(start_x, buttons.y, confirm_w, 1);
    let cancel_button = Rect::new(start_x + confirm_w + gap, buttons.y, cancel_w, 1);

    ReviewDialogGeometry {
        area,
        model_rows,
        confirm_button,
        cancel_button,
    }
}

pub fn render_review_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::ReviewDialogState,
    primary: PrimaryModifier,
) {
    let engine = current_engine();
    let palette = engine.palette();
    let entry_count = dialog.selector.len();
    let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, frame.area());
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(" Launch Code Review ")
        .borders(Borders::ALL)
        .border_style(engine.fg(palette.button_active_bg))
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(outer_block, area);

    let [header_area, list_area, path_area, editor_area, buttons_area, footer_area] =
        review_dialog_chunks(area, entry_count);

    let dim = engine.fg(palette.text_muted);

    // Header: what is about to be reviewed.
    let header_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Run automated code review on active context files?",
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  Files: ", dim),
            Span::styled(
                dialog.files_count.to_string(),
                engine.fg(palette.text_primary).bold(),
            ),
            Span::styled("    Estimated tokens: ", dim),
            Span::styled(
                format!("~{}", dialog.estimated_tokens),
                engine.fg(palette.text_primary).bold(),
            ),
        ]),
        Line::from(Span::styled(
            "  Only file contents are sent: history and repo map are excluded.",
            dim,
        )),
    ];
    frame.render_widget(
        Paragraph::new(header_lines).wrap(Wrap { trim: false }),
        header_area,
    );

    // Reviewer list. The shared `ModelSelector` owns the rows, numbering,
    // scrolling, and empty-list safety; the block adds the framing and the
    // focus color that shows which section owns the keyboard
    // (c-embedded-model-list, c-unified-dialog-integration).
    let list_title = if entry_count > 1 {
        format!(
            " Reviewer Model  [{} of {}] ",
            dialog.selector.selected_index() + 1,
            entry_count
        )
    } else {
        " Reviewer Model ".to_string()
    };
    let list_border = if dialog.focus.is_model_list() {
        engine.fg(palette.button_active_bg).bold()
    } else {
        engine.fg(palette.border)
    };
    let list_block = Block::default()
        .title(list_title)
        .borders(Borders::ALL)
        .border_style(list_border)
        .style(Style::default().bg(engine.color(palette.surface)));
    let list_inner = list_block.inner(list_area);
    frame.render_widget(list_block, list_area);
    if entry_count == 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  (no reviewer models configured)",
                dim,
            ))),
            list_inner,
        );
    } else {
        let rows = dialog.selector.render_list_lines(list_inner.height as usize);
        frame.render_widget(Paragraph::new(rows), list_inner);
    }

    // Target paths field. A single-line input that narrows the review to a
    // directory or glob without touching the User Context layer: the value rides
    // along in `ReviewStartParams::path_pattern` and the server merges the
    // matched files with the developer's layer (c-review-context-input). The
    // cursor is drawn only when the field owns focus.
    let show_path_cursor = dialog.focus.is_path_editor();
    let path_border = if show_path_cursor {
        engine.fg(palette.status_info).bold()
    } else {
        engine.fg(palette.border)
    };
    let mut path_content = Vec::new();
    if dialog.path_editor.is_empty() {
        let mut spans = vec![Span::raw("  > ")];
        if show_path_cursor {
            spans.push(Span::styled("█", engine.fg(palette.status_warning)));
        }
        spans.push(Span::styled(
            " target directory or glob, e.g. src/**/*.rs (optional)",
            dim,
        ));
        path_content.push(Line::from(spans));
    } else {
        for line in editor_lines(&dialog.path_editor, show_path_cursor) {
            let mut spans = vec![Span::raw("  > ")];
            spans.extend(line.spans);
            path_content.push(Line::from(spans));
        }
    }
    let path_block = Block::default()
        .title(" Target Paths (optional) ")
        .borders(Borders::ALL)
        .border_style(path_border)
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(
        Paragraph::new(path_content)
            .block(path_block)
            .wrap(Wrap { trim: false }),
        path_area,
    );

    // Instruction editor. The cursor is drawn only when the editor owns focus.
    let show_cursor = dialog.focus.is_prompt_editor();
    let editor_title_style = if show_cursor {
        engine.fg(palette.status_info).bold()
    } else {
        engine.fg(palette.border)
    };
    let mut editor_text = vec![Line::from(Span::styled(
        "  Additional review instructions (optional):",
        editor_title_style,
    ))];
    if dialog.prompt_editor.is_empty() {
        let mut spans = vec![Span::raw("  > ")];
        if show_cursor {
            spans.push(Span::styled("█", engine.fg(palette.status_warning)));
        }
        spans.push(Span::styled(
            " (type additional review focus or leave empty)...",
            dim,
        ));
        editor_text.push(Line::from(spans));
    } else {
        for line in editor_lines(&dialog.prompt_editor, show_cursor) {
            let mut spans = vec![Span::raw("  > ")];
            spans.extend(line.spans);
            editor_text.push(Line::from(spans));
        }
    }
    frame.render_widget(
        Paragraph::new(editor_text).wrap(Wrap { trim: false }),
        editor_area,
    );

    // Action bar with a focus-aware navigation hint.
    let in_actions = dialog.focus.is_actions();
    let confirm_selected = dialog.confirm_button == crate::app::ConfirmDialogButton::Confirm;
    let (confirm_style, cancel_style) = if in_actions {
        if confirm_selected {
            (
                engine.bg(palette.status_success).fg(engine.color(palette.text_inverse)).bold(),
                engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)),
            )
        } else {
            (
                engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)),
                engine.bg(palette.button_active_bg).fg(engine.color(palette.text_inverse)).bold(),
            )
        }
    } else {
        (
            engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)),
            engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)),
        )
    };
    let nav_hint = if dialog.focus.is_actions() {
        "Enter confirm • ← / → button • Esc cancel".to_string()
    } else {
        format!(
            "{}Enter start • Tab focus • {} model • Esc cancel",
            primary.prefix_str(),
            crate::components::model_selector::model_select_hint(primary)
        )
    };
    let actions = Line::from(vec![
        Span::styled(format!(" {} ", REVIEW_DIALOG_CONFIRM_LABEL), confirm_style),
        Span::raw("    "),
        Span::styled(format!(" {} ", REVIEW_DIALOG_CANCEL_LABEL), cancel_style),
    ]);
    frame.render_widget(
        Paragraph::new(actions).alignment(Alignment::Center),
        buttons_area,
    );

    // The navigation hint is docked to the card's bottom edge as its own strip
    // filled with `palette.overlay`, so it reads as persistent chrome instead
    // of a second line of buttons (c-1, c-2).
    render_dialog_footer(frame, footer_area, &nav_hint);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_dialog_geometry_buttons_within_area() {
        let term = Rect::new(0, 0, 120, 40);
        let geo = review_dialog_geometry(term, 3);
        assert!(geo.area.width > 0 && geo.area.height > 0);
        assert!(geo.confirm_button.width > 0 && geo.cancel_button.width > 0);
        assert_eq!(geo.confirm_button.y, geo.cancel_button.y);
        assert!(geo.confirm_button.right() <= geo.area.right());
        assert!(geo.cancel_button.right() <= geo.area.right());
        assert!(geo.confirm_button.right() < geo.cancel_button.x);

        assert!(geo.model_rows.y >= geo.area.y);
        assert!(geo.model_rows.y + geo.model_rows.height <= geo.area.bottom());
        assert_eq!(geo.model_rows.height, 3, "one row per reviewer entry");
    }

    #[test]
    fn test_review_dialog_chunks_reserve_path_field_between_list_and_editor() {
        let term = Rect::new(0, 0, 120, 40);
        let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, term);
        let [header, list, path, editor, buttons, footer] = review_dialog_chunks(area, 3);

        // The reserved single-line path field is fully visible on a normal terminal.
        assert_eq!(path.height, REVIEW_PATH_HEIGHT);

        // The interior stacks top-to-bottom without gaps or overlap, and the
        // footer block ends in a one-row docked hint strip.
        assert_eq!(header.y, area.y + 1);
        assert_eq!(list.y, header.y + header.height);
        assert_eq!(path.y, list.y + list.height);
        assert_eq!(editor.y, path.y + path.height);
        assert_eq!(buttons.y, editor.y + editor.height);
        assert_eq!(footer.y, buttons.y + buttons.height);
        assert!(footer.y + footer.height <= area.bottom());
        assert!(path.width > 0 && editor.height > 0);
        assert_eq!(footer.height, 1, "docked hint strip is one row");
    }
}
