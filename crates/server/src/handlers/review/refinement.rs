//! RPC projection of the Flow-owned group refinement operation.

use crate::contracts::Project;
use crate::state::AppState;
use std::sync::Arc;
use tauqe_flow::api::{GroupRefineError, GroupRefineInput, GroupRefineResult};
use tauqe_protocol::{errors, Request, Response, ReviewRefineGroupParams, ReviewRefineGroupResult};

pub async fn handle_review_refine_group(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewRefineGroupParams = match req
        .params
        .and_then(|value| serde_json::from_value(value).ok())
    {
        Some(params) => params,
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid review group refinement parameters",
            )
        }
    };
    let review_id = params.review_id;
    let group_id = params.group_id;
    let observer = Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    });
    let result = state
        .application
        .refine_review_group(
            GroupRefineInput {
                review_id: review_id.clone(),
                group_id: group_id.clone(),
                planner_model: params.model.map(Project::project),
                critic_model: params.critic_model.map(Project::project),
                skip_critique: params.skip_critique,
                instructions: params.instructions,
            },
            observer,
            crate::reap::task_exit_deadline(),
        )
        .await;
    match result {
        Ok(GroupRefineResult::Saved { group, explanation }) => Response::ok_typed(
            req.id,
            &ReviewRefineGroupResult::Success {
                review_id,
                group_id,
                group: Box::new((*group).project()),
                explanation,
            },
        ),
        Ok(GroupRefineResult::Blocked { reason }) => {
            Response::ok_typed(req.id, &ReviewRefineGroupResult::Blocked { reason })
        }
        Ok(GroupRefineResult::Aborted { reason }) => Response::ok_typed(
            req.id,
            &ReviewRefineGroupResult::Aborted {
                recommendation: reason.clone(),
                reason,
            },
        ),
        Err(error) => {
            let code = match &error {
                GroupRefineError::Busy => errors::OPERATION_IN_PROGRESS,
                GroupRefineError::InvalidAddress => errors::INVALID_PARAMS,
                GroupRefineError::NotFound => errors::NOT_FOUND,
                GroupRefineError::NotArchitectural => errors::INVALID_PARAMS,
                GroupRefineError::NothingToRefine => errors::NOTHING_TO_REFINE,
                GroupRefineError::Cancelled => errors::CANCELLED,
                GroupRefineError::EmptyHorizon => errors::EMPTY_ITEMS,
                GroupRefineError::InvalidTree(_) => errors::INVALID_PLAN_TREE,
                GroupRefineError::StaleSnapshot(_) => errors::REVIEW_UPDATE_FAILED,
                GroupRefineError::Model(_) => errors::MODEL_ERROR,
                GroupRefineError::Storage(_) => errors::REVIEW_UPDATE_FAILED,
            };
            Response::err(req.id, code, error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::review::tests::{
        architectural_session, horizon_config, horizon_leaf, horizon_response, init_repo_with_file,
        subplan_item, test_item, test_state, TestDir,
    };
    use tauqe_core::plan::PlanItemStatus;
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::review::{IssueStatus, ReviewGroup, ReviewGroupKind, ReviewStorage};
    use tauqe_protocol::{methods, DiscussionFocus, DiscussionTarget, RequestId};

    fn request(params: serde_json::Value) -> Request {
        Request {
            id: RequestId::Number(1),
            method: methods::REVIEW_REFINE_GROUP.into(),
            params: Some(params),
        }
    }

    #[tokio::test]
    async fn refuses_invalid_and_non_architectural_targets() {
        let dir = TestDir::new();
        let state = test_state(&dir);
        for params in [
            serde_json::json!({}),
            serde_json::json!({"review_id":"r","group_id":"g","step_id":"s"}),
            serde_json::json!({"review_id":"","group_id":"g"}),
        ] {
            assert_eq!(
                handle_review_refine_group(request(params), &state)
                    .await
                    .error
                    .unwrap()
                    .code,
                errors::INVALID_PARAMS
            );
        }
        assert_eq!(
            handle_review_refine_group(
                request(serde_json::json!({"review_id":"absent","group_id":"g"})),
                &state
            )
            .await
            .error
            .unwrap()
            .code,
            errors::NOT_FOUND
        );
        let mut session = architectural_session("r", "arch");
        session.groups[0].kind = ReviewGroupKind::Simple;
        ReviewStorage::save_session(dir.path(), &session).unwrap();
        assert_eq!(
            handle_review_refine_group(
                request(serde_json::json!({"review_id":"r","group_id":"arch"})),
                &state
            )
            .await
            .error
            .unwrap()
            .code,
            errors::INVALID_PARAMS
        );
        assert_eq!(
            handle_review_refine_group(
                request(serde_json::json!({"review_id":"r","group_id":"missing"})),
                &state
            )
            .await
            .error
            .unwrap()
            .code,
            errors::NOT_FOUND
        );
        session.groups[0].kind = ReviewGroupKind::Architectural;
        session.items[0].status = IssueStatus::Fixed;
        ReviewStorage::save_session(dir.path(), &session).unwrap();
        assert_eq!(
            handle_review_refine_group(
                request(serde_json::json!({"review_id":"r","group_id":"arch"})),
                &state
            )
            .await
            .error
            .unwrap()
            .code,
            errors::NOTHING_TO_REFINE
        );
    }

    #[tokio::test]
    async fn saved_horizon_keeps_finished_work_and_does_not_run_coder() {
        let dir = TestDir::new();
        init_repo_with_file(&dir, "src/target.rs");
        let mut session = architectural_session("rev-recover", "arch");
        let mut done = subplan_item(
            "done",
            "Completed",
            PlanItemStatus::Done,
            vec!["src/target.rs"],
        );
        done.commit = Some("existing-commit".into());
        let mut blocked = subplan_item(
            "blocked",
            "Blocked",
            PlanItemStatus::Blocked,
            vec!["src/target.rs"],
        );
        blocked.blocked_reason = Some("Missing decision".into());
        let mut dependent = subplan_item(
            "dependent",
            "Dependent",
            PlanItemStatus::Todo,
            vec!["src/target.rs"],
        );
        dependent.depends_on = vec!["blocked".into()];
        session.groups[0].implementation.items = vec![
            done.clone(),
            tauqe_core::plan::PlanItem {
                id: "branch".into(),
                title: "Open work".into(),
                children: vec![blocked, dependent],
                ..Default::default()
            },
        ];
        session.groups[0].blocked_reason = Some("Previous attempt stalled".into());
        session.groups[0].resolution_notes = Some("Keep the completed foundation".into());
        let mut closed = test_item(2, IssueStatus::Fixed, "src/target.rs");
        closed.group = Some("arch".into());
        session.items.push(closed.into());
        let neighbor = ReviewGroup {
            id: "neighbor".into(),
            title: "Other work".into(),
            ..Default::default()
        };
        session.groups.push(neighbor.clone());
        let original_items = session.items.clone();
        ReviewStorage::save_session(dir.path(), &session).unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            horizon_response(serde_json::json!([horizon_leaf("replacement")])),
        )]));
        let (out, _rx) = crate::transport::OutChannel::new();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(horizon_config(), dir.path().into(), history, out);
        state.application.test_set_provider(provider.clone()).await;
        let focus = DiscussionFocus::from_target(
            DiscussionTarget::ReviewGroup {
                session_id: "rev-recover".into(),
                group_id: "arch".into(),
            },
            vec!["step:blocked".into()],
        );
        let focused = crate::handlers::handle_request(
            Request {
                id: RequestId::Number(2),
                method: methods::DISCUSSION_SET_FOCUS.into(),
                params: Some(serde_json::json!({"focus":focus})),
            },
            &state,
        )
        .await;
        assert!(focused.error.is_none());
        let result = crate::handlers::handle_request(
            request(serde_json::json!({"review_id":"rev-recover","group_id":"arch"})),
            &state,
        )
        .await;
        assert!(result.error.is_none(), "{result:?}");
        let result: ReviewRefineGroupResult =
            serde_json::from_value(result.result.unwrap()).unwrap();
        assert!(matches!(result, ReviewRefineGroupResult::Success { .. }));
        let stored = ReviewStorage::load_review(dir.path(), "rev-recover")
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.groups[0].implementation.find_item("done"),
            Some(&done)
        );
        assert!(stored.groups[0]
            .implementation
            .find_item("replacement")
            .is_some());
        assert!(stored.groups[0].blocked_reason.is_none());
        assert_eq!(
            stored.groups[0].resolution_notes,
            session.groups[0].resolution_notes
        );
        assert_eq!(stored.groups[1], neighbor);
        assert_eq!(stored.items, original_items);
        assert_eq!(
            provider
                .requests()
                .iter()
                .map(|r| r.model.as_str())
                .collect::<Vec<_>>(),
            ["review/architect"]
        );
        assert!(
            !tauqe_core::plan::storage::PlanStorage::plans_dir(dir.path())
                .join("rev-recover.json")
                .exists()
        );
    }
}
