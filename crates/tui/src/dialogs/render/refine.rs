//! Rendering for the two-phase plan refinement dialog and its blocked-state
//! popup.

use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};

use crate::app::ConfirmDialogButton;
use crate::components::model_selector::{model_select_hint, ModelSelector};
use crate::config::PrimaryModifier;
use crate::i18n::active_t;
use crate::theme::engine::current_engine;

use super::plan_form::{
    plan_form_area, plan_form_geometry, render_plan_form, PlanFormGeometry, PlanFormView,
};
use super::{render_modal_confirm, ConfirmModalParams, ModalTone};
/// Width of the refine dialog, as a percentage of the terminal width.
pub const REFINE_DIALOG_WIDTH: u16 = 72;
/// Height of the refine dialog, as a percentage of the terminal height.
pub const REFINE_DIALOG_HEIGHT: u16 = 72;
/// Header rows: the plan and the two-phase synthesis note.
const REFINE_HEADER_HEIGHT: u16 = 3;

/// Full drawn confirm-button label, resolved from the runtime locale.
///
/// Rendering and mouse hit-testing both measure this label, so the clickable
/// button can never drift from the drawn one (Conventions §6, §9, c-1).
fn refine_confirm_label() -> String {
    format!(" [ {} ] ", active_t("modal.start_refinement"))
}

/// Full drawn cancel-button label, resolved from the runtime locale.
fn refine_cancel_label() -> String {
    format!(" [ {} ] ", active_t("modal.cancel"))
}

/// Ladder title naming the selected rung and its position.
fn ladder_title(base_key: &str, counter_key: &str, selector: &ModelSelector) -> String {
    if selector.len() > 1 {
        active_t(counter_key)
            .replacen("{}", &(selector.selected_index() + 1).to_string(), 1)
            .replacen("{}", &selector.len().to_string(), 1)
    } else {
        active_t(base_key)
    }
}

/// Critic title carrying the state of the independent audit phase.
fn critic_title(dialog: &crate::views::plans::PlanRefineDialogState) -> String {
    let mut title = ladder_title(
        "dialog.refine.critic_list_title",
        "dialog.refine.critic_list_counter",
        &dialog.critic,
    );
    if dialog.skip_critique {
        title.push_str(&active_t("dialog.plan_form.critic_skipped"));
        title.push(' ');
    }
    title
}

/// Rectangles of the refine dialog, shared by the renderer and the mouse
/// hit-tester so the surface that is drawn is the surface that is clicked
/// (Conventions §6.6–§6.8).
pub(crate) fn refine_dialog_geometry(
    term: Rect,
    planner_count: usize,
    critic_count: usize,
) -> PlanFormGeometry {
    let area = plan_form_area(term, REFINE_DIALOG_WIDTH, REFINE_DIALOG_HEIGHT);
    plan_form_geometry(
        area,
        planner_count,
        critic_count,
        REFINE_HEADER_HEIGHT,
        &refine_confirm_label(),
        &refine_cancel_label(),
    )
}

/// Renders the plan refinement form: the plan being modified, the Planner and
/// Critic ladders, the instructions editor, and the action bar.
pub fn render_plan_refine_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::views::plans::PlanRefineDialogState,
    primary: PrimaryModifier,
) {
    let engine = current_engine();
    let palette = engine.palette();
    let dim = engine.fg(palette.text_muted);
    let area = plan_form_area(frame.area(), REFINE_DIALOG_WIDTH, REFINE_DIALOG_HEIGHT);

    let header_lines = vec![
        Line::from(vec![
            Span::styled(active_t("dialog.refine.plan_label"), dim),
            Span::styled(
                dialog.plan_title.clone(),
                engine.fg(palette.text_primary).bold(),
            ),
            Span::styled(format!(" [{}]", dialog.plan_id), dim),
        ]),
        Line::from(Span::styled(active_t("dialog.refine.header_note"), dim)),
    ];

    let hint = if dialog.focus.is_actions() {
        active_t("dialog.refine.hint_actions")
    } else {
        active_t("dialog.refine.hint_fields")
            .replacen("{}", primary.prefix_str(), 1)
            .replacen("{}", &model_select_hint(primary), 1)
    };

    render_plan_form(
        frame,
        &PlanFormView {
            area,
            title: active_t("dialog.refine.title"),
            header_height: REFINE_HEADER_HEIGHT,
            header_lines,
            planner_title: ladder_title(
                "dialog.refine.model_list_title",
                "dialog.refine.model_list_counter",
                &dialog.planner,
            ),
            planner: &dialog.planner,
            planner_focused: dialog.focus.is_planner_list(),
            critic_title: critic_title(dialog),
            critic: &dialog.critic,
            critic_focused: dialog.focus.is_critic_list(),
            editor_title: active_t("dialog.refine.editor_title"),
            editor_placeholder: active_t("dialog.refine.editor_placeholder"),
            editor: &dialog.instructions_editor,
            editor_focused: dialog.focus.is_instructions_editor(),
            confirm_label: refine_confirm_label(),
            cancel_label: refine_cancel_label(),
            confirm_selected: dialog.confirm_button == ConfirmDialogButton::Confirm,
            actions_focused: dialog.focus.is_actions(),
            hint,
        },
    );
}

