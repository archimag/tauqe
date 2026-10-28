use tauqe_protocol::methods;
use tokio::sync::MutexGuard;

use crate::app::AppState;
use crate::dialogs::render::status::{PLAN_STATUS_ORDER, REVIEW_STATUS_ORDER};
use crate::rpc::RpcConnection;

/// Applies the status chosen in the status dialog to a plan item or a review finding,
/// updating local state optimistically and registering a server rollback.
pub(crate) async fn apply_status_dialog(
    mut st: MutexGuard<'_, AppState>,
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
            // The chosen index is the position of the status in the list the
            // dialog draws, so rendering and fast-select share one source of
            // truth and can never disagree about which status a digit applies.
            let new_status = match PLAN_STATUS_ORDER.get(chosen_idx).copied() {
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
                let payload = serde_json::to_value(params)?;
                // The optimistic mutation and its rollback are recorded, so the
                // guard is released before the request leaves the loop (c-1).
                drop(st);
                crate::rpc::send_request_with_id(
                    server_writer,
                    req_id,
                    methods::PLAN_UPDATE_ITEM,
                    payload,
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
            // The review lifecycle carries seven statuses and is read from the
            // same ordered list the dialog renders, so `Refine` keeps sitting
            // between `InProgress` and `Fixed` for both the rows and the digits.
            let new_status = match REVIEW_STATUS_ORDER.get(chosen_idx).copied() {
                Some(s) => s,
                None => return Ok(()),
            };

            if new_status != current_status {
                // Resolve the target session once and reuse it for the local
                // update, the optimistic rollback, and the request, so a
                // finding id shared across sessions never mutates the wrong
                // session's finding.
                let target_review_id =
                    review_id.or_else(|| st.review.session.as_ref().map(|s| s.id.clone()));
                st.review
                    .set_item_status(target_review_id.as_deref(), item_id, new_status);
                if st.review.hide_closed {
                    st.review.clamp_selection();
                    st.review.scroll_to_selected();
                }
                let req_id = crate::rpc::allocate_request_id();
                server_writer.record_optimistic_rollback(
                    req_id,
                    crate::rpc::OptimisticRollback::ReviewIssueStatus {
                        review_id: target_review_id.clone(),
                        item_id,
                        prev_status: current_status,
                    },
                );
                let params = tauqe_protocol::ReviewUpdateItemParams {
                    review_id: target_review_id,
                    item_id,
                    status: Some(new_status),
                    note: None,
                };
                let payload = serde_json::to_value(params)?;
                // The optimistic mutation and its rollback are recorded, so the
                // guard is released before the request leaves the loop (c-1).
                drop(st);
                crate::rpc::send_request_with_id(
                    server_writer,
                    req_id,
                    methods::REVIEW_UPDATE_ITEM,
                    payload,
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
    use tauqe_protocol::{IssueType, ReviewIssue, ReviewItem, ReviewItemStatus, ReviewSession};

    fn item(id: u32) -> ReviewItem {
        ReviewItem::from(ReviewIssue {
            id,
            title: format!("Finding {id}"),
            uuid: None,
            group: None,
            model: None,
            issue_type: IssueType::Architecture,
            status: ReviewItemStatus::Todo,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            plan: Vec::new(),
            note: None,
        })
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
            groups: Vec::new(),
            post_analysis: None,
            launch_config: None,
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

        let state = std::sync::Arc::new(tokio::sync::Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        apply_status_dialog(guard, 2, &mut conn).await.unwrap();

        // rev-2's finding moves to InProgress; rev-1's identically numbered
        // finding, and the active session mirror, stay untouched.
        let st = state.lock().await;
        assert_eq!(
            st.review.sessions[0].items[0].status,
            ReviewItemStatus::Todo
        );
        assert_eq!(
            st.review.sessions[1].items[0].status,
            ReviewItemStatus::InProgress
        );
        assert_eq!(
            st.review.session.as_ref().map(|s| s.items[0].status),
            Some(ReviewItemStatus::Todo)
        );
    }

    #[tokio::test]
    async fn applies_refine_status_optimistically() {
        let mut st = crate::app::test_state();
        st.review = ReviewViewState {
            sessions: vec![session("rev-1")],
            session: Some(session("rev-1")),
            ..Default::default()
        };
        st.status_dialog = Some(StatusDialogState::new(
            StatusDialogTarget::ReviewItem {
                review_id: Some("rev-1".to_string()),
                item_id: 1,
                item_title: "Finding 1".to_string(),
                current_status: ReviewItemStatus::Todo,
            },
            // Option index 3 is REFINE in the seven-status lifecycle.
            3,
        ));

        let state = std::sync::Arc::new(tokio::sync::Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        apply_status_dialog(guard, 3, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert_eq!(
            st.review.sessions[0].items[0].status,
            ReviewItemStatus::Refine
        );
        assert_eq!(st.status_dialog, None, "the dialog must be consumed");
    }
}
