//! Shared projection of an implementation horizon for the terminal views.
//!
//! The Plans tree and the Review tree render the same step graph: a step row
//! carries the same lifecycle badge, and a horizon reports the same leaf
//! progress. Hosting that vocabulary once keeps the two surfaces from drifting
//! apart, so a review subplan always reads exactly like an implementation step
//! (c-5, Conventions §1.3, §8.4).

use ratatui::style::{Style, Stylize};
use tauqe_protocol::{PlanImplementation, PlanItemStatus};

use crate::theme::engine::current_engine;

/// Filled badge style for a step lifecycle status.
///
/// The Plans tree and the Review tree both project a step status through this
/// single palette mapping, so a lifecycle badge can never name the same status
/// with two different colors (Conventions §8.4).
pub(crate) fn status_style(status: PlanItemStatus) -> Style {
    let engine = current_engine();
    let palette = engine.palette();
    let inverse = engine.color(palette.text_inverse);
    match status {
        PlanItemStatus::Discussion => engine.bg(palette.button_active_bg).fg(inverse).bold(),
        PlanItemStatus::Todo => engine.bg(palette.status_warning).fg(inverse).bold(),
        PlanItemStatus::InProgress => engine.bg(palette.status_info).fg(inverse).bold(),
        PlanItemStatus::Done => engine.bg(palette.status_success).fg(inverse).bold(),
        PlanItemStatus::Blocked => engine.bg(palette.status_error).fg(inverse).bold(),
        PlanItemStatus::Cancelled => engine.bg(palette.text_muted).fg(inverse).bold(),
    }
}

/// Executable leaf progress of a horizon as `(total, done)`.
///
/// Delegates to `PlanImplementation::count_stats`, the single authoritative
/// metric: only leaves carry an explicit status, cancelled leaves are excluded
/// from the total, and `done` counts the finished leaves. The Plans view and
/// the Review view therefore report one number for one horizon (c-5).
pub(crate) fn leaf_progress(implementation: &PlanImplementation) -> (usize, usize) {
    implementation.count_stats()
}
