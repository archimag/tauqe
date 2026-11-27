use super::*;
use tauqe_core::context::ContextAccess;
use tauqe_core::providers::StreamEvent;

#[tokio::test]
async fn zero_round_budget_closes_without_a_provider_call() {
    let root = tempfile::tempdir().unwrap();
    let mut config: AppConfig = toml::from_str("[repl]\nmax_turn_rounds = 0\n").unwrap();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    let (history, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
    let provider = Arc::new(MockLlmProvider::new());
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Question".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    released(&app).await;
    assert!(provider.requests().is_empty());
    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.total_count, 2);
    assert!(page.items[0].rounds.is_empty());
    assert!(page.items[1].text.contains("0 provider call(s)"));
    assert!(std::iter::from_fn(|| received.try_recv().ok()).any(|(_, event)|
        matches!(event, ApplicationEvent::Result { completion, .. } if completion.error.is_none())));
    assert_eq!(app.usage_snapshot().await.prev.total_tokens, 0);
}

#[tokio::test]
async fn dropped_ask_caller_keeps_the_operation_until_terminal_publication() {
    use tokio::sync::Notify;

    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    *app.interactive_execution().terminal_gate.lock().await =
        Some((entered.clone(), resume.clone()));
    let provider = Arc::new(MockLlmProvider::scripted([
        metered(r#"{"message":"First reply.","action":{"kind":"answer"}}"#),
        metered(r#"{"message":"Second reply.","action":{"kind":"answer"}}"#),
    ]));
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    let caller_app = app.clone();
    let caller = tokio::spawn(async move {
        caller_app
            .ask(
                ModelAskInput::Prompt {
                    text: "First".into(),
                },
                Arc::new(Observer(events)),
            )
            .await
    });
    let started = caller.await.unwrap().unwrap();
    tokio::time::timeout(DEADLINE, entered.notified())
        .await
        .unwrap();
    let old_id = app.active_operation_id().unwrap();
    assert_eq!(started.operation_id, format!("op-{old_id}"));
    let (ignored, _) = mpsc::unbounded_channel();
    assert!(matches!(
        app.ask(
            ModelAskInput::Prompt {
                text: "Too early".into()
            },
            Arc::new(Observer(ignored))
        )
        .await,
        Err(super::AskError::Busy)
    ));
    assert_eq!(provider.requests().len(), 1);
    resume.notify_one();
    released(&app).await;
    assert!(std::iter::from_fn(|| received.try_recv().ok()).any(
        |(_, event)| matches!(event, ApplicationEvent::Finished(reply) if reply == "First reply.")
    ));
    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.items[1].text, "First reply.");

    let (next_events, mut next_received) = mpsc::unbounded_channel();
    let next = app
        .ask(
            ModelAskInput::Prompt {
                text: "Second".into(),
            },
            Arc::new(Observer(next_events)),
        )
        .await
        .unwrap();
    app.abort_stalled_operation_with_deadline(old_id, Duration::from_millis(10))
        .await;
    released(&app).await;
    assert!(std::iter::from_fn(|| next_received.try_recv().ok())
        .any(|(id, event)| id == next.operation_id
            && matches!(event, ApplicationEvent::Finished(reply) if reply == "Second reply.")));
    assert_eq!(provider.requests().len(), 2);
    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.total_count, 4);
    assert_eq!(page.items[3].text, "Second reply.");
}

