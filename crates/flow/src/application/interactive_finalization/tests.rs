use super::*;
use std::sync::{Arc, Mutex};
use tauqe_core::config::AppConfig;
use tauqe_core::context::ContextAccess;
use tauqe_core::control::outcome::{LoopResult, LoopSuccess, ResponseEnvelope};
use tauqe_core::control::repl::ReplArtifact;
use tauqe_core::history::entry::HistoryEntry;

struct Observer {
    app: Application,
    trace: Arc<Mutex<Vec<&'static str>>>,
    snapshots: Vec<(&'static str, usize, Option<String>, bool)>,
}
#[async_trait::async_trait]
impl InteractiveFinalizationObserver for Observer {
    async fn publish(&mut self, update: InteractiveFinalizationUpdate<'_>) {
        let name = match update {
            InteractiveFinalizationUpdate::Cancelled => "cancelled",
            InteractiveFinalizationUpdate::UsageFlushed(snapshot) => {
                assert!(snapshot.cur.is_none());
                "usage"
            }
            InteractiveFinalizationUpdate::EditCompletion(_) => "edit",
            InteractiveFinalizationUpdate::Context(_) => "context",
            InteractiveFinalizationUpdate::Plans(_) => "plans",
            InteractiveFinalizationUpdate::Review(_) => "review",
        };
        let history = self
            .app
            .history_for_turn()
            .await
            .unwrap()
            .snapshot()
            .unwrap();
        let response = history.entries().iter().rev().find_map(|entry| {
            if let HistoryEntry::Response { message, .. } = entry {
                Some(message.clone())
            } else {
                None
            }
        });
        let new_file = self
            .app
            .context_state()
            .await
            .items
            .iter()
            .any(|item| item.path == "new.rs");
        self.snapshots
            .push((name, history.entries().len(), response, new_file));
        self.trace.lock().unwrap().push(name);
    }
}
async fn setup(
    root: &std::path::Path,
) -> (
    Application,
    OperationGuard,
    Option<u64>,
    Option<UsageGuard>,
    Observer,
) {
    std::fs::write(root.join("resident.rs"), "resident").unwrap();
    std::fs::write(root.join("new.rs"), "new").unwrap();
    let (history, _) = tokio::sync::mpsc::unbounded_channel();
    let app = Application::for_repository(AppConfig::default(), root.to_path_buf(), history);
    app.test_mutate_context(|context| {
        context.add_auto_file("resident.rs", ContextAccess::ReadOnly)
    })
    .await
    .unwrap()
    .unwrap();
    let operation = app.try_begin_operation().unwrap();
    let started = app
        .prepare_interactive_history(&operation, "Request".into())
        .await
        .unwrap();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let published = trace.clone();
    let usage = app
        .begin_usage("finalization", move |_, snapshot| {
            if snapshot.cur.is_none() {
                published.lock().unwrap().push("totals");
            }
        })
        .await;
    usage
        .observe(&tauqe_core::providers::ModelUsageInfo {
            prompt_tokens: 2,
            completion_tokens: 3,
            total_tokens: 5,
            cost: Some(0.01),
            ..Default::default()
        })
        .await;
    let observer = Observer {
        app: app.clone(),
        trace,
        snapshots: vec![],
    };
    (app, operation, started.turn_id, Some(usage), observer)
}
async fn outcome(app: &Application, edit: bool) -> InteractiveTaskOutcome {
    let mut context = app.context_for_turn().await;
    context
        .add_auto_file("new.rs", ContextAccess::ReadOnly)
        .unwrap();
    InteractiveTaskOutcome {
        context,
        result: Ok(LoopResult::Success(LoopSuccess::new(
            ReplArtifact {
                message: Some("Completed response".into()),
                modified_paths: if edit { vec!["new.rs".into()] } else { vec![] },
                plans_applied: false,
                plan_rejections: vec![],
                plan_warnings: vec![],
                review_decisions_applied: vec![],
                envelope: ResponseEnvelope::default(),
                commit_hash: None,
                verification: None,
                commit_error: None,
                rounds_spent: 1,
            },
            ResponseEnvelope::default(),
        ))),
    }
}
fn input(
    joined: Result<InteractiveTaskOutcome, InteractiveTaskError>,
    turn_id: Option<u64>,
    events: Vec<StreamEvent>,
) -> InteractiveFinalizationInput {
    let mut stream = InteractiveStreamState::default();
    for event in events {
        stream.observe(&event);
    }
    InteractiveFinalizationInput {
        joined,
        turn_id,
        stream,
        focus: None,
    }
}
#[tokio::test]
async fn cancellation_precedes_join_and_stream_failures_and_keeps_notice_order() {
    for task_failed in [false, true] {
        for streamed_cancel in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (app, operation, turn_id, mut usage, mut observer) = setup(root.path()).await;
            app.cancel_active_operation();
            let joined = if task_failed {
                Err(InteractiveTaskError::TaskFailed("task failed".into()))
            } else {
                Ok(outcome(&app, false).await)
            };
            let mut events = vec![StreamEvent::Error("provider error".into())];
            if streamed_cancel {
                events.push(StreamEvent::Cancelled);
            }
            let result = app
                .finalize_interactive_turn(
                    &operation,
                    input(joined, turn_id, events),
                    &mut usage,
                    &mut observer,
                )
                .await
                .unwrap();
            assert!(
                matches!(result.outcome,InteractiveFinalizationOutcome::Interrupted{cancelled:true,error:Some(reason)} if reason=="Operation cancelled")
            );
            assert!(usage.is_none());
            assert_eq!(app.history_page(None, None).await.unwrap().total_count, 0);
            let expected = if streamed_cancel {
                vec!["totals", "usage"]
            } else if task_failed {
                vec!["totals", "usage", "cancelled"]
            } else {
                vec!["cancelled", "totals", "usage"]
            };
            assert_eq!(*observer.trace.lock().unwrap(), expected);
            assert!(!app
                .context_state()
                .await
                .items
                .iter()
                .any(|item| item.path == "new.rs"));
            if !task_failed && !streamed_cancel {
                assert_eq!(observer.snapshots[0].1, 1);
            }
        }
    }
}
#[tokio::test]
async fn task_failure_overrides_stream_failure_and_records_history_before_accounting() {
    let root = tempfile::tempdir().unwrap();
    let (app, operation, turn_id, mut usage, mut observer) = setup(root.path()).await;
    let result = app
        .finalize_interactive_turn(
            &operation,
            input(
                Err(InteractiveTaskError::TaskFailed("task failed".into())),
                turn_id,
                vec![StreamEvent::Error("provider error".into())],
            ),
            &mut usage,
            &mut observer,
        )
        .await
        .unwrap();
    assert!(
        matches!(result.outcome,InteractiveFinalizationOutcome::Interrupted{cancelled:false,error:Some(reason)} if reason=="task failed")
    );
    assert_eq!(*observer.trace.lock().unwrap(), ["totals", "usage"]);
    assert_eq!(
        observer.snapshots[0].2.as_deref(),
        Some("Workflow task failed: task failed")
    );
}
#[tokio::test]
async fn stream_failure_records_diagnostic_instead_of_preview_and_does_not_adopt_context() {
    let root = tempfile::tempdir().unwrap();
    let (app, operation, turn_id, mut usage, mut observer) = setup(root.path()).await;
    let identity = app.session_uuid().await;
    let joined = Ok(outcome(&app, true).await);
    let result = app
        .finalize_interactive_turn(
            &operation,
            input(
                joined,
                turn_id,
                vec![
                    StreamEvent::TextDelta("Withdrawn".into()),
                    StreamEvent::ResetStream,
                    StreamEvent::TextDelta("Current attempt".into()),
                    StreamEvent::Error("offline".into()),
                ],
            ),
            &mut usage,
            &mut observer,
        )
        .await
        .unwrap();
    assert!(
        matches!(result.outcome,InteractiveFinalizationOutcome::Interrupted{cancelled:false,error:Some(reason)} if reason=="offline")
    );
    assert_eq!(observer.snapshots[0].2.as_deref(), Some("offline"));
    assert_eq!(*observer.trace.lock().unwrap(), ["totals", "usage"]);
    assert_eq!(app.session_uuid().await, identity);
    assert!(!app
        .context_state()
        .await
        .items
        .iter()
        .any(|item| item.path == "new.rs"));
}
#[tokio::test]
async fn successful_edit_accounts_then_merges_then_publishes_then_persists_and_cleans_context() {
    let root = tempfile::tempdir().unwrap();
    let (app, operation, turn_id, mut usage, mut observer) = setup(root.path()).await;
    let identity = app.session_uuid().await;
    let result = app
        .finalize_interactive_turn(
            &operation,
            input(Ok(outcome(&app, true).await), turn_id, vec![]),
            &mut usage,
            &mut observer,
        )
        .await
        .unwrap();
    assert!(matches!(
        result.outcome,
        InteractiveFinalizationOutcome::Completed(_)
    ));
    assert_eq!(
        *observer.trace.lock().unwrap(),
        ["totals", "usage", "edit", "context"]
    );
    assert!(!observer.snapshots[0].3);
    assert!(observer.snapshots[1].3);
    assert!(!observer.snapshots[2].3);
    assert!(observer.snapshots[1].2.is_none());
    assert_eq!(
        observer.snapshots[2].2.as_deref(),
        Some("Completed response")
    );
    assert_ne!(app.session_uuid().await, identity);
    assert!(usage.is_none());
    assert_eq!(result.usage.prev.total_tokens, 5);
    assert_eq!(app.active_operation_id(), Some(operation.id()));
}
#[tokio::test]
async fn control_error_is_completed_after_accounting_and_keeps_adopted_context() {
    let root = tempfile::tempdir().unwrap();
    let (app, operation, turn_id, mut usage, mut observer) = setup(root.path()).await;
    let mut joined = outcome(&app, false).await;
    joined.result = Err(tauqe_core::control::outcome::SystemError::Provider(
        "offline".into(),
    ));
    let result = app
        .finalize_interactive_turn(
            &operation,
            input(
                Ok(joined),
                turn_id,
                vec![StreamEvent::TextDelta("Premature preview".into())],
            ),
            &mut usage,
            &mut observer,
        )
        .await
        .unwrap();
    assert!(
        matches!(result.outcome,InteractiveFinalizationOutcome::Completed(completion) if completion.error.as_deref()==Some("provider error: offline"))
    );
    assert_eq!(
        *observer.trace.lock().unwrap(),
        ["totals", "usage", "edit", "context"]
    );
    assert!(app
        .context_state()
        .await
        .items
        .iter()
        .any(|item| item.path == "new.rs"));
    assert_eq!(
        observer.snapshots[2].2.as_deref(),
        Some("provider error: offline")
    );
}
#[tokio::test]
async fn foreign_operation_does_not_flush_or_publish_or_change_history() {
    let root = tempfile::tempdir().unwrap();
    let (app, _operation, turn_id, mut usage, mut observer) = setup(root.path()).await;
    let other = Application::default();
    let foreign = other.try_begin_operation().unwrap();
    assert!(matches!(
        app.finalize_interactive_turn(
            &foreign,
            input(
                Err(InteractiveTaskError::TaskFailed("failed".into())),
                turn_id,
                vec![]
            ),
            &mut usage,
            &mut observer
        )
        .await,
        Err(InteractiveTaskError::OperationNotOwned)
    ));
    assert!(usage.is_some());
    assert!(observer.trace.lock().unwrap().is_empty());
    assert_eq!(app.history_page(None, None).await.unwrap().total_count, 1);
}

