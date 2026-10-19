use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::components::modal::{render_dialog_footer, render_modal_container, ModalTone};
use crate::i18n::active_t;
use crate::theme::engine::current_engine;
use crate::ui::centered_rect;
use crate::ui::header::truncate_to_width;

pub fn status_dialog_area(term: Rect, options_count: usize) -> Rect {
    let height = (options_count as u16 + 9).clamp(11, 18);
    let height_percent = (height * 100 / term.height.max(1)).clamp(30, 65);
    centered_rect(60, height_percent, term)
}

/// Fast-select hint rendered on the Apply button, matching the `1..N` digits
/// the dialog accepts for the current option count.
fn status_fast_select_label(options_count: usize) -> &'static str {
    if options_count >= 7 {
        "1-7"
    } else if options_count >= 6 {
        "1-6"
    } else if options_count >= 5 {
        "1-5"
    } else {
        "1-4"
    }
}

/// Apply button label as drawn by [`render_status_dialog`], including its
/// surrounding `[ ]` decoration.
fn status_apply_label(options_count: usize) -> String {
    format!(
        " [ {} (Enter / {}) ] ",
        active_t("dialog.button.apply"),
        status_fast_select_label(options_count)
    )
}

/// Cancel button label decoration, resolved from the locale.
fn status_cancel_label() -> String {
    format!(" [ {} (Esc) ] ", active_t("modal.cancel"))
}
/// Spacing drawn between the two action buttons.
const STATUS_BUTTON_GAP: &str = "    ";

/// Splits the dialog interior into `[header, options, buttons, footer]`.
///
/// Shared by the renderer and mouse hit-testing so the option rows and the
/// action buttons stay in lockstep and can never drift apart (Conventions §2).
/// The footer block holds the interactive buttons and the one-row docked hint
/// strip.
fn status_dialog_chunks(area: Rect, options_count: usize) -> [Rect; 4] {
    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(options_count as u16),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner);
    [chunks[0], chunks[1], chunks[2], chunks[3]]
}

/// Returns the apply and cancel button rectangles of the status dialog,
/// reproducing the centered action bar drawn by [`render_status_dialog`] so
/// mouse hit-testing cannot drift from the rendered buttons (Conventions §6).
pub fn status_dialog_buttons(term: Rect, options_count: usize) -> (Rect, Rect) {
    let area = status_dialog_area(term, options_count);
    let bar = status_dialog_chunks(area, options_count)[2];

    let apply_w = UnicodeWidthStr::width(status_apply_label(options_count).as_str()) as u16;
    let cancel_w = UnicodeWidthStr::width(status_cancel_label().as_str()) as u16;
    let gap_w = UnicodeWidthStr::width(STATUS_BUTTON_GAP) as u16;
    let total = apply_w + gap_w + cancel_w;
    let start_x = bar.x + bar.width.saturating_sub(total) / 2;

    (
        Rect::new(start_x, bar.y, apply_w, 1),
        Rect::new(start_x + apply_w + gap_w, bar.y, cancel_w, 1),
    )
}

/// Options area of the status dialog: the scrollable rows listing every
/// selectable status. Shared by rendering and mouse hit-testing so a click
/// resolves against the exact window the rows are drawn in (Conventions §6.6).
pub fn status_dialog_options_area(term: Rect, options_count: usize) -> Rect {
    let area = status_dialog_area(term, options_count);
    status_dialog_chunks(area, options_count)[1]
}