#[tokio::test]
async fn researched_answer_keeps_context_and_dialogue_identity() {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.txt"), "source\n").unwrap();
    tauqe_core::git::cmd::stage_files(root.path(), &["source.txt".into()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(root.path(), "initial", false).unwrap();
    let app = application(root.path());
    let identity = app.session_uuid().await;
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([
        metered(r#"{"message":"Reading.","action":{"kind":"research","context_requests":[{"path":"source.txt","access":"read_only"}]}}"#),
        metered(r#"{"message":"Answered.","action":{"kind":"answer"}}"#),
    ])))
    .await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Read source".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    released(&app).await;
    assert_eq!(app.session_uuid().await, identity);
    assert!(app
        .context_state()
        .await
        .items
        .iter()
        .any(|item| item.path == "source.txt"));
    assert!(std::iter::from_fn(|| received.try_recv().ok()).any(
        |(_, event)| matches!(event, ApplicationEvent::Finished(text) if text == "Answered.")
    ));
}

#[tokio::test]
async fn applied_edit_publishes_only_the_settled_shared_context() {
    use tauqe_core::context::ContextLayer;

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    tauqe_tools::git::checkpoint::ensure_tauqe_ignored(root.path()).unwrap();
    for (path, content) in [("source.txt", "before\n"), ("user.txt", "user\n")] {
        std::fs::write(root.path().join(path), content).unwrap();
    }
    std::fs::write(root.path().join("tauqe.toml"), "[models]\nrepl = ['openrouter:mock/model']\n[project.verify]\ncommand = 'grep'\nargs = ['-qx', 'after', 'source.txt']\n").unwrap();
    tauqe_core::git::cmd::stage_files(
        root.path(),
        &["source.txt".into(), "user.txt".into(), "tauqe.toml".into()],
    )
    .unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(root.path(), "initial", false).unwrap();
    let mut config = AppConfig::default();
    config.repl.workflow = WorkflowKind::Git;
    config.context.include_repo_map = false;
    let (history, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
    let (events, mut received) = mpsc::unbounded_channel();
    let observer = Arc::new(Observer(events));
    app.add_context_file(
        "source.txt".into(),
        ContextAccess::Editable,
        None,
        observer.clone(),
    )
    .await
    .unwrap();
    app.add_context_file(
        "user.txt".into(),
        ContextAccess::ReadOnly,
        None,
        observer.clone(),
    )
    .await
    .unwrap();
    while received.try_recv().is_ok() {}
    let identity = app.session_uuid().await;
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([metered(serde_json::json!({
        "message": "Edited.",
        "action": {"kind": "edit", "commit_message": "fix: update source",
            "patch": "*** Begin Patch\n*** Update File: source.txt\n@@\n-before\n+after\n*** End Patch"}
    }).to_string())]))).await;
    app.ask(
        ModelAskInput::Prompt {
            text: "Edit source".into(),
        },
        observer,
    )
    .await
    .unwrap();
    released(&app).await;
    let final_state = app.context_state().await;
    assert_ne!(app.session_uuid().await, identity);
    assert!(final_state
        .items
        .iter()
        .any(|item| item.path == "user.txt" && item.layer == ContextLayer::User));
    let mut published = Vec::new();
    let mut finished = false;
    while let Ok((_, event)) = received.try_recv() {
        match event {
            ApplicationEvent::Context(state) => {
                assert!(!finished);
                published.push(state);
            }
            ApplicationEvent::Result { completion, .. } => {
                assert!(published.last().is_some_and(|state| state == &final_state));
                assert!(completion.error.is_none());
            }
            ApplicationEvent::Finished(_) => finished = true,
            _ => {}
        }
    }
    assert_eq!(published, [final_state]);
    assert!(finished);
}

#[tokio::test]
async fn terminal_publication_keeps_the_operation_after_accounting_closes() {
    use tokio::sync::Notify;

    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    *app.interactive_execution().terminal_gate.lock().await =
        Some((entered.clone(), resume.clone()));
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([metered(
        r#"{"message":"Done.","action":{"kind":"answer"}}"#,
    )])))
    .await;
    let (events, mut received) = mpsc::unbounded_channel();
    let started = app
        .ask(
            ModelAskInput::Prompt {
                text: "Task".into(),
            },
            Arc::new(Observer(events)),
        )
        .await
        .unwrap();
    tokio::time::timeout(DEADLINE, entered.notified())
        .await
        .unwrap();
    let id = app.active_operation_id().unwrap();
    assert_eq!(started.operation_id, format!("op-{id}"));
    assert!(app.usage_snapshot().await.cur.is_none());
    assert_eq!(app.usage_snapshot().await.prev.total_tokens, 7);
    assert!(app.try_begin_operation().is_err());
    app.abort_stalled_operation_with_deadline(id, Duration::from_millis(10))
        .await;
    assert_eq!(app.active_operation_id(), Some(id));
    resume.notify_one();
    released(&app).await;
    let terminal: Vec<_> = std::iter::from_fn(|| received.try_recv().ok())
        .filter_map(|(_, event)| match event {
            ApplicationEvent::Result { .. } => Some("result"),
            ApplicationEvent::Finished(_) => Some("finished"),
            ApplicationEvent::Stream(StreamEvent::Error(_) | StreamEvent::Cancelled) => {
                Some("interrupted")
            }
            _ => None,
        })
        .collect();
    assert_eq!(terminal, ["result", "finished"]);
    let next = app.try_begin_operation().unwrap();
    app.abort_stalled_operation_with_deadline(id, Duration::from_millis(10))
        .await;
    assert_eq!(app.active_operation_id(), Some(next.id()));
}