#[tokio::test]
async fn completed_document_mutations_publish_context_then_plans_then_review() {
    let root = tempfile::tempdir().unwrap();
    let (app, operation, turn_id, mut usage, mut observer) = setup(root.path()).await;
    let plan = tauqe_core::plan::Plan {
        id: "plan".into(),
        ..Default::default()
    };
    tauqe_core::plan::storage::PlanStorage::save_plan(root.path(), &plan).unwrap();
    let review = tauqe_core::review::ReviewSession {
        id: "review".into(),
        ..Default::default()
    };
    tauqe_core::review::storage::ReviewStorage::save_session(root.path(), &review).unwrap();
    let mut joined = outcome(&app, false).await;
    if let Ok(LoopResult::Success(success)) = &mut joined.result {
        success.value.plans_applied = true;
        success.value.review_decisions_applied = vec![1, 1];
    }
    let mut request = input(Ok(joined), turn_id, vec![]);
    request.focus = Some(DiscussionTarget::ReviewGroup {
        session_id: "review".into(),
        group_id: "group".into(),
    });
    let result = app
        .finalize_interactive_turn(&operation, request, &mut usage, &mut observer)
        .await
        .unwrap();
    assert!(matches!(
        result.outcome,
        InteractiveFinalizationOutcome::Completed(_)
    ));
    assert_eq!(
        *observer.trace.lock().unwrap(),
        ["totals", "usage", "edit", "context", "plans", "review"]
    );
    assert_eq!(result.usage.prev.total_tokens, 5);
    assert_eq!(result.usage.operations, 1);
}
