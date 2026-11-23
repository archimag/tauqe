use super::*;
use tauqe_core::providers::StreamEvent;

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
    assert!(page.items[1].text.contains("Failed to write plan document"));
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
            observer.as_ref(),
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
async fn cancelled_ask_waits_for_an_admitted_document_write_and_publishes_it() {
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
    assert!(app.abort_operation(operation));
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
                assert_eq!(saved, 1);
                assert!(notice);
                cancelled += 1;
            }
            other => assert!(!terminal(&other)),
        }
    }
    assert_eq!((saved, cancelled), (1, 1));
    let reopened = application(root.path());
    assert!(reopened
        .get_plan(Some("admitted".into()))
        .await
        .unwrap()
        .is_some());
    let history = reopened.history_page(None, None).await.unwrap();
    assert_eq!(history.items[0].rounds[0].message, "Saving plan.");
    assert!(history.items[1].text.starts_with("Operation cancelled"));
    assert!(history.items[1]
        .text
        .contains("Plan document already saved: admitted."));
    assert_eq!(app.usage_snapshot().await.prev.total_tokens, 7);
}