#[tokio::test]
async fn late_cancel_after_usage_preserves_the_saved_answer_without_success_events() {
    use tokio::sync::Notify;

    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    *app.interactive_execution().terminal_gate.lock().await =
        Some((entered.clone(), resume.clone()));
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([metered(
        r#"{"message":"Saved reply.","action":{"kind":"answer"}}"#,
    )])))
    .await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Task".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    tokio::time::timeout(DEADLINE, entered.notified())
        .await
        .unwrap();
    let id = app.active_operation_id().unwrap();
    assert_eq!(app.usage_snapshot().await.prev.total_tokens, 7);
    assert!(app.try_begin_operation().is_err());
    assert_eq!(app.cancel_active_operation(), Some(id));
    app.abort_stalled_operation_with_deadline(id, Duration::from_millis(10))
        .await;
    assert_eq!(app.active_operation_id(), Some(id));
    resume.notify_one();
    released(&app).await;
    let mut usage = 0;
    let mut terminal = Vec::new();
    while let Ok((_, event)) = received.try_recv() {
        match event {
            ApplicationEvent::Usage(snapshot) => {
                assert!(snapshot.cur.is_none());
                assert_eq!(snapshot.prev.total_tokens, 7);
                usage += 1;
            }
            ApplicationEvent::Stream(StreamEvent::Cancelled) => terminal.push("cancelled"),
            ApplicationEvent::Stream(StreamEvent::Error(_))
            | ApplicationEvent::Result { .. }
            | ApplicationEvent::Finished(_) => panic!("late cancellation must be terminal"),
            _ => {}
        }
    }
    assert_eq!(usage, 1);
    assert_eq!(terminal, ["cancelled"]);
    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.total_count, 2);
    assert_eq!(page.items[1].text, "Saved reply.");
}

#[tokio::test]
async fn late_cancel_after_saved_plan_preserves_document_without_success_events() {
    use tokio::sync::Notify;

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.txt"), "source\n").unwrap();
    let app = application(root.path());
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    *app.interactive_execution().terminal_gate.lock().await =
        Some((entered.clone(), resume.clone()));
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([metered(
        r#"{"message":"Saved plan.","action":{"kind":"plan","plan_create":{"action":"save","id":"kept","title":"Kept","concept":[{"id":"c-1","title":"Invariant"}],"items":[{"id":"1","title":"Step","files":["source.txt"]}]}}}"#,
    )])))
    .await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Create plan".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    tokio::time::timeout(DEADLINE, entered.notified())
        .await
        .unwrap();
    let id = app.active_operation_id().unwrap();
    assert!(app.get_plan(Some("kept".into())).await.unwrap().is_some());
    assert!(app.try_begin_operation().is_err());
    assert_eq!(app.cancel_active_operation(), Some(id));
    assert!(
        !app.abort_operation(id),
        "the committed document has no abortable worker"
    );
    resume.notify_one();
    released(&app).await;
    let mut saved = 0;
    let mut terminal = Vec::new();
    while let Ok((_, event)) = received.try_recv() {
        match event {
            ApplicationEvent::PlanDocument {
                plan,
                is_save: true,
            } => {
                assert_eq!(plan.id, "kept");
                saved += 1;
            }
            ApplicationEvent::Stream(StreamEvent::Cancelled) => terminal.push("cancelled"),
            ApplicationEvent::Result { .. } | ApplicationEvent::Finished(_) => {
                panic!("late cancellation must not publish success")
            }
            _ => {}
        }
    }
    assert_eq!(saved, 1);
    assert_eq!(terminal, ["cancelled"]);
    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.items[1].text, "Saved plan.");
    assert_eq!(app.usage_snapshot().await.prev.total_tokens, 7);
    assert_eq!(app.usage_snapshot().await.operations, 1);
}

#[tokio::test]
async fn cancel_after_worker_report_interrupts_history_before_completion() {
    use tokio::sync::Notify;

    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    *app.interactive_execution().report_gate.lock().await = Some((entered.clone(), resume.clone()));
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([metered(
        r#"{"message":"Accepted reply.","action":{"kind":"answer"}}"#,
    )])))
    .await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Task".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    tokio::time::timeout(DEADLINE, entered.notified())
        .await
        .unwrap();
    let id = app.active_operation_id().unwrap();
    assert_eq!(app.cancel_active_operation(), Some(id));
    assert!(app.try_begin_operation().is_err());
    resume.notify_one();
    released(&app).await;
    let mut terminal = Vec::new();
    while let Ok((_, event)) = received.try_recv() {
        match event {
            ApplicationEvent::Stream(StreamEvent::Cancelled) => terminal.push("cancelled"),
            ApplicationEvent::Stream(StreamEvent::Error(_))
            | ApplicationEvent::Result { .. }
            | ApplicationEvent::Finished(_) => panic!("cancelled report must not finish"),
            _ => {}
        }
    }
    assert_eq!(terminal, ["cancelled"]);
    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.total_count, 2);
    assert!(page.items[1].text.starts_with("Operation cancelled"));
    assert_eq!(page.items[0].rounds[0].message, "Accepted reply.");
    assert_eq!(app.usage_snapshot().await.prev.total_tokens, 7);
}

