use tokio::process::ChildStdin;

use crate::app::AppState;
use tauqe_protocol::methods;

/// Applies the status chosen in the status dialog to a plan item or a review finding,
/// updating local state optimistically and registering a server rollback.
pub(crate) async fn apply_status_dialog(
    st: &mut AppState,
    chosen_idx: usize,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<()> {
    let dialog = match st.status_dialog.take() {
        Some(d) => d,
        None => return Ok(()),
    };

    match dialog.target {
        crate::app::StatusDialogTarget::PlanItem {
            plan_id,
            item_id,
            current_status,
            ..
        } => {
            let statuses = [
                tauqe_protocol::PlanItemStatus::Discussion,
                tauqe_protocol::PlanItemStatus::Todo,
                tauqe_protocol::PlanItemStatus::InProgress,
                tauqe_protocol::PlanItemStatus::Done,
                tauqe_protocol::PlanItemStatus::Blocked,
                tauqe_protocol::PlanItemStatus::Cancelled,
            ];
            let new_status = match statuses.get(chosen_idx).copied() {
                Some(s) => s,
                None => return Ok(()),
            };

            if new_status != current_status {
                if let Some(plan) = st.plans_view.plans.iter_mut().find(|p| p.id == plan_id) {
                    plan.update_item_status(&item_id, new_status);
                }
                let req_id = crate::rpc::allocate_request_id();
                crate::rpc::record_optimistic_rollback(
                    req_id,
                    crate::rpc::OptimisticRollback::PlanItemStatus {
                        plan_id: plan_id.clone(),
                        item_id: item_id.clone(),
                        prev_status: current_status,
                    },
                );
                let params = tauqe_protocol::PlanUpdateItemParams {
                    plan_id,
                    item_id,
                    status: Some(new_status),
                };
                crate::rpc::send_request_with_id(
                    server_writer,
                    req_id,
                    methods::PLAN_UPDATE_ITEM,
                    serde_json::to_value(params)?,
                )
                .await?;
            }
        }
        crate::app::StatusDialogTarget::ReviewItem {
            review_id,
            item_id,
            current_status,
            ..
        } => {
            let statuses = [
                tauqe_protocol::ReviewStatus::Discussion,
                tauqe_protocol::ReviewStatus::Todo,
                tauqe_protocol::ReviewStatus::InProgress,
                tauqe_protocol::ReviewStatus::Fixed,
                tauqe_protocol::ReviewStatus::Rejected,
            ];
            let new_status = match statuses.get(chosen_idx).copied() {
                Some(s) => s,
                None => return Ok(()),
            };

            if new_status != current_status {
                for session in &mut st.review.sessions {
                    if review_id.as_ref().map(|id| id == &session.id).unwrap_or(true) {
                        if let Some(item) = session.items.iter_mut().find(|i| i.id == item_id) {
                            item.status = new_status;
                        }
                    }
                }
                if let Some(session) = st.review.session.as_mut() {
                    if let Some(item) = session.items.iter_mut().find(|i| i.id == item_id) {
                        item.status = new_status;
                    }
                }
                if st.review.hide_closed {
                    st.review.clamp_selection();
                    st.review.scroll_to_selected();
                }
                let req_id = crate::rpc::allocate_request_id();
                crate::rpc::record_optimistic_rollback(
                    req_id,
                    crate::rpc::OptimisticRollback::ReviewItemStatus {
                        item_id,
                        prev_status: current_status,
                    },
                );
                let target_review_id = review_id.or_else(|| st.review.session.as_ref().map(|s| s.id.clone()));
                let params = tauqe_protocol::ReviewUpdateItemParams {
                    review_id: target_review_id,
                    item_id,
                    status: Some(new_status),
                };
                crate::rpc::send_request_with_id(
                    server_writer,
                    req_id,
                    methods::REVIEW_UPDATE_ITEM,
                    serde_json::to_value(params)?,
                )
                .await?;
            }
        }
    }
    Ok(())
}
