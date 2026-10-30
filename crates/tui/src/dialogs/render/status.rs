use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use tauqe_protocol::{PlanItemStatus, ReviewItemStatus};

use crate::components::modal::{
    modal_geometry, render_dialog_footer, render_modal_container, verb_hint, ModalTone,
};
use crate::i18n::active_t;
use crate::theme::color::RgbColor;
use crate::theme::engine::current_engine;
use crate::theme::palette::ThemePalette;
use crate::ui::header::truncate_to_width;

/// Card rows the shared modal frame reserves around the body: the two border
/// rows, the top inset, the pre-action spacer, the action bar, and the docked
/// hint strip (Conventions §6.7).
const STATUS_CARD_CHROME_ROWS: u16 = 6;

/// Body rows outside the option list: the target item line and the one blank
/// inter-section spacer under it (Conventions §6.7).
const STATUS_BODY_CHROME_ROWS: u16 = 2;

/// Spacing drawn between the two action buttons.
const STATUS_BUTTON_GAP: &str = "    ";

/// Ordered plan status cycle presented by the dialog.
///
/// The rows are drawn from this list and a fast-select digit maps to its
/// position, so a status can never be applied at a position other than the one
/// it is rendered at (Conventions §1.3).
pub(crate) const PLAN_STATUS_ORDER: [PlanItemStatus; 6] = [
    PlanItemStatus::Discussion,
    PlanItemStatus::Todo,
    PlanItemStatus::InProgress,
    PlanItemStatus::Done,
    PlanItemStatus::Blocked,
    PlanItemStatus::Cancelled,
];

/// Ordered review status cycle presented by the dialog.
pub(crate) const REVIEW_STATUS_ORDER: [ReviewItemStatus; 6] = [
    ReviewItemStatus::Discussion,
    ReviewItemStatus::Todo,
    ReviewItemStatus::InProgress,
    ReviewItemStatus::Fixed,
    ReviewItemStatus::Skipped,
    ReviewItemStatus::Rejected,
];

/// One rendered status option: the badge text, its palette roles, and the
/// localized description.
struct StatusOption {
    badge: &'static str,
    bg: RgbColor,
    fg: RgbColor,
    description: String,
}

/// Presentation of a plan status row.
fn plan_status_option(status: PlanItemStatus, palette: &ThemePalette) -> StatusOption {
    let (badge, bg, fg, key) = match status {
        PlanItemStatus::Discussion => (
            "DISCUSSION",
            palette.button_active_bg,
            palette.text_inverse,
            "dialog.status.option.discussion",
        ),
        PlanItemStatus::Todo => (
            "TODO",
            palette.status_warning,
            palette.text_inverse,
            "dialog.status.plan.option.todo",
        ),
        PlanItemStatus::InProgress => (
            "IN_PROGRESS",
            palette.status_info,
            palette.text_inverse,
            "dialog.status.plan.option.in_progress",
        ),
        PlanItemStatus::Done => (
            "DONE",
            palette.status_success,
            palette.text_inverse,
            "dialog.status.plan.option.done",
        ),
        PlanItemStatus::Blocked => (
            "BLOCKED",
            palette.status_error,
            palette.text_inverse,
            "dialog.status.plan.option.blocked",
        ),
        PlanItemStatus::Cancelled => (
            "CANCELLED",
            palette.button_bg,
            palette.text_muted,
            "dialog.status.plan.option.cancelled",
        ),
    };
    StatusOption {
        badge,
        bg,
        fg,
        description: active_t(key),
    }
}

/// Presentation of a review status row.
fn review_status_option(status: ReviewItemStatus, palette: &ThemePalette) -> StatusOption {
    let (badge, bg, fg, key) = match status {
        ReviewItemStatus::Discussion => (
            "DISCUSSION",
            palette.button_active_bg,
            palette.text_inverse,
            "dialog.status.option.discussion",
        ),
        ReviewItemStatus::Todo => (
            "TODO",
            palette.status_warning,
            palette.text_inverse,
            "dialog.status.review.option.todo",
        ),
        ReviewItemStatus::InProgress => (
            "IN_PROGRESS",
            palette.status_info,
            palette.text_inverse,
            "dialog.status.review.option.in_progress",
        ),
        ReviewItemStatus::Fixed => (
            "FIXED",
            palette.status_success,
            palette.text_inverse,
            "dialog.status.review.option.fixed",
        ),
        ReviewItemStatus::Skipped => (
            "SKIPPED",
            palette.button_bg,
            palette.text_muted,
            "dialog.status.review.option.skipped",
        ),
        ReviewItemStatus::Rejected => (
            "REJECTED",
            palette.button_bg,
            palette.text_muted,
            "dialog.status.review.option.rejected",
        ),
    };
    StatusOption {
        badge,
        bg,
        fg,
        description: active_t(key),
    }
}