#[tokio::test]
async fn cancellation_between_technical_settlement_and_continuation_skips_plan_correction() {
    use tokio::sync::Notify;

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.txt"), "source\n").unwrap();
    let app = application(root.path());
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    *app.interactive_execution().continuation_gate.lock().await =
        Some((entered.clone(), resume.clone()));
    let provider = Arc::new(MockLlmProvider::scripted([
        metered(
            r#"{"message":"Draft.","action":{"kind":"plan","plan_create":{"action":"save","id":"not-saved","title":"Not saved","concept":[],"items":[{"id":"1","title":"Step","files":["source.txt"]}]}}}"#,
        ),
        metered(r#"{"message":"Unexpected correction.","action":{"kind":"answer"}}"#),
    ]));
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    let started = app
        .ask(
            ModelAskInput::Prompt {
                text: "Create a plan".into(),
            },
            Arc::new(Observer(events)),
        )
        .await
        .unwrap();
    tokio::time::timeout(DEADLINE, entered.notified())
        .await
        .unwrap();
    let id = app.active_operation_id().unwrap();
    assert_eq!(started.operation_id, format!("op-{id}"));
    assert!(app.try_begin_operation().is_err());
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(app.cancel_active_operation(), Some(id));
    resume.notify_one();
    released(&app).await;
    assert_eq!(provider.requests().len(), 1);
    let mut terminal = Vec::new();
    while let Ok((event_id, event)) = received.try_recv() {
        assert_eq!(event_id, started.operation_id);
        match event {
            ApplicationEvent::Stream(StreamEvent::Cancelled) => terminal.push("cancelled"),
            ApplicationEvent::Result { .. } | ApplicationEvent::Finished(_) => {
                panic!("cancelled technical result must not publish success")
            }
            ApplicationEvent::PlanDocument { .. } => panic!("correction must not save a plan"),
            _ => {}
        }
    }
    assert_eq!(terminal, ["cancelled"]);
    assert!(app
        .get_plan(Some("not-saved".into()))
        .await
        .unwrap()
        .is_none());
    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.items[0].rounds[0].message, "Draft.");
    assert!(page.items[1].text.starts_with("Operation cancelled"));
    let usage = app.usage_snapshot().await;
    assert_eq!(usage.operations, 1);
    assert_eq!(usage.prev.total_tokens, 7);
}

#[tokio::test]
async fn cancellation_during_history_registration_never_calls_the_model() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let provider = Arc::new(MockLlmProvider::new());
    app.test_set_provider(provider.clone()).await;
    let pause = app.history_session().pause_registration().await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Cancelled during registration".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    tokio::time::timeout(DEADLINE, pause.written.notified())
        .await
        .unwrap();
    assert!(app.cancel_active_operation().is_some());
    assert!(!app.abort_operation(app.active_operation_id().unwrap()));
    pause.resume.notify_one();
    released(&app).await;
    assert!(provider.requests().is_empty());
    assert_eq!(app.history_page(None, None).await.unwrap().total_count, 0);
    assert!(std::iter::from_fn(|| received.try_recv().ok())
        .any(|(_, event)| { matches!(event, ApplicationEvent::Stream(StreamEvent::Cancelled)) }));
}

#[tokio::test]
async fn stalled_scribe_is_abortable_without_aborting_its_supervisor() {
    use tauqe_core::models::ProviderKind;
    use tauqe_core::providers::{LlmProvider, LlmRequest, ProviderResult};
    use tokio::sync::Notify;

    struct StalledScribe(Arc<Notify>);

    #[async_trait::async_trait]
    impl LlmProvider for StalledScribe {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            _: LlmRequest<'_>,
            _: mpsc::Sender<StreamEvent>,
            _: tauqe_core::stop::HardCancellation,
        ) -> ProviderResult {
            self.0.notify_one();
            std::future::pending().await
        }
    }

    let root = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    config.history.budget_tokens = 60;
    config.history.tail_turns = 1;
    let (history, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
    for index in 0..4 {
        app.history_session()
            .record_history_exchange(format!("Old {index}"), vec![], "Done".into(), None)
            .await
            .unwrap();
    }
    let before = app.history_page(Some(100), None).await.unwrap();
    let entered = Arc::new(Notify::new());
    app.test_set_provider(Arc::new(StalledScribe(entered.clone())))
        .await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "New task".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    tokio::time::timeout(DEADLINE, entered.notified())
        .await
        .unwrap();
    let id = app.cancel_active_operation().unwrap();
    assert!(app.abort_operation(id));
    released(&app).await;
    let after = app.history_page(Some(100), None).await.unwrap();
    assert_eq!(after.items, before.items);
    assert!(std::iter::from_fn(|| received.try_recv().ok())
        .any(|(_, event)| { matches!(event, ApplicationEvent::Stream(StreamEvent::Cancelled)) }));
}

