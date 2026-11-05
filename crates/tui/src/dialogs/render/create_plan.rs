//! Plan creation dialog: the two-phase Architect pipeline form.
//!
//! The form asks for the engineering directives and for the two Architects that
//! run the synthesis: the Planner drafts the plan, the Critic audits it
//! independently, and the Planner reconciles the critique into the issued
//! structure. Drawing it through the shared plan-form renderer keeps it
//! identical to the refinement form (c-typed-plan-creation-dialog).

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::app::ConfirmDialogButton;
use crate::components::model_selector::{model_select_hint, ModelSelector};
use crate::config::PrimaryModifier;
use crate::i18n::active_t;
use crate::theme::engine::current_engine;
use crate::views::plans::PlanCreateDialogState;

use super::plan_form::{
    plan_form_area, plan_form_geometry, render_plan_form, PlanFormGeometry, PlanFormView,
};

/// Width of the creation dialog, as a percentage of the terminal.
const CREATE_PLAN_DIALOG_WIDTH: u16 = 72;
/// Height of the creation dialog, as a percentage of the terminal.
const CREATE_PLAN_DIALOG_HEIGHT: u16 = 72;
/// Header rows: the pipeline note and the two synthesis phases.
const CREATE_PLAN_HEADER_HEIGHT: u16 = 3;

/// Full drawn confirm-button label, resolved from the runtime locale.
fn confirm_label() -> String {
    format!(" [ {} ] ", active_t("modal.create_plan"))
}

/// Full drawn cancel-button label, resolved from the runtime locale.
fn cancel_label() -> String {
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
fn critic_title(dialog: &PlanCreateDialogState) -> String {
    let mut title = ladder_title(
        "dialog.create_plan.critic_list_title",
        "dialog.create_plan.critic_list_counter",
        &dialog.critic,
    );
    if dialog.skip_critique {
        title.push_str(&active_t("dialog.plan_form.critic_skipped"));
        title.push(' ');
    }
    title
}

/// Rectangles of the creation dialog, shared by the renderer and the mouse
/// hit-tester so the drawn buttons are always the clickable ones
/// (Conventions §6.6).
pub(crate) fn create_plan_dialog_geometry(
    term: Rect,
    planner_count: usize,
    critic_count: usize,
) -> PlanFormGeometry {
    let area = plan_form_area(term, CREATE_PLAN_DIALOG_WIDTH, CREATE_PLAN_DIALOG_HEIGHT);
    plan_form_geometry(
        area,
        planner_count,
        critic_count,
        CREATE_PLAN_HEADER_HEIGHT,
        &confirm_label(),
        &cancel_label(),
    )
}

pub(crate) fn render_plan_create_dialog(
    frame: &mut Frame,
    dialog: &PlanCreateDialogState,
    primary: PrimaryModifier,
) {
    let engine = current_engine();
    let palette = engine.palette();
    let dim = engine.fg(palette.text_muted);
    let area = plan_form_area(
        frame.area(),
        CREATE_PLAN_DIALOG_WIDTH,
        CREATE_PLAN_DIALOG_HEIGHT,
    );

    let header_lines = vec![
        Line::from(Span::styled(
            active_t("dialog.create_plan.header_note"),
            dim,
        )),
        Line::from(Span::styled(
            active_t("dialog.create_plan.header_phases"),
            dim,
        )),
    ];

    let hint = if dialog.focus.is_actions() {
        active_t("dialog.create_plan.hint_actions")
    } else {
        active_t("dialog.create_plan.hint_fields")
            .replacen("{}", primary.prefix_str(), 1)
            .replacen("{}", &model_select_hint(primary), 1)
    };

    render_plan_form(
        frame,
        &PlanFormView {
            area,
            title: active_t("dialog.create_plan.title"),
            header_height: CREATE_PLAN_HEADER_HEIGHT,
            header_lines,
            planner_title: ladder_title(
                "dialog.create_plan.planner_list_title",
                "dialog.create_plan.planner_list_counter",
                &dialog.planner,
            ),
            planner: &dialog.planner,
            planner_focused: dialog.focus.is_planner_list(),
            critic_title: critic_title(dialog),
            critic: &dialog.critic,
            critic_focused: dialog.focus.is_critic_list(),
            editor_title: active_t("dialog.create_plan.editor_title"),
            editor_placeholder: active_t("dialog.create_plan.editor_placeholder"),
            editor: &dialog.directives_editor,
            editor_focused: dialog.focus.is_instructions_editor(),
            confirm_label: confirm_label(),
            cancel_label: cancel_label(),
            confirm_selected: dialog.confirm_button == ConfirmDialogButton::Confirm,
            actions_focused: dialog.focus.is_actions(),
            hint,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The creation card stacks its three tiers exactly as the refinement card
    /// does: the header under the top inset, the Planner ladder, the Critic
    /// ladder, and the directives editor are distinct zones separated by one
    /// blank row, and the action buttons stay inside the card
    /// (Conventions §6.7, §6.8).
    #[test]
    fn test_create_plan_dialog_geometry_stacks_the_three_tiers() {
        let term = Rect::new(0, 0, 120, 50);
        let geometry = create_plan_dialog_geometry(term, 3, 2);

        assert_eq!(geometry.header.y, geometry.area.y + 2, "border + top inset");
        assert_eq!(geometry.header.x, geometry.area.x + 2, "horizontal padding");
        assert_eq!(geometry.header.width, geometry.area.width - 4);
        assert_eq!(geometry.planner_rows.height, 3, "one row per Planner entry");
        assert_eq!(geometry.critic_rows.height, 2, "one row per Critic entry");
        assert_eq!(
            geometry.planner.y,
            geometry.header.y + geometry.header.height + 1
        );
        assert_eq!(
            geometry.critic.y,
            geometry.planner.y + geometry.planner.height + 1
        );
        assert_eq!(
            geometry.editor_area.y,
            geometry.critic.y + geometry.critic.height + 1,
            "pre-editor spacer"
        );
        assert!(geometry.editor_area.height >= 3);
        assert_eq!(geometry.confirm_button.y, geometry.cancel_button.y);
        assert!(geometry.confirm_button.right() < geometry.cancel_button.x);
        assert!(geometry.cancel_button.right() <= geometry.area.right());
    }
}
