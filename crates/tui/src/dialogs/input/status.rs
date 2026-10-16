use crate::app::AppState;
use crate::rpc::RpcConnection;
use tauqe_protocol::methods;

/// Applies the status chosen in the status dialog to a plan item or a review finding,
/// updating local state optimistically and registering a server rollback.
pub(crate) async fn apply_status_dialog(
    st: &mut AppState,
    chosen_idx: usize,
    server_writer: &mut RpcConnection,
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
                server_writer.record_optimistic_rollback(
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
                tauqe_protocol::ReviewItemStatus::Discussion,
                tauqe_protocol::ReviewItemStatus::Todo,
                tauqe_protocol::ReviewItemStatus::InProgress,
                tauqe_protocol::ReviewItemStatus::Fixed,
                tauqe_protocol::ReviewItemStatus::Skipped,
                tauqe_protocol::ReviewItemStatus::Rejected,
            ];
            let new_status = match statuses.get(chosen_idx).copied() {
                Some(s) => s,
                None => return Ok(()),
            };

            if new_status != current_status {
                // Resolve the target session once and reuse it for the local
                // update, the optimistic rollback, and the request, so a
                // finding id shared across sessions never mutates the wrong
                // session's finding.
                let target_review_id = review_id
                    .or_else(|| st.review.session.as_ref().map(|s| s.id.clone()));
                st.review
                    .set_item_status(target_review_id.as_deref(), item_id, new_status);
                if st.review.hide_closed {
                    st.review.clamp_selection();
                    st.review.scroll_to_selected();
                }
                let req_id = crate::rpc::allocate_request_id();
                server_writer.record_optimistic_rollback(
                    req_id,
                    crate::rpc::OptimisticRollback::ReviewItemStatus {
                        review_id: target_review_id.clone(),
                        item_id,
                        prev_status: current_status,
                    },
                );
                let params = tauqe_protocol::ReviewUpdateItemParams {
                    review_id: target_review_id,
                    item_id,
                    status: Some(new_status),
                    resolution_notes: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{StatusDialogState, StatusDialogTarget};
    use crate::views::review::ReviewViewState;
    use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSeverity, ReviewSession};

    fn item(id: u32) -> ReviewItem {
        ReviewItem {
            id,
            title: format!("Finding {id}"),
            model: None,
            severity: ReviewSeverity::Warning,
            status: ReviewItemStatus::Todo,
            files: Vec::new(),
            subplan: Vec::new(),
            resolution_notes: None,
            body: String::new(),
        }
    }

    fn session(id: &str) -> ReviewSession {
        ReviewSession {
            id: id.to_string(),
            title: format!("Session {id}"),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![item(1)],
            raw_markdown: String::new(),
        }
    }

    #[tokio::test]
    async fn applies_review_status_only_to_the_target_session() {
        let mut st = crate::app::test_state();
        st.review = ReviewViewState {
            sessions: vec![session("rev-1"), session("rev-2")],
            session: Some(session("rev-1")),
            ..Default::default()
        };
        st.status_dialog = Some(StatusDialogState::new(
            StatusDialogTarget::ReviewItem {
                review_id: Some("rev-2".to_string()),
                item_id: 1,
                item_title: "Finding 1".to_string(),
                current_status: ReviewItemStatus::Todo,
            },
            2,
        ));

        let mut conn = RpcConnection::new(Vec::<u8>::new());
        apply_status_dialog(&mut st, 2, &mut conn).await.unwrap();

        // rev-2's finding moves to InProgress; rev-1's identically numbered
        // finding, and the active session mirror, stay untouched.
        assert_eq!(st.review.sessions[0].items[0].status, ReviewItemStatus::Todo);
        assert_eq!(
            st.review.sessions[1].items[0].status,
            ReviewItemStatus::InProgress
        );
        assert_eq!(
            st.review.session.as_ref().map(|s| s.items[0].status),
            Some(ReviewItemStatus::Todo)
        );
    }
}