#[tokio::test]
async fn small_history_does_not_trigger_scribe_when_other_input_overflows() {
    let root = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    config.history.budget_tokens = 20_000;
    config.request.max_input_tokens = 1;
    let (sender, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), sender);
    app.record_history_turn("Short previous task".into(), vec![])
        .await
        .unwrap();
    let provider = Arc::new(MockLlmProvider::new());
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Large request".repeat(100),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    released(&app).await;
    assert!(
        provider.requests().is_empty(),
        "neither Scribe nor REPL should be called"
    );
    assert!(std::iter::from_fn(|| received.try_recv().ok()).any(|(_, event)| {
        matches!(event, ApplicationEvent::Result { completion, .. } if completion.error.as_deref().is_some_and(|error| error.contains("input")))
    }));
    let page = app.history_page(None, None).await.unwrap();
    assert_eq!(page.items[0].text, "Short previous task");
    assert!(!page
        .items
        .iter()
        .any(|item| item.text.contains("Earlier tasks completed")));
}

#[tokio::test]
async fn ask_records_an_answer_once_before_finished() {
    const REPLY: &str = "Unique terminal reply.";

    struct JournalObserver(Observer, std::path::PathBuf);

    impl ApplicationObserver for JournalObserver {
        fn publish(&self, id: &str, event: ApplicationEvent) {
            if matches!(&event, ApplicationEvent::Finished(_)) {
                let journal = std::fs::read_to_string(&self.1).unwrap();
                assert_eq!(journal.matches(REPLY).count(), 1);
            }
            self.0.publish(id, event);
        }
    }

    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([metered(
        serde_json::json!({"message": REPLY, "action": {"kind": "answer"}}).to_string(),
    )])))
    .await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Question".into(),
        },
        Arc::new(JournalObserver(
            Observer(events),
            root.path().join(".tauqe/history.jsonl"),
        )),
    )
    .await
    .unwrap();
    released(&app).await;

    let mut terminal = Vec::new();
    while let Ok((_, event)) = received.try_recv() {
        match event {
            ApplicationEvent::Result { completion, usage } => {
                assert!(completion.error.is_none());
                assert!(usage.cur.is_none());
                assert_eq!(usage.prev.total_tokens, 7);
                assert_eq!(
                    std::fs::read_to_string(root.path().join(".tauqe/history.jsonl"))
                        .unwrap()
                        .matches(REPLY)
                        .count(),
                    1,
                    "history must be durable before the result is published"
                );
                terminal.push("result");
            }
            ApplicationEvent::Finished(reply) => {
                assert_eq!(reply, REPLY);
                terminal.push("finished");
            }
            ApplicationEvent::Stream(StreamEvent::Error(error)) => panic!("{error}"),
            ApplicationEvent::Stream(StreamEvent::Cancelled) => panic!("unexpected cancellation"),
            _ => {}
        }
    }
    assert_eq!(terminal, ["result", "finished"]);
    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.total_count, 2);
    assert!(page.items[0].rounds.is_empty());
    assert_eq!(page.items[1].text, REPLY);
}

#[tokio::test]
async fn failed_context_merge_prevents_a_successful_terminal_answer() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    app.context_session().fail_next_settlement(true, false);
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([metered(
        r#"{"message":"Accepted reply.","action":{"kind":"answer"}}"#,
    )])))
    .await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Question".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    released(&app).await;
    let mut errors = Vec::new();
    while let Ok((_, event)) = received.try_recv() {
        match event {
            ApplicationEvent::Result { .. } | ApplicationEvent::Finished(_) => {
                panic!("a failed context settlement cannot publish success")
            }
            ApplicationEvent::Stream(StreamEvent::Error(error)) => errors.push(error),
            _ => {}
        }
    }
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("Failed to merge the turn context"));
}

