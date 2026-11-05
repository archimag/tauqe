use std::sync::Arc;

use tauqe_protocol::methods;
use tokio::sync::{Mutex, MutexGuard};

use crate::app::AppState;
use crate::dialogs::render::status::{PLAN_STATUS_ORDER, REVIEW_STATUS_ORDER};
use crate::rpc::{
    allocate_request_id, send_request_with_rollback, OptimisticRollback, RpcConnection,
};

/// Applies the status chosen in the status dialog to a plan item or a review finding,
/// updating local state optimistically and binding its rollback to the transport write.
pub(crate) async fn apply_status_dialog(
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    chosen_idx: usize,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    let dialog = match st.modals.status_dialog.take() {
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
                    plan.implementation.update_item_status(&item_id, new_status);
                }
                // The rollback value is built first and travels with the
                // request: the transport records it only once the write
                // succeeds, and the same value reverts the optimistic mutation
                // here when the write fails (c-1, c-3).
                let rollback = OptimisticRollback::PlanItemStatus {
                    plan_id: plan_id.clone(),
                    item_id: item_id.clone(),
                    prev_status: current_status,
                };
                let req_id = allocate_request_id();
                let params = tauqe_protocol::PlanUpdateItemParams {
                    plan_id,
                    item_id,
                    status: Some(new_status),
                };
                let payload = serde_json::to_value(params)?;
                // The optimistic mutation and its rollback are owned by this
                // site, so the guard is released before the request leaves the
                // loop (c-1).
                drop(st);
                if let Err(error) = send_request_with_rollback(
                    server_writer,
                    req_id,
                    methods::PLAN_UPDATE_ITEM,
                    payload,
                    &rollback,
                )
                .await
                {
                    // The request never reached the daemon, so no response will
                    // ever undo the mutation: revert it here and report the
                    // failure once, at the site that owns the revert (c-1, c-3).
                    let mut st = state.lock().await;
                    crate::rpc::responses::apply_optimistic_rollback(&mut st, rollback);
                    let message = crate::i18n::active_t("notify.command_error");
                    st.notify_error(message.replacen("{}", &error.to_string(), 1));
                }
            }
        }
        crate::app::StatusDialogTarget::ReviewItem {
            review_id,
            item_id,
            current_status,
            ..
        } => {
            // The review lifecycle carries six statuses and is read from the
            // same ordered list the dialog renders, so a digit and the row it
            // highlights can never disagree about a status.
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
                let rollback = OptimisticRollback::ReviewIssueStatus {
                    review_id: target_review_id.clone(),
                    item_id,
                    prev_status: current_status,
                };
                let req_id = allocate_request_id();
                let params = tauqe_protocol::ReviewUpdateItemParams {
                    review_id: target_review_id,
                    item_id,
                    status: Some(new_status),
                    note: None,
                };
                let payload = serde_json::to_value(params)?;
                // The optimistic mutation and its rollback are owned by this
                // site, so the guard is released before the request leaves the
                // loop (c-1).
                drop(st);
                if let Err(error) = send_request_with_rollback(
                    server_writer,
                    req_id,
                    methods::REVIEW_UPDATE_ITEM,
                    payload,
                    &rollback,
                )
                .await
                {
                    let mut st = state.lock().await;
                    crate::rpc::responses::apply_optimistic_rollback(&mut st, rollback);
                    let message = crate::i18n::active_t("notify.command_error");
                    st.notify_error(message.replacen("{}", &error.to_string(), 1));
                }
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
    use tauqe_protocol::{
        IssueType, PlanItemStatus, ReviewIssue, ReviewItem, ReviewItemStatus, ReviewSession,
    };

    fn item(id: u32) -> ReviewItem {
        ReviewItem::from(ReviewIssue {
            id,
            title: format!("Finding {id}"),
            uuid: None,
            commit: None,
            group: None,
            model: None,
            issue_type: IssueType::Architecture,
            status: ReviewItemStatus::Todo,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
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
        st.modals.status_dialog = Some(StatusDialogState::new(
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
        apply_status_dialog(&state, guard, 2, &mut conn)
            .await
            .unwrap();

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
    async fn applies_fixed_status_optimistically() {
        let mut st = crate::app::test_state();
        st.review = ReviewViewState {
            sessions: vec![session("rev-1")],
            session: Some(session("rev-1")),
            ..Default::default()
        };
        st.modals.status_dialog = Some(StatusDialogState::new(
            StatusDialogTarget::ReviewItem {
                review_id: Some("rev-1".to_string()),
                item_id: 1,
                item_title: "Finding 1".to_string(),
                current_status: ReviewItemStatus::Todo,
            },
            // Option index 3 is FIXED in the six-status lifecycle.
            3,
        ));

        let state = std::sync::Arc::new(tokio::sync::Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        apply_status_dialog(&state, guard, 3, &mut conn)
            .await
            .unwrap();

        let st = state.lock().await;
        assert_eq!(
            st.review.sessions[0].items[0].status,
            ReviewItemStatus::Fixed
        );
        assert_eq!(st.modals.status_dialog, None, "the dialog must be consumed");
    }
    /// Async sink whose every write fails, standing in for a server pipe that
    /// closed before the status update reached it.
    struct FailingWriter;

    impl tokio::io::AsyncWrite for FailingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "server pipe closed",
            )))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A plan-status write that never reaches the transport must revert the row
    /// at the site that owns the mutation and leave no rollback entry behind,
    /// because the id of the failed write never receives a response (c-3).
    #[tokio::test]
    async fn failed_plan_status_write_reverts_the_row_and_leaks_no_rollback() {
        let mut st = crate::app::test_state();
        st.plans_view.plans = vec![tauqe_protocol::Plan::from_parts(
            "plan-1".to_string(),
            "Plan 1".to_string(),
            None,
            0,
            0,
            vec![],
            vec![tauqe_protocol::PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                status: PlanItemStatus::Todo,
                ..Default::default()
            }],
        )];
        st.modals.status_dialog = Some(StatusDialogState::new(
            StatusDialogTarget::PlanItem {
                plan_id: "plan-1".to_string(),
                item_id: "1".to_string(),
                item_title: "Step 1".to_string(),
                current_status: PlanItemStatus::Todo,
            },
            // Option index 3 is DONE in the six-status lifecycle.
            3,
        ));

        let state = std::sync::Arc::new(tokio::sync::Mutex::new(st));
        let mut conn = RpcConnection::new(FailingWriter);

        // The request id is allocated inside the update, so the tracked window
        // is bracketed by two client-side allocations (c-3).
        let window_start = crate::rpc::allocate_request_id();
        let guard = state.lock().await;
        apply_status_dialog(&state, guard, 3, &mut conn)
            .await
            .expect("a failed write is reported in the interface, not as a dispatch error");
        let window_end = crate::rpc::allocate_request_id();

        assert!(conn.written().is_empty(), "the transport accepted no bytes");
        for id in window_start..window_end {
            assert!(
                conn.take_optimistic_rollback(id).is_none(),
                "request {id} leaked a rollback entry"
            );
        }

        let st = state.lock().await;
        assert_eq!(
            st.plans_view.plans[0].implementation.items[0].status,
            PlanItemStatus::Todo,
            "the row keeps the status the server recorded"
        );
        assert_eq!(
            st.active_notification().map(|n| n.level),
            Some(crate::app::NotificationLevel::Error)
        );
    }

    /// A review-status write that never reaches the transport must revert the
    /// finding exactly as the error-response path does, and leave no rollback
    /// entry behind (c-3).
    #[tokio::test]
    async fn failed_review_status_write_reverts_the_finding_and_leaks_no_rollback() {
        let mut st = crate::app::test_state();
        st.review = ReviewViewState {
            sessions: vec![session("rev-1")],
            session: Some(session("rev-1")),
            ..Default::default()
        };
        st.modals.status_dialog = Some(StatusDialogState::new(
            StatusDialogTarget::ReviewItem {
                review_id: Some("rev-1".to_string()),
                item_id: 1,
                item_title: "Finding 1".to_string(),
                current_status: ReviewItemStatus::Todo,
            },
            // Option index 3 is FIXED in the six-status lifecycle.
            3,
        ));

        let state = std::sync::Arc::new(tokio::sync::Mutex::new(st));
        let mut conn = RpcConnection::new(FailingWriter);

        let window_start = crate::rpc::allocate_request_id();
        let guard = state.lock().await;
        apply_status_dialog(&state, guard, 3, &mut conn)
            .await
            .expect("a failed write is reported in the interface, not as a dispatch error");
        let window_end = crate::rpc::allocate_request_id();

        assert!(conn.written().is_empty(), "the transport accepted no bytes");
        for id in window_start..window_end {
            assert!(
                conn.take_optimistic_rollback(id).is_none(),
                "request {id} leaked a rollback entry"
            );
        }

        let st = state.lock().await;
        assert_eq!(
            st.review.sessions[0].items[0].status,
            ReviewItemStatus::Todo,
            "the finding keeps the status the server recorded"
        );
        assert_eq!(
            st.active_notification().map(|n| n.level),
            Some(crate::app::NotificationLevel::Error)
        );
    }
}
