use crate::api::{
    Application, ApplicationEvent, ApplicationObserver, AskError, BatchOutcome, ModelAskInput,
    PlanBatchOptions, ReviewEvent, ReviewStartInput,
};
use std::{path::Path, sync::Arc, time::Duration};
use tauqe_core::config::AppConfig;
use tauqe_core::discussion::{DiscussionFocus, DiscussionTarget};
use tauqe_core::plan::{Plan, PlanItem, PlanItemStatus};
use tauqe_core::providers::{mock::MockResponse, MockLlmProvider};
use tauqe_core::workflow::WorkflowKind;
use tokio::sync::mpsc;

const DEADLINE: Duration = Duration::from_secs(5);

struct Observer(mpsc::UnboundedSender<(String, ApplicationEvent)>);

impl ApplicationObserver for Observer {
    fn publish(&self, id: &str, event: ApplicationEvent) {
        let _ = self.0.send((id.into(), event));
    }
}

fn application(root: &Path) -> Application {
    let mut config = AppConfig::default();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    let (history, _) = mpsc::unbounded_channel();
    Application::for_repository(config, root.into(), history)
}

async fn released(app: &Application) {
    tokio::time::timeout(DEADLINE, async {
        while app.active_operation_id().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("operation releases after terminal publication");
}

#[tokio::test]
async fn ask_returns_the_published_operation_id_and_settles_headlessly() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text("Reply")]));
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    let started = app
        .ask(
            ModelAskInput::Prompt {
                text: "Question".into(),
            },
            Arc::new(Observer(events)),
        )
        .await
        .unwrap();
    let (id, event) = received.recv().await.unwrap();
    assert_eq!(id, started.operation_id);
    assert!(matches!(event, ApplicationEvent::Started { .. }));
    tokio::time::timeout(DEADLINE, async {
        let mut result = false;
        let mut flushed = false;
        while let Some((id, event)) = received.recv().await {
            assert_eq!(id, started.operation_id);
            match event {
                ApplicationEvent::UsageTotals(snapshot) => flushed = snapshot.cur.is_none(),
                ApplicationEvent::Result { .. } => result = true,
                ApplicationEvent::Finished(text) => {
                    assert_eq!(text, "Reply");
                    assert!(result && flushed);
                    return;
                }
                _ => {}
            }
        }
        panic!("interactive execution closed without a finished event");
    })
    .await
    .unwrap();
    released(&app).await;
    assert_eq!(app.usage_snapshot().await.operations, 1);
    assert_eq!(app.history_page(None, None).await.unwrap().total_count, 2);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn ask_compacts_history_before_announcing_the_turn() {
    struct StartObserver(Observer, Arc<MockLlmProvider>);

    impl ApplicationObserver for StartObserver {
        fn publish(&self, id: &str, event: ApplicationEvent) {
            if matches!(&event, ApplicationEvent::Started { .. }) {
                assert_eq!(self.1.requests().len(), 1, "only the Scribe has run");
            }
            self.0.publish(id, event);
        }
    }

    let root = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    config.history.budget_tokens = 500;
    config.history.tail_turns = 1;
    let (history, _history_events) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().to_path_buf(), history);
    for _ in 0..4 {
        app.record_history_exchange(
            "Previous task ".repeat(200),
            Vec::new(),
            "Done".into(),
            None,
        )
        .await
        .unwrap();
    }
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(r#"{"summary":"Earlier tasks completed."}"#),
        MockResponse::text("Reply"),
    ]));
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    let started = app
        .ask(
            ModelAskInput::Prompt {
                text: "New task".into(),
            },
            Arc::new(StartObserver(Observer(events), provider.clone())),
        )
        .await
        .unwrap();
    tokio::time::timeout(DEADLINE, async {
        let (id, event) = received.recv().await.expect("the turn did not start");
        assert_eq!(id, started.operation_id);
        assert!(matches!(event, ApplicationEvent::Started { .. }));
        while let Some((_, event)) = received.recv().await {
            if matches!(event, ApplicationEvent::Finished(_)) {
                return;
            }
        }
        panic!("the turn did not finish");
    })
    .await
    .unwrap();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]
        .messages
        .iter()
        .any(|message| message.content.contains("Earlier tasks completed.")));
}
#[tokio::test]
async fn a_busy_ask_keeps_stale_focus_and_makes_no_provider_call() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let provider = Arc::new(MockLlmProvider::new());
    app.test_set_provider(provider.clone()).await;
    let focus = DiscussionFocus::Focused {
        target: DiscussionTarget::Plan {
            id: "missing".into(),
        },
        focused_items: Vec::new(),
    };
    app.set_discussion_focus(focus.clone()).await;
    let operation = app.try_begin_operation().unwrap();
    let (events, mut received) = mpsc::unbounded_channel();
    let outcome = app
        .ask(
            ModelAskInput::Prompt {
                text: "Continue".into(),
            },
            Arc::new(Observer(events)),
        )
        .await;
    assert!(matches!(outcome, Err(AskError::Busy)));
    assert_eq!(app.discussion_focus().await, focus);
    assert_eq!(app.active_operation_id(), Some(operation.id()));
    assert!(provider.requests().is_empty());
    assert!(received.try_recv().is_err());
}