#[tokio::test]
async fn ask_does_not_retry_or_publish_an_unsaved_plan_after_a_write_failure() {
    use tauqe_core::plan::storage::PlanStorage;

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.txt"), "source\n").unwrap();
    let document = PlanStorage::plans_dir(root.path()).join("unsaved.json");
    std::fs::create_dir_all(&document).unwrap();
    let app = application(root.path());
    let provider = Arc::new(MockLlmProvider::scripted([
        metered(
            r#"{"message":"Proposed plan.","action":{"kind":"plan","plan_create":{"action":"save","id":"unsaved","title":"Unsaved","concept":[{"id":"c-1","title":"Invariant"}],"items":[{"id":"1","title":"Step","files":["source.txt"]}]},"commit_message":"create plan","reason":""}}"#,
        ),
        metered(r#"{"message":"Unexpected retry.","action":{"kind":"answer"}}"#),
    ]));
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Create a plan".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    released(&app).await;

    let mut diagnostic = None;
    let mut finished = 0;
    while let Ok((_, event)) = received.try_recv() {
        match event {
            ApplicationEvent::Result { completion, usage } => {
                assert!(usage.cur.is_none());
                assert_eq!(usage.prev.total_tokens, 7);
                assert!(diagnostic
                    .replace(completion.error.expect("plan write failed"))
                    .is_none());
            }
            ApplicationEvent::Finished(reply) => {
                assert_eq!(diagnostic.as_deref(), Some(reply.as_str()));
                finished += 1;
            }
            ApplicationEvent::PlanDocument { .. } => panic!("the plan was not saved"),
            ApplicationEvent::Stream(StreamEvent::Error(error)) => panic!("{error}"),
            ApplicationEvent::Stream(StreamEvent::Cancelled) => panic!("unexpected cancellation"),
            _ => {}
        }
    }
    assert_eq!(finished, 1);
    assert_eq!(provider.requests().len(), 1);
    assert!(document.is_dir());
    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.total_count, 2);
    assert_eq!(page.items[0].rounds.len(), 1);
    assert_eq!(page.items[0].rounds[0].message, "Proposed plan.");
    assert_eq!(diagnostic.as_deref(), Some(page.items[1].text.as_str()));
    assert!(page.items[1].text.contains("Is a directory"));
}