pub fn render_status_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::StatusDialogState,
) {
    let engine = current_engine();
    let palette = engine.palette();
    let options_count = dialog.target.options_count();
    let area = status_dialog_area(frame.area(), options_count);

    let (title, raw_target_title, current_idx, options) = match &dialog.target {
        crate::app::StatusDialogTarget::PlanItem { item_title, current_status, .. } => {
            let cur = match current_status {
                tauqe_protocol::PlanItemStatus::Discussion => 0,
                tauqe_protocol::PlanItemStatus::Todo => 1,
                tauqe_protocol::PlanItemStatus::InProgress => 2,
                tauqe_protocol::PlanItemStatus::Done => 3,
                tauqe_protocol::PlanItemStatus::Blocked => 4,
                tauqe_protocol::PlanItemStatus::Cancelled => 5,
            };
            let opts = vec![
                ("DISCUSSION", palette.button_active_bg, palette.text_inverse, active_t("dialog.status.option.discussion")),
                ("TODO", palette.status_warning, palette.text_inverse, active_t("dialog.status.plan.option.todo")),
                ("IN_PROGRESS", palette.status_info, palette.text_inverse, active_t("dialog.status.plan.option.in_progress")),
                ("DONE", palette.status_success, palette.text_inverse, active_t("dialog.status.plan.option.done")),
                ("BLOCKED", palette.status_error, palette.text_inverse, active_t("dialog.status.plan.option.blocked")),
                ("CANCELLED", palette.button_bg, palette.text_muted, active_t("dialog.status.plan.option.cancelled")),
            ];
            (active_t("dialog.status.plan.title"), item_title.as_str(), cur, opts)
        }
        crate::app::StatusDialogTarget::ReviewItem { item_title, current_status, .. } => {
            let cur = match current_status {
                tauqe_protocol::ReviewItemStatus::Discussion => 0,
                tauqe_protocol::ReviewItemStatus::Todo => 1,
                tauqe_protocol::ReviewItemStatus::InProgress => 2,
                tauqe_protocol::ReviewItemStatus::Refine => 3,
                tauqe_protocol::ReviewItemStatus::Fixed => 4,
                tauqe_protocol::ReviewItemStatus::Skipped => 5,
                tauqe_protocol::ReviewItemStatus::Rejected => 6,
            };
            let opts = vec![
                ("DISCUSSION", palette.button_active_bg, palette.text_inverse, active_t("dialog.status.option.discussion")),
                ("TODO", palette.status_warning, palette.text_inverse, active_t("dialog.status.review.option.todo")),
                ("IN_PROGRESS", palette.status_info, palette.text_inverse, active_t("dialog.status.review.option.in_progress")),
                ("REFINE", palette.status_error, palette.text_inverse, active_t("dialog.status.review.option.refine")),
                ("FIXED", palette.status_success, palette.text_inverse, active_t("dialog.status.review.option.fixed")),
                ("SKIPPED", palette.button_bg, palette.text_muted, active_t("dialog.status.review.option.skipped")),
                ("REJECTED", palette.button_bg, palette.text_muted, active_t("dialog.status.review.option.rejected")),
            ];
            (active_t("dialog.status.review.title"), item_title.as_str(), cur, opts)
        }
    };

    render_modal_container(frame, area, &title, ModalTone::Info);

    let chunks = status_dialog_chunks(area, options_count);

    let max_title_w = chunks[0].width.saturating_sub(10) as usize;
    let target_preview = truncate_to_width(raw_target_title.trim(), max_title_w);
    let target_line = Line::from(vec![
        Span::styled(active_t("dialog.status.item_label"), engine.fg(palette.text_muted)),
        Span::styled(target_preview, engine.fg(palette.text_primary).bold()),
    ]);
    frame.render_widget(Paragraph::new(vec![Line::raw(""), target_line, Line::raw("")]), chunks[0]);

    let mut body_lines = Vec::new();
    for (idx, (label, bg, fg, desc)) in options.iter().enumerate() {
        let is_cursor = idx == dialog.selector.selected_index();
        let is_current = idx == current_idx;

        let cursor_prefix = if is_cursor { " ▶ " } else { "   " };
        let current_marker = if is_current { "(•) " } else { "( ) " };
        let key_shortcut = format!("[{}] ", idx + 1);

        let row_style = if is_cursor {
            engine.bg(palette.overlay)
        } else {
            Style::default()
        };

        let badge_style = engine.bg(*bg).fg(engine.color(*fg)).bold();

        body_lines.push(Line::from(vec![
            Span::styled(cursor_prefix, engine.fg(palette.status_info).bold()),
            Span::styled(
                current_marker,
                if is_current {
                    engine.fg(palette.status_success).bold()
                } else {
                    engine.fg(palette.text_muted)
                },
            ),
            Span::styled(key_shortcut, engine.fg(palette.status_warning).bold()),
            Span::styled(format!(" {:<11} ", label), badge_style),
            Span::raw(" "),
            Span::styled(desc.clone(), if is_cursor { engine.fg(palette.text_primary) } else { engine.fg(palette.text_secondary) }),
        ]).style(row_style));
    }
    frame.render_widget(Paragraph::new(body_lines), chunks[1]);

    let actions = Line::from(vec![
        Span::styled(
            status_apply_label(options_count),
            engine.bg(palette.button_active_bg).fg(engine.color(palette.text_inverse)).bold(),
        ),
        Span::raw(STATUS_BUTTON_GAP),
        Span::styled(
            status_cancel_label(),
            engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)),
        ),
    ]);
    let actions_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(actions_widget, chunks[2]);

    // The navigation hint is docked to the card's bottom edge as its own strip
    // filled with `palette.overlay`, so it reads as persistent chrome instead
    // of a second line of buttons (c-1, c-2).
    let fast_select_label = status_fast_select_label(options_count);
    let footer_hint =
        active_t("dialog.status.fast_select_hint").replacen("{}", fast_select_label, 1);
    render_dialog_footer(frame, chunks[3], &footer_hint);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The review status dialog exposes the full seven-status lifecycle,
    /// including `REFINE`, and advertises the `1-7` fast-select range.
    #[test]
    fn review_status_dialog_renders_seven_options_including_refine() {
        let term = Rect::new(0, 0, 100, 30);
        let dialog = crate::app::StatusDialogState::new(
            crate::app::StatusDialogTarget::ReviewItem {
                review_id: Some("rev-1".to_string()),
                item_id: 1,
                item_title: "Finding".to_string(),
                current_status: tauqe_protocol::ReviewItemStatus::Todo,
            },
            0,
        );
        assert_eq!(dialog.target.options_count(), 7);

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|f| render_status_dialog(f, &dialog))
            .unwrap();
        let buffer = terminal.backend().buffer();

        let mut text = String::new();
        for y in 0..term.height {
            for x in 0..term.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("REFINE"), "expected the REFINE option: {text}");
        assert!(text.contains("FIXED"), "expected the FIXED option: {text}");
        assert!(text.contains("1-7"), "expected the 1-7 fast-select hint: {text}");

        let (apply_btn, cancel_btn) = status_dialog_buttons(term, 7);
        assert!(
            apply_btn.right() <= cancel_btn.x,
            "action buttons must not overlap"
        );
    }

    /// Mouse hit-testing must compute the same button rectangles the renderer
    /// draws for the status dialog action bar, so a click on Apply resolves to
    /// Apply instead of falling through to the dismiss path.
    #[test]
    fn status_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 30);
        let dialog = crate::app::StatusDialogState::new(
            crate::app::StatusDialogTarget::PlanItem {
                plan_id: "plan-1".to_string(),
                item_id: "1".to_string(),
                item_title: "Step".to_string(),
                current_status: tauqe_protocol::PlanItemStatus::Todo,
            },
            0,
        );
        let options_count = dialog.target.options_count();

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|f| render_status_dialog(f, &dialog))
            .unwrap();

        let (apply_btn, cancel_btn) = status_dialog_buttons(term, options_count);
        let buffer = terminal.backend().buffer();

        let apply_text: String = (apply_btn.x..apply_btn.x + apply_btn.width)
            .map(|x| buffer[(x, apply_btn.y)].symbol())
            .collect();
        assert!(
            apply_text.contains("Apply"),
            "apply row text: {apply_text:?}"
        );

        let cancel_text: String = (cancel_btn.x..cancel_btn.x + cancel_btn.width)
            .map(|x| buffer[(x, cancel_btn.y)].symbol())
            .collect();
        assert!(
            cancel_text.contains("Cancel"),
            "cancel row text: {cancel_text:?}"
        );

        assert!(
            apply_btn.right() <= cancel_btn.x,
            "action buttons must not overlap"
        );
    }

    /// The options area sits between the header and the action buttons, so a
    /// click mapped to it can never collide with the button row.
    #[test]
    fn status_options_area_sits_between_header_and_buttons() {
        let term = Rect::new(0, 0, 100, 30);
        let area = status_dialog_area(term, 6);
        let options = status_dialog_options_area(term, 6);
        let (apply_btn, _cancel_btn) = status_dialog_buttons(term, 6);

        assert!(options.y > area.y);
        assert_eq!(options.bottom(), apply_btn.y);
        assert!(options.height >= 1);
    }
}