#[tokio::test]
async fn a_plan_batch_executes_with_only_a_publication_observer() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let (events, mut received) = mpsc::unbounded_channel();
    let observer = Arc::new(Observer(events));
    let plan = Plan::from_parts(
        "plan".into(),
        "Plan".into(),
        None,
        0,
        0,
        vec![],
        vec![PlanItem {
            id: "1".into(),
            title: "Confirm the work".into(),
            files: vec!["source.rs".into()],
            ..Default::default()
        }],
    );
    app.save_plan(plan, observer.as_ref()).await.unwrap();
    while received.try_recv().is_ok() {}
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([MockResponse::text(
        r#"{"message":"","action":{"kind":"edit","changes":[],"commit_message":"","message":"","reason":"Already done"}}"#,
    )]))).await;
    let started = app
        .execute_plan("plan".into(), PlanBatchOptions::default(), observer)
        .await
        .unwrap();
    let (id, report) = tokio::time::timeout(DEADLINE, async {
        while let Some((id, event)) = received.recv().await {
            if let ApplicationEvent::PlanBatchFinished(report) = event {
                return (id, report);
            }
        }
        panic!("plan closed without a terminal batch report");
    })
    .await
    .unwrap();
    assert_eq!(id, started.operation_id);
    assert_eq!(started.step_ids, ["1"]);
    assert_eq!(report.outcome, BatchOutcome::Completed { steps: 1 });
    released(&app).await;
    let plan = app.get_plan(Some(started.plan_id)).await.unwrap().unwrap();
    assert_eq!(
        plan.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Done
    );
}

#[tokio::test]
async fn a_review_launch_materializes_and_publishes_the_saved_session_headlessly() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("source.rs"), "fn sample() {}").unwrap();
    let app = application(root.path());
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([MockResponse::text(
        r#"{"summary":"Audit complete.","findings":[]}"#,
    )])))
    .await;
    let (events, mut received) = mpsc::unbounded_channel();
    let observer = Arc::new(Observer(events));
    app.add_context_file(
        "source.rs".into(),
        tauqe_core::context::ContextAccess::ReadOnly,
        None,
        observer.as_ref(),
    )
    .await
    .unwrap();
    let id = app
        .start_review(
            ReviewStartInput {
                preset: None,
                model: None,
                path_patterns: Vec::new(),
                prompt: None,
            },
            observer,
            DEADLINE,
        )
        .await
        .unwrap();
    let session = tokio::time::timeout(DEADLINE, async {
        let mut saved = None;
        while let Some((event_id, event)) = received.recv().await {
            match event {
                ApplicationEvent::ReviewDocument(session) => saved = Some(session.id),
                ApplicationEvent::ReviewRun(ReviewEvent::Finished { session, .. }) => {
                    assert_eq!(event_id, id);
                    assert_eq!(saved.as_deref(), Some(session.id.as_str()));
                    return session;
                }
                ApplicationEvent::ReviewRun(ReviewEvent::Error(error)) => panic!("{error}"),
                _ => {}
            }
        }
        panic!("review closed without a termination notice");
    })
    .await
    .unwrap();
    released(&app).await;
    assert_eq!(
        app.get_review(Some(session.id.clone())).await.unwrap(),
        Some(session)
    );
}