#[cfg(unix)]
#[tokio::test]
async fn ask_distinguishes_git_finalization_failure_from_history_failure() {
    use crate::application::edit_cycle::EditCycleEvent;
    use tauqe_core::edits::VerificationVerdict;

    for history_fails in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        tauqe_core::git::init_repository(Some(path), true).unwrap();
        tauqe_tools::git::checkpoint::ensure_tauqe_ignored(path).unwrap();
        std::fs::write(path.join("a.txt"), "before\n").unwrap();
        // Verification passes; the obstruction affects only a later write.
        let obstruct = if history_fails {
            "mv .tauqe/history.jsonl .git/saved-history && mkdir .tauqe/history.jsonl"
        } else {
            "touch .git/index.lock"
        };
        let project = format!("[models]\nrepl = ['openrouter:mock/model']\n[project.verify]\ncommand = 'sh'\nargs = ['-c', \"grep -qx after a.txt && {obstruct}\"]\n");
        std::fs::write(path.join("tauqe.toml"), project).unwrap();
        tauqe_core::git::cmd::stage_files(path, &["a.txt".into(), "tauqe.toml".into()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(path, "initial", false).unwrap();
        let mut config = AppConfig::default();
        config.repl.workflow = WorkflowKind::Git;
        config.context.include_repo_map = false;
        let (history, _) = mpsc::unbounded_channel();
        let app = Application::for_repository(config, path.into(), history);
        let (events, mut received) = mpsc::unbounded_channel();
        let observer = Arc::new(Observer(events));
        app.add_context_file(
            "a.txt".into(),
            tauqe_core::context::ContextAccess::Editable,
            None,
            observer.clone(),
        )
        .await
        .unwrap();
        app.test_set_provider(Arc::new(MockLlmProvider::scripted([metered(serde_json::json!({
            "message": "Accepted edit reply.",
            "action": {"kind": "edit", "commit_message": "fix: update file",
                "patch": "*** Begin Patch\n*** Update File: a.txt\n@@\n-before\n+after\n*** End Patch"}
        }).to_string())]))).await;
        app.ask(
            ModelAskInput::Prompt {
                text: "Update a.txt".into(),
            },
            observer,
        )
        .await
        .unwrap();
        released(&app).await;

        let mut commit = None;
        let mut edit = None;
        let mut terminal = Vec::new();
        let mut flushed = false;
        while let Ok((_, event)) = received.try_recv() {
            match event {
                ApplicationEvent::Usage(snapshot) => flushed = snapshot.cur.is_none(),
                ApplicationEvent::Commit { hash, .. } => assert!(commit.replace(hash).is_none()),
                ApplicationEvent::Edit(EditCycleEvent::Finished(outcome)) => {
                    assert!(edit.replace(outcome).is_none())
                }
                ApplicationEvent::Result { completion, .. } => {
                    assert!(!history_fails);
                    assert!(completion.error.is_some());
                    terminal.push("result");
                }
                ApplicationEvent::Finished(_) => terminal.push("finished"),
                ApplicationEvent::Stream(StreamEvent::Error(error)) => {
                    assert!(history_fails, "{error}");
                    assert!(
                        error.contains("Could not record the final response"),
                        "{error}"
                    );
                    assert!(flushed);
                    assert!(commit.is_some());
                    assert!(edit.as_ref().is_some_and(|outcome| outcome.applied));
                    terminal.push("error");
                }
                ApplicationEvent::Stream(StreamEvent::Cancelled) => {
                    panic!("unexpected cancellation")
                }
                _ => {}
            }
        }
        let usage = app.usage_snapshot().await;
        assert!(usage.cur.is_none());
        assert_eq!(usage.prev.total_tokens, 7);
        assert_eq!(usage.operations, 1);
        let edit = edit.expect("the edit lifecycle must close");
        assert_eq!(edit.verification, Some(VerificationVerdict::Passed));
        assert_eq!(edit.applied, history_fails);
        assert_eq!(edit.commit_hash, commit);
        assert_eq!(
            std::fs::read_to_string(path.join("a.txt")).unwrap(),
            "after\n"
        );
        if history_fails {
            assert_eq!(terminal, ["error"]);
            assert_eq!(
                tauqe_core::git::cmd::run_git(Some(path), &["rev-parse", "--short", "HEAD"])
                    .unwrap()
                    .trim(),
                commit.as_deref().unwrap()
            );
        } else {
            assert_eq!(terminal, ["result", "finished"]);
            assert!(commit.is_none());
            let page = application(path).history_page(None, None).await.unwrap();
            assert_eq!(page.total_count, 2);
            assert_eq!(page.items[0].rounds.len(), 1);
            assert_eq!(page.items[0].rounds[0].message, "Accepted edit reply.");
            let response = &page.items[1];
            assert_eq!(Some(response.text.as_str()), edit.error.as_deref());
            assert!(response.text.contains("index.lock"), "{}", response.text);
            assert!(!response.text.contains("Accepted edit reply."));
            assert!(!response.text.to_lowercase().contains("rolled back"));
            assert!(response.commit_hash.is_none());
            assert!(response.files.is_empty());
        }
    }
}

#[tokio::test]
async fn cancelled_ask_discards_prepared_document_before_commit() {
    use tokio::sync::Notify;

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.txt"), "source\n").unwrap();
    let app = application(root.path());
    let admitted = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    *app.interactive_execution().document_write_gate.lock().await =
        Some((admitted.clone(), release.clone()));
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([metered(
        r#"{"message":"Saving plan.","action":{"kind":"plan","plan_create":{"action":"save","id":"admitted","title":"Admitted","concept":[{"id":"c-1","title":"Invariant"}],"items":[{"id":"1","title":"Step","files":["source.txt"]}]},"commit_message":"create plan","reason":""}}"#,
    )]))).await;
    let (events, mut received) = mpsc::unbounded_channel();
    let started = app
        .ask(
            ModelAskInput::Prompt {
                text: "Create a plan".into(),
            },
            Arc::new(Observer(events)),
        )
        .await
        .unwrap();
    tokio::time::timeout(DEADLINE, admitted.notified())
        .await
        .unwrap();
    let operation = app.cancel_active_operation().unwrap();
    assert_eq!(app.active_operation_id(), Some(operation));
    let terminal = |event: &ApplicationEvent| {
        matches!(
            event,
            ApplicationEvent::Result { .. }
                | ApplicationEvent::Finished(_)
                | ApplicationEvent::Stream(StreamEvent::Cancelled | StreamEvent::Error(_))
        )
    };
    assert!(tokio::time::timeout(Duration::from_millis(20), async {
        while let Some((_, event)) = received.recv().await {
            if terminal(&event) || matches!(event, ApplicationEvent::PlanDocument { .. }) {
                return;
            }
        }
    })
    .await
    .is_err());
    assert!(app.try_begin_operation().is_err());
    assert!(app
        .get_plan(Some("admitted".into()))
        .await
        .unwrap()
        .is_none());
    release.notify_one();
    released(&app).await;

    let mut saved = 0;
    let mut cancelled = 0;
    let mut notice = false;
    while let Ok((id, event)) = received.try_recv() {
        assert_eq!(id, started.operation_id);
        match event {
            ApplicationEvent::PlanDocument {
                plan,
                is_save: true,
            } => {
                assert_eq!(plan.id, "admitted");
                saved += 1;
            }
            ApplicationEvent::Stream(StreamEvent::TextDelta(text)) => {
                notice |= text.contains("Plan document already saved: admitted.");
            }
            ApplicationEvent::Stream(StreamEvent::Cancelled) => {
                assert_eq!(saved, 0);
                assert!(!notice);
                cancelled += 1;
            }
            other => assert!(!terminal(&other)),
        }
    }
    assert_eq!((saved, cancelled), (0, 1));
    assert_eq!(app.usage_snapshot().await.operations, 1);
    assert_eq!(app.usage_snapshot().await.prev.total_tokens, 7);
    assert!(
        !tauqe_core::plan::storage::PlanStorage::plans_dir(root.path())
            .join("admitted.json")
            .exists()
    );
    assert!(
        !tauqe_core::plan::storage::PlanStorage::plans_dir(root.path())
            .join("active")
            .exists()
    );
    let reopened = application(root.path());
    assert!(reopened
        .get_plan(Some("admitted".into()))
        .await
        .unwrap()
        .is_none());
    let history = reopened.history_page(None, None).await.unwrap();
    assert_eq!(history.items[0].rounds[0].message, "Saving plan.");
    assert!(history.items[1].text.starts_with("Operation cancelled"));
    assert!(!history.items[1].text.contains("already saved"));
    assert_eq!(app.usage_snapshot().await.prev.total_tokens, 7);
}