/// Single source of truth for the status dialog card, shared by rendering and
/// mouse hit-testing.
///
/// The height budget is exactly the chrome the shared frame owns plus the target
/// line, its spacer, and one row per option, so a card never clips an option and
/// never collapses one of its air rows (Conventions §6.6–§6.8).
pub fn status_dialog_area(term: Rect, options_count: usize) -> Rect {
    let options = u16::try_from(options_count).unwrap_or(u16::MAX);
    let height = options
        .saturating_add(STATUS_BODY_CHROME_ROWS)
        .saturating_add(STATUS_CARD_CHROME_ROWS)
        .clamp(11, 18)
        .min(term.height);
    let width = (term.width.saturating_mul(60) / 100).min(term.width);
    Rect::new(
        term.x + term.width.saturating_sub(width) / 2,
        term.y + term.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

/// Vertical bands of the status dialog body.
struct StatusBody {
    /// Single row carrying the target item line.
    target: Rect,
    /// Selectable status rows, reached through exactly one blank spacer.
    options: Rect,
}

/// Splits the air-aware dialog body into the target line and the option list.
///
/// The spacer row belongs to the body renderer because only it knows where its
/// own sections begin and end (Conventions §6.7).
fn status_body(body: Rect) -> StatusBody {
    let target_h = body.height.min(1);
    let spacer_h = body.height.saturating_sub(target_h).min(1);
    let options_y = body.y.saturating_add(target_h).saturating_add(spacer_h);
    StatusBody {
        target: Rect::new(body.x, body.y, body.width, target_h),
        options: Rect::new(
            body.x,
            options_y,
            body.width,
            body.height
                .saturating_sub(target_h)
                .saturating_sub(spacer_h),
        ),
    }
}

/// Fast-select hint rendered on the Apply button, matching the `1..N` digits
/// the dialog accepts for the current option count.
fn status_fast_select_label(options_count: usize) -> &'static str {
    if options_count >= 6 {
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

/// Returns the apply and cancel button rectangles of the status dialog.
///
/// Both derive from the shared action-bar band, so the buttons that are drawn
/// are always the buttons that are clickable (Conventions §6.6).
pub fn status_dialog_buttons(term: Rect, options_count: usize) -> (Rect, Rect) {
    status_buttons_in(
        modal_geometry(status_dialog_area(term, options_count)).action_bar,
        options_count,
    )
}

/// Centers the drawn localized button labels inside an action-bar row.
fn status_buttons_in(action_bar: Rect, options_count: usize) -> (Rect, Rect) {
    let apply_w = UnicodeWidthStr::width(status_apply_label(options_count).as_str()) as u16;
    let cancel_w = UnicodeWidthStr::width(status_cancel_label().as_str()) as u16;
    let gap_w = UnicodeWidthStr::width(STATUS_BUTTON_GAP) as u16;
    let total = apply_w + gap_w + cancel_w;
    let start_x = action_bar.x + action_bar.width.saturating_sub(total) / 2;

    (
        Rect::new(start_x, action_bar.y, apply_w, 1),
        Rect::new(start_x + apply_w + gap_w, action_bar.y, cancel_w, 1),
    )
}

/// Options area of the status dialog: the scrollable rows listing every
/// selectable status. Shared by rendering and mouse hit-testing so a click
/// resolves against the exact window the rows are drawn in (Conventions §6.6).
pub fn status_dialog_options_area(term: Rect, options_count: usize) -> Rect {
    status_body(modal_geometry(status_dialog_area(term, options_count)).body).options
}

pub fn render_status_dialog(frame: &mut ratatui::Frame, dialog: &crate::app::StatusDialogState) {
    let engine = current_engine();
    let palette = engine.palette();
    let options_count = dialog.target.options_count();
    let area = status_dialog_area(frame.area(), options_count);

    let (title, raw_target_title, current_idx, options) = match &dialog.target {
        crate::app::StatusDialogTarget::PlanItem {
            item_title,
            current_status,
            ..
        } => (
            active_t("dialog.status.plan.title"),
            item_title.as_str(),
            PLAN_STATUS_ORDER
                .iter()
                .position(|status| status == current_status)
                .unwrap_or(0),
            PLAN_STATUS_ORDER
                .iter()
                .map(|status| plan_status_option(*status, &palette))
                .collect::<Vec<_>>(),
        ),
        crate::app::StatusDialogTarget::ReviewItem {
            item_title,
            current_status,
            ..
        } => (
            active_t("dialog.status.review.title"),
            item_title.as_str(),
            REVIEW_STATUS_ORDER
                .iter()
                .position(|status| status == current_status)
                .unwrap_or(0),
            REVIEW_STATUS_ORDER
                .iter()
                .map(|status| review_status_option(*status, &palette))
                .collect::<Vec<_>>(),
        ),
    };

    render_modal_container(frame, area, &title, ModalTone::Info);

    // The shared frame owns the vertical air (top inset, pre-action spacer,
    // docked hint strip) and the horizontal padding, so the card breathes like
    // every other dialog and its geometry stays the single source of truth for
    // mouse hit-testing (Conventions §6.6–§6.8).
    let geometry = modal_geometry(area);
    let body = status_body(geometry.body);

    let item_label = active_t("dialog.status.item_label");
    let label_width = u16::try_from(UnicodeWidthStr::width(item_label.as_str())).unwrap_or(0);
    let max_title_w = usize::from(body.target.width.saturating_sub(label_width));
    let target_preview = truncate_to_width(raw_target_title.trim(), max_title_w);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(item_label, engine.fg(palette.text_muted)),
            Span::styled(target_preview, engine.fg(palette.text_primary).bold()),
        ])),
        body.target,
    );

    let mut option_lines = Vec::new();
    for (idx, option) in options.iter().enumerate() {
        let is_cursor = idx == dialog.selector.selected_index();
        let is_current = idx == current_idx;

        // The marker carries its own leading space, so no row presses against
        // the card border (Conventions §6.8).
        let cursor_prefix = if is_cursor { " ▶ " } else { "   " };
        let current_marker = if is_current { "(•) " } else { "( ) " };
        let key_shortcut = format!("[{}] ", idx + 1);

        let row_style = if is_cursor {
            engine.bg(palette.overlay)
        } else {
            Style::default()
        };

        let badge_style = engine.bg(option.bg).fg(engine.color(option.fg)).bold();

        option_lines.push(
            Line::from(vec![
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
                Span::styled(format!(" {:<11} ", option.badge), badge_style),
                Span::raw(" "),
                Span::styled(
                    option.description.clone(),
                    if is_cursor {
                        engine.fg(palette.text_primary)
                    } else {
                        engine.fg(palette.text_secondary)
                    },
                ),
            ])
            .style(row_style),
        );
    }
    frame.render_widget(Paragraph::new(option_lines), body.options);

    let (apply_btn, cancel_btn) = status_buttons_in(geometry.action_bar, options_count);
    frame.render_widget(
        Paragraph::new(Span::styled(
            status_apply_label(options_count),
            engine
                .bg(palette.button_active_bg)
                .fg(engine.color(palette.text_inverse))
                .bold(),
        )),
        apply_btn,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            status_cancel_label(),
            engine
                .bg(palette.button_bg)
                .fg(engine.color(palette.button_fg)),
        )),
        cancel_btn,
    );

    // The navigation hint is docked to the card's bottom edge as its own strip
    // filled with `palette.overlay`, so it reads as persistent chrome instead
    // of a second line of buttons (c-1, c-2).
    let fast_select_label = status_fast_select_label(options_count);
    let footer_hint =
        active_t("dialog.status.fast_select_hint").replacen("{}", fast_select_label, 1);
    render_dialog_footer(frame, geometry.hint_row, &verb_hint(footer_hint));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The review status dialog exposes the full six-status lifecycle and
    /// advertises the `1-6` fast-select range.
    #[test]
    fn review_status_dialog_renders_six_options_without_refine() {
        let term = Rect::new(0, 0, 100, 30);
        let dialog = crate::app::StatusDialogState::new(
            crate::app::StatusDialogTarget::ReviewItem {
                review_id: Some("rev-1".to_string()),
                item_id: 1,
                item_title: "Finding".to_string(),
                current_status: ReviewItemStatus::Todo,
            },
            0,
        );
        assert_eq!(dialog.target.options_count(), 6);

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal.draw(|f| render_status_dialog(f, &dialog)).unwrap();
        let buffer = terminal.backend().buffer();

        let mut text = String::new();
        for y in 0..term.height {
            for x in 0..term.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(
            text.contains("IN_PROGRESS"),
            "expected the IN_PROGRESS option: {text}"
        );
        assert!(text.contains("FIXED"), "expected the FIXED option: {text}");
        assert!(
            !text.contains("REFINE"),
            "the retired REFINE option must never be offered: {text}"
        );
        assert!(
            text.contains("1-6"),
            "expected the 1-6 fast-select hint: {text}"
        );

        let (apply_btn, cancel_btn) = status_dialog_buttons(term, 6);
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
                current_status: PlanItemStatus::Todo,
            },
            0,
        );
        let options_count = dialog.target.options_count();

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal.draw(|f| render_status_dialog(f, &dialog)).unwrap();

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

    /// The options area sits between the target line and the action buttons and
    /// keeps exactly one blank row above the buttons.
    #[test]
    fn status_options_area_sits_between_target_and_buttons() {
        let term = Rect::new(0, 0, 100, 30);
        let area = status_dialog_area(term, 6);
        let options = status_dialog_options_area(term, 6);
        let (apply_btn, _cancel_btn) = status_dialog_buttons(term, 6);

        assert!(options.y > area.y);
        assert_eq!(options.bottom() + 1, apply_btn.y, "pre-action spacer");
        assert_eq!(options.height, 6);
    }

    /// The card keeps one blank row under the border title, one blank row
    /// between the target item and the options, and one column of air on each
    /// side (Conventions §6.7, §6.8).
    #[test]
    fn status_body_keeps_air_between_its_sections() {
        let term = Rect::new(0, 0, 100, 30);
        let area = status_dialog_area(term, 6);
        let geometry = modal_geometry(area);
        let body = status_body(geometry.body);

        assert_eq!(body.target.y, area.y + 2, "border + top inset");
        assert_eq!(body.options.y, body.target.y + 2, "inter-section spacer");
        assert_eq!(body.options.x, area.x + 2, "border + horizontal padding");
        assert_eq!(body.options.width, area.width - 4);
        assert_eq!(
            geometry.action_bar.y,
            geometry.body.y + geometry.body.height + 1,
            "pre-action spacer"
        );
    }

    /// Reads the interior columns of a card row, skipping the two vertical
    /// border glyphs, so a blank interior row reads as empty text instead of a
    /// pair of frame characters (Conventions §6.8).
    fn interior_row(terminal: &Terminal<TestBackend>, area: Rect, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        let start = area.x.saturating_add(1);
        let end = area.x.saturating_add(area.width).saturating_sub(1);
        (start..end).map(|x| buffer[(x, y)].symbol()).collect()
    }

    /// Every air row is drawn blank while its neighbouring content rows are
    /// not, so the dialog never presses text against its title or its buttons
    /// (Conventions §6.7).
    #[test]
    fn status_dialog_draws_its_air_rows_blank() {
        let term = Rect::new(0, 0, 100, 30);
        let dialog = crate::app::StatusDialogState::new(
            crate::app::StatusDialogTarget::PlanItem {
                plan_id: "plan-1".to_string(),
                item_id: "1".to_string(),
                item_title: "Step".to_string(),
                current_status: PlanItemStatus::Todo,
            },
            0,
        );

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal.draw(|f| render_status_dialog(f, &dialog)).unwrap();

        let area = status_dialog_area(term, 6);
        let geometry = modal_geometry(area);
        let body = status_body(geometry.body);

        assert!(
            interior_row(&terminal, area, area.y + 1).trim().is_empty(),
            "top inset must stay blank"
        );
        assert!(
            !interior_row(&terminal, area, body.target.y)
                .trim()
                .is_empty(),
            "the target line must be drawn under the top inset"
        );
        assert!(
            interior_row(&terminal, area, body.target.y + 1)
                .trim()
                .is_empty(),
            "the inter-section spacer must stay blank"
        );
        assert!(
            !interior_row(&terminal, area, body.options.y)
                .trim()
                .is_empty(),
            "the first option must be drawn under the spacer"
        );
        assert!(
            interior_row(&terminal, area, geometry.action_bar.y - 1)
                .trim()
                .is_empty(),
            "the pre-action spacer must stay blank"
        );
        assert!(
            !interior_row(&terminal, area, geometry.action_bar.y)
                .trim()
                .is_empty(),
            "the action bar must sit directly under the spacer"
        );
    }
}