/// Presentation parameters for the "refinement blocked" modal, shared by the
/// renderer and mouse hit-testing so the drawn action buttons are always the
/// clickable ones (Conventions §6, §7).
pub fn refine_blocked_params<'a>(
    blocked: &'a crate::views::plans::PlanRefineBlockedState,
    confirm_button: crate::app::ConfirmDialogButton,
) -> ConfirmModalParams<'a> {
    let engine = current_engine();
    let palette = engine.palette();
    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.refine_blocked.heading"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                active_t("dialog.refine_blocked.plan_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(
                format!("{} [{}]", blocked.plan_title, blocked.plan_id),
                engine.fg(palette.text_primary).bold(),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.refine_blocked.reason_label"),
            engine.fg(palette.status_error).bold(),
        )),
    ];

    for line in blocked.reason.lines() {
        body_lines.push(Line::from(Span::styled(
            format!("  {}", line),
            engine.fg(palette.text_primary),
        )));
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        active_t("dialog.refine_blocked.note"),
        engine.fg(palette.text_muted),
    )));

    ConfirmModalParams {
        title: active_t("dialog.refine_blocked.title"),
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: active_t("dialog.button.replan_horizon"),
        cancel_label: active_t("dialog.button.dismiss"),
        confirm_button,
        destructive: false,
        width: 70,
    }
}

pub fn render_plan_refine_blocked_dialog(
    frame: &mut ratatui::Frame,
    blocked: &crate::views::plans::PlanRefineBlockedState,
    confirm_button: crate::app::ConfirmDialogButton,
) {
    render_modal_confirm(frame, refine_blocked_params(blocked, confirm_button));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_refine_dialog_geometry_buttons_within_area() {
        let term = Rect::new(0, 0, 120, 50);
        let geo = refine_dialog_geometry(term, 3, 2);
        assert!(geo.area.width > 0 && geo.area.height > 0);
        assert!(geo.confirm_button.width > 0 && geo.cancel_button.width > 0);
        assert_eq!(geo.confirm_button.y, geo.cancel_button.y);
        assert!(geo.confirm_button.right() <= geo.area.right());
        assert!(geo.cancel_button.right() <= geo.area.right());
        assert!(geo.confirm_button.right() < geo.cancel_button.x);

        assert_eq!(geo.planner_rows.height, 3, "one row per Planner entry");
        assert_eq!(geo.critic_rows.height, 2, "one row per Critic entry");
        assert!(geo.planner_rows.y > geo.header.y);
        assert!(geo.critic_rows.y > geo.planner.bottom());
        assert!(geo.editor_area.y > geo.critic.bottom());
    }

    /// The two Architect ladders and the instructions editor are distinct
    /// zones: a blank row sits under the header, between the ladders, and above
    /// the action bar (Conventions §6.7, §6.8).
    #[test]
    fn refine_dialog_sections_are_separated_by_blank_rows() {
        let term = Rect::new(0, 0, 120, 50);
        let area = plan_form_area(term, REFINE_DIALOG_WIDTH, REFINE_DIALOG_HEIGHT);
        let chunks = plan_form_geometry(
            area,
            3,
            2,
            REFINE_HEADER_HEIGHT,
            " [ Start ] ",
            " [ Cancel ] ",
        );

        assert_eq!(chunks.header.y, area.y + 2, "border + top inset");
        assert_eq!(chunks.header.x, area.x + 2, "border + horizontal padding");
        assert_eq!(chunks.header.width, area.width - 4);
        assert_eq!(chunks.planner.y, chunks.header.y + chunks.header.height + 1);
        assert_eq!(
            chunks.critic.y,
            chunks.planner.y + chunks.planner.height + 1
        );
        assert_eq!(
            chunks.editor_area.y,
            chunks.critic.y + chunks.critic.height + 1,
            "pre-editor spacer"
        );
        assert!(chunks.editor_area.height >= 3);
    }
}