#[tokio::test]
async fn cancellation_at_a_plan_correction_round_does_not_save_the_plan() {
    use tauqe_core::models::ProviderKind;
    use tauqe_core::providers::{LlmProvider, LlmRequest, ProviderResult};
    use tokio::sync::Notify;

    struct PauseCorrection {
        first: MockLlmProvider,
        entered: Arc<Notify>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for PauseCorrection {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            request: LlmRequest<'_>,
            tx: mpsc::Sender<StreamEvent>,
            cancel: tauqe_core::stop::HardCancellation,
        ) -> ProviderResult {
            if self.first.requests().is_empty() {
                self.first.stream(request, tx, cancel).await
            } else {
                self.entered.notify_one();
                cancel.cancelled().await;
                let _ = tx.send(StreamEvent::Cancelled).await;
                Ok(())
            }
        }
    }

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.txt"), "source\n").unwrap();
    let app = application(root.path());
    let invalid = serde_json::json!({
        "message":"Draft.", "action":{"kind":"plan","plan_create":{
            "action":"save","id":"unfinished","title":"Unfinished",
            "concept":[{"id":"c-1","title":"Invariant"}],
            "items":[{"id":"1","title":"First","files":["source.txt"]},
                     {"id":"1","title":"Duplicate","files":["source.txt"]}]
        }}
    });
    let entered = Arc::new(Notify::new());
    let provider = Arc::new(PauseCorrection {
        first: MockLlmProvider::scripted([metered(invalid.to_string())]),
        entered: entered.clone(),
    });
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Create plan".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    tokio::time::timeout(DEADLINE, entered.notified())
        .await
        .unwrap();
    assert!(app.cancel_active_operation().is_some());
    released(&app).await;
    let mut cancelled = 0;
    while let Ok((_, event)) = received.try_recv() {
        match event {
            ApplicationEvent::Stream(StreamEvent::Cancelled) => cancelled += 1,
            ApplicationEvent::PlanDocument { .. }
            | ApplicationEvent::Result { .. }
            | ApplicationEvent::Finished(_) => panic!("cancelled correction published success"),
            ApplicationEvent::Stream(StreamEvent::TextDelta(text)) => {
                assert!(!text.contains("already saved"))
            }
            _ => {}
        }
    }
    assert_eq!(cancelled, 1);
    assert!(
        !tauqe_core::plan::storage::PlanStorage::plans_dir(root.path())
            .join("unfinished.json")
            .exists()
    );
    assert!(
        !tauqe_core::plan::storage::PlanStorage::plans_dir(root.path())
            .join("active")
            .exists()
    );
    assert_eq!(app.usage_snapshot().await.prev.total_tokens, 7);
    assert_eq!(provider.first.requests().len(), 1);
}
