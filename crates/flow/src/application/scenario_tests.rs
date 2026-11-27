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

mod batch_routes;
mod repl_settlement;
mod review_refinement;

#[tokio::test]
async fn plan_reconciliation_uses_the_escalated_planner_tier() {
    use crate::api::PlanSynthesisInput;
    use tauqe_core::models::{ModelRef, ModelSelection};
    use tauqe_core::plan::PlanSynthesisResult;

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    let mut config: AppConfig = toml::from_str(
        "[models]\nrepl = ['openrouter:planner/first']\narchitect = [{ model = 'openrouter:planner/first', attempts = 1 }, { model = 'openrouter:planner/second', attempts = 1 }]\n",
    ).unwrap();
    config.context.include_repo_map = false;
    let (history, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
    let horizon = |title: &str| {
        metered(
            serde_json::json!({
                "status": "success", "explanation": title, "reason": null,
                "abort": {"is_aborted": false, "reason": "", "recommendation": ""},
                "context_requests": [], "repo_searches": [], "repo_maps": [],
                "items": [{"id": "1", "title": title, "status": "todo", "files": ["source.rs"]}]
            })
            .to_string(),
        )
    };
    let provider = Arc::new(MockLlmProvider::scripted([
        metered("not a plan response"),
        horizon("Draft"),
        metered(
            r#"{"verdict":"changes_required","summary":"Repair","findings":[{"category":"decomposition_defect","target":"1","detail":"Repair the step"}]}"#,
        ),
        horizon("Reconciled"),
    ]));
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    let result = app
        .synthesize_plan(
            PlanSynthesisInput::Create {
                directives: "Plan a source change".into(),
                planner_model: None,
                critic_model: Some(ModelSelection::Specific(ModelRef::openrouter(
                    "critic/model",
                ))),
                skip_critique: false,
            },
            Arc::new(Observer(events)),
            DEADLINE,
        )
        .await
        .unwrap();
    let PlanSynthesisResult::Success { plan, .. } = result else {
        panic!("synthesis must succeed")
    };
    assert_eq!(plan.implementation.items[0].title, "Reconciled");
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.model.as_str())
            .collect::<Vec<_>>(),
        [
            "planner/first",
            "planner/second",
            "critic/model",
            "planner/second"
        ]
    );
    assert_eq!(requests[1].session_id, requests[3].session_id);
    let mut reconciliation_header = false;
    while let Ok((_, event)) = received.try_recv() {
        if let ApplicationEvent::Stream(tauqe_core::providers::StreamEvent::TextDelta(text)) = event
        {
            reconciliation_header |= text
                .contains("Reconciling the independent critique with openrouter:planner/second");
        }
    }
    assert!(reconciliation_header);
    assert_eq!(app.usage_snapshot().await.operations, 3);
}

const DEADLINE: Duration = Duration::from_secs(5);

fn metered(text: impl Into<String>) -> MockResponse {
    MockResponse::text(text).with_usage(tauqe_core::providers::ModelUsageInfo {
        prompt_tokens: 3,
        completion_tokens: 4,
        total_tokens: 7,
        ..Default::default()
    })
}

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
async fn first_request_overflow_never_calls_provider_or_publishes_an_answer() {
    let root = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    config.request.max_input_tokens = 1;
    let (history, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
    let provider = Arc::new(MockLlmProvider::new());
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Do not truncate this task".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    released(&app).await;
    assert!(provider.requests().is_empty());
    let mut failures = 0;
    while let Ok((_, event)) = received.try_recv() {
        match event {
            ApplicationEvent::Result { completion, .. } => {
                assert!(completion.error.as_deref().unwrap().contains("input"));
                failures += 1;
            }
            ApplicationEvent::Finished(text) => assert!(text.contains("input")),
            _ => {}
        }
    }
    assert_eq!(failures, 1);
    assert!(app.try_begin_operation().is_ok());
}

#[tokio::test]
async fn discovered_file_overflow_retains_the_accepted_round_without_a_second_call() {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    let marker = "discovered material must not be truncated".repeat(10_000);
    std::fs::write(root.path().join("large.rs"), &marker).unwrap();
    tauqe_core::git::cmd::stage_files(root.path(), &["large.rs".into()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(root.path(), "initial", false).unwrap();
    let mut config: AppConfig =
        toml::from_str("[models]\nrepl = ['openrouter:mock/model']\n").unwrap();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    config.request.max_input_tokens = 30_000;
    let (history, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
    let provider = Arc::new(MockLlmProvider::scripted([metered(
        r#"{"message":"Research accepted.","action":{"kind":"research","context_requests":[{"path":"large.rs","access":"read_only"}]}}"#,
    )]));
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Read the full file".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    released(&app).await;
    assert_eq!(provider.requests().len(), 1);
    assert!(provider.requests()[0]
        .messages
        .iter()
        .all(|m| !m.content.contains(&marker)));
    let mut failed = false;
    while let Ok((_, event)) = received.try_recv() {
        if let ApplicationEvent::Result { completion, .. } = event {
            assert!(completion.error.as_deref().unwrap().contains("input"));
            failed = true;
        }
    }
    assert!(failed);
    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.items[0].rounds[0].message, "Research accepted.");
    assert!(!page
        .items
        .iter()
        .any(|item| item.text == "Research accepted."));
    assert_eq!(
        std::fs::read_to_string(root.path().join("large.rs")).unwrap(),
        marker
    );
    assert!(!root.path().join(".tauqe/active-transaction.json").exists());
    assert!(app.try_begin_operation().is_ok());
}

#[tokio::test]
async fn ask_returns_the_published_operation_id_and_settles_headlessly() {
    const QUESTION: &str = "  Question\n ";
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let session_id = format!("tauqe-repl-{}", app.session_uuid().await);
    let effort = app
        .resolve_turn_model(None, None, tauqe_core::models::ModelRole::REPL)
        .await
        .effort;
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        r#"{"message":"Reply","action":{"kind":"answer"}}"#,
    )]));
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    let started = app
        .ask(
            ModelAskInput::Prompt {
                text: QUESTION.into(),
            },
            Arc::new(Observer(events)),
        )
        .await
        .unwrap();
    let (id, event) = received.recv().await.unwrap();
    assert_eq!(id, started.operation_id);
    assert!(matches!(event, ApplicationEvent::Started { .. }));
    tokio::time::timeout(DEADLINE, async {
        let mut results = 0;
        let mut finished = 0;
        let mut flushed = false;
        let mut text = String::new();
        while let Some((id, event)) = received.recv().await {
            assert_eq!(id, started.operation_id);
            match event {
                ApplicationEvent::UsageTotals(snapshot) => flushed = snapshot.cur.is_none(),
                ApplicationEvent::Stream(tauqe_core::providers::StreamEvent::TextDelta(delta)) => {
                    text.push_str(&delta)
                }
                ApplicationEvent::Result { completion, .. } => {
                    assert!(completion.error.is_none());
                    assert_eq!(
                        completion.result,
                        tauqe_core::llm::ModelResult::Answer {
                            text: "Reply".into()
                        }
                    );
                    results += 1;
                }
                ApplicationEvent::Finished(reply) => {
                    assert_eq!(reply, "Reply");
                    assert!(flushed);
                    assert_eq!(results, 1);
                    finished += 1;
                }
                _ => {}
            }
        }
        assert_eq!((results, finished), (1, 1));
        assert_eq!(text, "Reply");
    })
    .await
    .unwrap();
    released(&app).await;
    assert_eq!(app.usage_snapshot().await.operations, 1);
    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.total_count, 2);
    assert!(page.items[0].rounds.is_empty());
    assert_eq!(page.items[0].text, QUESTION);
    assert_eq!(page.items[1].text, "Reply");
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].session_id.as_deref(), Some(session_id.as_str()));
    assert_eq!(requests[0].reasoning_effort, effort);
    let schema = serde_json::to_string(&requests[0].response_format).unwrap();
    assert!(schema.contains("\"plan_create\""));
    assert!(!schema.contains("\"plan_update\""));
    assert!(!schema.contains("\"review_update\""));
    assert!(requests[0]
        .messages
        .iter()
        .any(|message| message.role == "user" && message.content.contains(QUESTION)));
}

#[tokio::test]
async fn started_identity_matches_the_request_for_role_and_explicit_selection() {
    use tauqe_core::config::OpenRouterConfig;
    use tauqe_core::models::{
        ModelCatalogEntry, ModelEntry, ModelRef, ModelRole, ModelSelection, ReasoningEffort,
    };

    for (selection, role, effort) in [
        (
            ModelSelection::Role(ModelRole::Architect),
            Some(ModelRole::Architect),
            Some(ReasoningEffort::High),
        ),
        (
            ModelSelection::Specific(ModelRef::openrouter("other/model")),
            None,
            None,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.context.include_repo_map = false;
        config.models.repl = vec![ModelEntry::new(ModelRef::openrouter("base/model"), None)];
        config.models.architect = Some(vec![ModelEntry::new(
            ModelRef::openrouter("other/model"),
            Some(ReasoningEffort::High),
        )]);
        config.providers.openrouter = Some(OpenRouterConfig {
            api_key: None,
            models: vec![
                ModelCatalogEntry {
                    model: ModelRef::openrouter("base/model"),
                    alias: Some("Base".into()),
                },
                ModelCatalogEntry {
                    model: ModelRef::openrouter("other/model"),
                    alias: Some("Other".into()),
                },
            ],
        });
        let (history, _) = mpsc::unbounded_channel();
        let app = Application::for_repository(config, root.path().into(), history);
        app.begin_configuration_change()
            .unwrap()
            .select_model(selection)
            .await
            .unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([metered(
            r#"{"message":"Reply","action":{"kind":"answer"}}"#,
        )]));
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
        let started = std::iter::from_fn(|| received.try_recv().ok())
            .find_map(|(_, event)| match event {
                ApplicationEvent::Started {
                    model,
                    role,
                    alias,
                    effort,
                    ..
                } => Some((model, role, alias, effort)),
                _ => None,
            })
            .unwrap();
        let request = &provider.requests()[0];
        assert_eq!(started.0.name, request.model);
        assert_eq!(started.1, role);
        assert_eq!(started.2.as_deref(), Some("Other"));
        assert_eq!(started.3, request.reasoning_effort);
        assert_eq!(started.3, effort);
    }
}

#[tokio::test]
async fn ask_records_only_the_original_discussion_comment() {
    const TITLE: &str = "Unique plan title for dialogue";
    const STEP: &str = "Selected implementation step";
    const COMMENT_MARKER: &str = "Unique developer comment";
    const COMMENT: &str = "  Unique developer comment\nSecond line\n ";
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.txt"), "source\n").unwrap();
    tauqe_core::plan::storage::PlanStorage::save_plan(
        root.path(),
        &Plan::from_parts(
            "dialogue".into(),
            TITLE.into(),
            None,
            0,
            0,
            vec![],
            vec![PlanItem {
                id: "1".into(),
                title: STEP.into(),
                readonly_files: vec!["source.txt".into()],
                ..Default::default()
            }],
        ),
    )
    .unwrap();
    let app = application(root.path());
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        r#"{"message":"Reply","action":{"kind":"answer"}}"#,
    )]));
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Discussion {
            target: DiscussionTarget::Plan {
                id: "dialogue".into(),
            },
            focused_items: vec!["1".into()],
            comment: COMMENT.into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    released(&app).await;
    assert!(
        std::iter::from_fn(|| received.try_recv().ok()).any(|(_, event)| {
            matches!(event, ApplicationEvent::Finished(reply) if reply == "Reply")
        })
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let messages = &requests[0].messages;
    assert!(messages.iter().any(|message| message.role == "user"
        && message.content.contains(TITLE)
        && message.content.contains(&format!("{STEP} [FOCUSED]"))
        && message.content.contains(COMMENT.trim())));
    assert_eq!(
        messages
            .iter()
            .map(|message| message.content.matches(COMMENT_MARKER).count())
            .sum::<usize>(),
        1
    );

    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.total_count, 2);
    assert_eq!(page.items[0].text, COMMENT);
    assert_eq!(page.items[1].text, "Reply");
}

#[tokio::test]
async fn ask_refuses_an_explicit_missing_plan_without_changing_focus_or_calling_the_provider() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let provider = Arc::new(MockLlmProvider::new());
    app.test_set_provider(provider.clone()).await;
    let previous = DiscussionFocus::Focused {
        target: DiscussionTarget::Plan {
            id: "previous".into(),
        },
        focused_items: vec!["1".into()],
    };
    app.set_discussion_focus(previous.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    let result = app
        .ask(
            ModelAskInput::Discussion {
                target: DiscussionTarget::Plan {
                    id: "missing".into(),
                },
                focused_items: vec![],
                comment: String::new(),
            },
            Arc::new(Observer(events)),
        )
        .await;
    assert!(matches!(
        result,
        Err(AskError::Prompt(
            crate::api::DiscussionPromptError::NotFound(_)
        ))
    ));
    assert_eq!(app.discussion_focus().await, previous);
    assert!(app.active_operation_id().is_none());
    assert!(provider.requests().is_empty());
    assert!(received.try_recv().is_err());
}

#[tokio::test]
async fn ask_refuses_to_execute_when_the_turn_cannot_be_recorded() {
    use tauqe_core::providers::StreamEvent;

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.txt"), "source\n").unwrap();
    std::fs::create_dir_all(root.path().join(".tauqe/history.jsonl")).unwrap();
    let mut config: AppConfig =
        toml::from_str("[models]\nrepl = ['openrouter:mock/model']\n").unwrap();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    let (history, _history_events) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        r#"{"message":"Saved.","action":{"kind":"plan","plan_create":{"action":"save","id":"unrecorded","title":"Unrecorded","concept":[{"id":"c-1","title":"Invariant"}],"items":[{"id":"1","title":"Step","files":["source.txt"]}]},"commit_message":"create plan","reason":""}}"#,
    )]));
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
    released(&app).await;

    let mut errors = Vec::new();
    while let Ok((id, event)) = received.try_recv() {
        assert_eq!(id, started.operation_id);
        match event {
            ApplicationEvent::Stream(StreamEvent::Error(message)) => errors.push(message),
            ApplicationEvent::Result { .. }
            | ApplicationEvent::Finished(_)
            | ApplicationEvent::PlanDocument { .. } => {
                panic!("an unrecorded turn must not publish success or a saved plan")
            }
            _ => {}
        }
    }
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("Could not start interactive turn history"));
    assert!(errors[0].contains("history.jsonl"));
    assert!(provider.requests().is_empty());
    assert!(app
        .get_plan(Some("unrecorded".into()))
        .await
        .unwrap()
        .is_none());
    assert!(app.try_begin_operation().is_ok());
}

#[tokio::test]
async fn ask_records_equal_replies_separately_and_keeps_the_round_before_a_budget_error() {
    const PROGRESS: &str = "Same reply.";
    for budget in [1, 2] {
        let root = tempfile::tempdir().unwrap();
        tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
        std::fs::write(root.path().join("source.txt"), "source\n").unwrap();
        let mut config: AppConfig = toml::from_str(&format!(
            "[models]\nrepl = ['openrouter:mock/model']\n[repl]\nmax_turn_rounds = {budget}\n"
        ))
        .unwrap();
        config.context.include_repo_map = false;
        let (history, _) = mpsc::unbounded_channel();
        let app = Application::for_repository(config, root.path().into(), history);
        let mut scripts = vec![metered(serde_json::json!({
            "message": PROGRESS,
            "action": {"kind":"research", "context_requests":[{"path":"source.txt","access":"read_only"}]}
        }).to_string())];
        if budget == 2 {
            scripts.push(metered(
                serde_json::json!({
                    "message": PROGRESS, "action": {"kind":"answer"}
                })
                .to_string(),
            ));
        }
        let provider = Arc::new(MockLlmProvider::scripted(scripts));
        app.test_set_provider(provider.clone()).await;
        let (events, _received) = mpsc::unbounded_channel();
        app.ask(
            ModelAskInput::Prompt {
                text: "Read the source".into(),
            },
            Arc::new(Observer(events)),
        )
        .await
        .unwrap();
        released(&app).await;
        assert_eq!(provider.requests().len(), budget);
        let page = application(root.path())
            .history_page(None, None)
            .await
            .unwrap();
        assert_eq!(page.total_count, 2);
        assert_eq!(page.items[0].rounds.len(), 1);
        assert_eq!(page.items[0].rounds[0].message, PROGRESS);
        if budget == 1 {
            assert!(page.items[1]
                .text
                .contains("budget was exhausted after 1 call(s)"));
            assert!(!page.items[1].text.contains(PROGRESS));
        } else {
            assert_eq!(page.items[1].text, PROGRESS);
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn ask_keeps_both_patch_replies_in_order_without_repeating_the_edit_source() {
    use tauqe_core::llm::ModelResult;
    use tauqe_core::providers::StreamEvent;

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    tauqe_tools::git::checkpoint::ensure_tauqe_ignored(root.path()).unwrap();
    std::fs::write(root.path().join("a.txt"), "before\n").unwrap();
    std::fs::write(root.path().join("helper.txt"), "original helper\n").unwrap();
    std::fs::write(
        root.path().join("tauqe.toml"),
        "[models]\nrepl = ['openrouter:mock/model']\n[project.verify]\ncommand = 'grep'\nargs = ['-qx', 'after', 'a.txt']\n",
    )
    .unwrap();
    tauqe_core::git::cmd::stage_files(
        root.path(),
        &["a.txt".into(), "helper.txt".into(), "tauqe.toml".into()],
    )
    .unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(root.path(), "initial", false).unwrap();
    let mut config: AppConfig = toml::from_str(
        "[models]\nrepl = ['openrouter:mock/model']\n[repl]\nworkflow = 'git'\nmax_turn_rounds = 2\n",
    )
    .unwrap();
    config.context.include_repo_map = false;
    let (history, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
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
    app.add_context_file(
        "helper.txt".into(),
        tauqe_core::context::ContextAccess::Editable,
        None,
        observer.clone(),
    )
    .await
    .unwrap();
    let patch = |old, message, commit_message| {
        let other_hunk = if old == "absent fragment" {
            "*** Update File: helper.txt\n@@\n-original helper\n+refused helper\n"
        } else {
            ""
        };
        metered(serde_json::json!({
            "message": message, "scratchpad": "private patch note",
            "action": {"kind":"edit", "commit_message":commit_message,
                "patch":format!("*** Begin Patch\n{other_hunk}*** Update File: a.txt\n@@\n-{old}\n+after\n*** End Patch")}
        }).to_string())
    };
    let provider = Arc::new(MockLlmProvider::scripted([
        patch("absent fragment", "M1", "fix: change the tracked file"),
        patch("before", "M2", "fix: repair the patch"),
    ]));
    app.test_set_provider(provider.clone()).await;
    app.ask(
        ModelAskInput::Prompt {
            text: "Change before to after".into(),
        },
        observer,
    )
    .await
    .unwrap();
    released(&app).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]
        .messages
        .iter()
        .any(|message| message.role == "user"
            && message.content.contains("Rejected patch")
            && message.content.contains("absent fragment")));
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "after\n"
    );
    let mut commit = None;
    let mut accepted_preview = false;
    while let Ok((_, event)) = received.try_recv() {
        if let ApplicationEvent::Stream(StreamEvent::EditHunk { path, new_text, .. }) = &event {
            assert_eq!(
                path, "a.txt",
                "a refused patch must not publish a partial preview"
            );
            accepted_preview |= new_text.contains("after");
        }
        if let ApplicationEvent::Result { completion, .. } = event {
            let ModelResult::Edit { outcome, .. } = completion.result else {
                panic!("expected an edit result");
            };
            assert!(outcome.applied);
            assert_eq!(
                outcome.verification,
                Some(tauqe_core::edits::VerificationVerdict::Passed)
            );
            assert!(commit.replace(outcome.commit_hash.unwrap()).is_none());
        }
    }
    assert!(accepted_preview);
    assert_eq!(
        std::fs::read_to_string(root.path().join("helper.txt")).unwrap(),
        "original helper\n"
    );
    let commit = commit.expect("edit must publish a commit");
    assert_eq!(
        tauqe_core::git::cmd::run_git(Some(root.path()), &["log", "-1", "--format=%s"])
            .unwrap()
            .trim(),
        "fix: change the tracked file"
    );
    let page = application(root.path())
        .history_page(None, None)
        .await
        .unwrap();
    assert_eq!(page.total_count, 2);
    let rounds: Vec<_> = page.items[0]
        .rounds
        .iter()
        .map(|round| (round.index, round.message.as_str()))
        .collect();
    assert_eq!(rounds, [(1, "M1"), (2, "M2")]);
    assert!(page.items[1].text.is_empty());
    assert_eq!(page.items[1].commit_hash.as_deref(), Some(commit.as_str()));
    assert_eq!(page.items[1].files[0].path, "a.txt");
    let journal = std::fs::read_to_string(root.path().join(".tauqe/history.jsonl")).unwrap();
    assert!(!journal.contains("private patch note"));
    assert!(!journal.contains("*** Begin Patch"));
}

#[tokio::test]
async fn research_rounds_are_shown_replayed_and_persisted_even_if_the_last_call_fails() {
    use tauqe_core::providers::StreamEvent;
    for interrupted in [false, true] {
        let root = tempfile::tempdir().unwrap();
        tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
        let paths = ["one.rs", "two.rs", "three.rs", "four.rs"];
        for path in paths {
            std::fs::write(root.path().join(path), "// source").unwrap();
        }
        tauqe_core::git::cmd::stage_files(root.path(), &paths.map(String::from)).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(root.path(), "Add sources", false)
            .unwrap();
        let app = application(root.path());
        let provider = Arc::new(MockLlmProvider::scripted([
            metered(
                r#"{"message":"Opening.","scratchpad":"first-round marker","action":{"kind":"research","context_requests":[{"path":"one.rs","access":"read_only"},{"path":"two.rs","access":"read_only"}]}}"#,
            ),
            metered(
                r#"{"message":"","scratchpad":"private note","action":{"kind":"research","message":"Intermediate.","context_requests":[{"path":"three.rs","access":"read_only"},{"path":"four.rs","access":"read_only"}]}}"#,
            ),
            if interrupted {
                MockResponse::error("offline")
            } else {
                metered(r#"{"message":"Final.","action":{"kind":"answer"}}"#)
            },
        ]));
        app.test_set_provider(provider.clone()).await;
        let (events, mut received) = mpsc::unbounded_channel();
        app.ask(
            ModelAskInput::Prompt {
                text: "Load a pair of files and then another pair".into(),
            },
            Arc::new(Observer(events)),
        )
        .await
        .unwrap();
        released(&app).await;
        let mut live = Vec::new();
        let mut terminals = 0;
        let mut flushed = false;
        while let Ok((_, event)) = received.try_recv() {
            match event {
                ApplicationEvent::Usage(snapshot) => flushed = snapshot.cur.is_none(),
                ApplicationEvent::Result { usage, .. } => {
                    assert!(flushed);
                    assert!(usage.cur.is_none());
                    terminals += 1;
                }
                ApplicationEvent::Stream(StreamEvent::TurnRoundStarted { .. }) => {
                    live.push(String::new())
                }
                ApplicationEvent::Stream(StreamEvent::TextDelta(delta)) => {
                    live.last_mut().unwrap().push_str(&delta)
                }
                _ => {}
            }
        }
        assert_eq!(live.len(), 3);
        assert_eq!(terminals, usize::from(!interrupted));
        let usage = app.usage_snapshot().await;
        assert!(usage.cur.is_none());
        assert_eq!(usage.operations, 1);
        assert_eq!(usage.prev.total_tokens, if interrupted { 14 } else { 21 });
        for path in paths {
            assert_eq!(
                std::fs::read_to_string(root.path().join(path)).unwrap(),
                "// source"
            );
        }
        assert!(!root.path().join(".tauqe/active-transaction.json").exists());
        for (section, message) in live.iter().zip(["Opening.", "Intermediate."]) {
            assert!(section.starts_with(message));
            assert_eq!(section.matches(message).count(), 1);
        }

        for note in ["first-round marker", "private note"] {
            assert!(live.iter().all(|section| !section.contains(note)));
        }
        let requests = provider.requests();
        for (index, request) in requests.iter().enumerate() {
            let replies: Vec<_> = request
                .messages
                .iter()
                .filter(|m| m.role == "assistant")
                .map(|m| m.content.as_str())
                .collect();
            assert_eq!(
                replies,
                [
                    "Opening.\n\n<scratchpad>first-round marker</scratchpad>",
                    "Intermediate.\n\n<scratchpad>private note</scratchpad>",
                ][..index]
            );
            for batch in 0..index {
                let marker = format!("<context_feedback round=\"{}\">", batch + 1);
                let result = request
                    .messages
                    .iter()
                    .find(|msg| msg.role == "user" && msg.content.starts_with(&marker))
                    .unwrap();
                for path in &paths[batch * 2..batch * 2 + 2] {
                    assert!(result.content.contains(&format!("<path>{path}</path>")));
                }
                if batch + 1 == index {
                    assert_eq!(request.messages.last().unwrap(), result);
                }
            }
        }
        let journal = std::fs::read_to_string(root.path().join(".tauqe/history.jsonl")).unwrap();
        assert!(!journal.contains("first-round marker"));
        assert!(!journal.contains("private note"));
        let reopened = application(root.path());
        let page = reopened.history_page(None, None).await.unwrap();
        assert_eq!(page.total_count, 2);
        let turn = page
            .items
            .iter()
            .find(|item| item.text == "Load a pair of files and then another pair")
            .unwrap();
        let rounds: Vec<_> = turn
            .rounds
            .iter()
            .map(|r| (r.index, r.message.as_str()))
            .collect();
        assert_eq!(rounds, [(1, "Opening."), (2, "Intermediate.")]);
        let terminal = page.items.iter().find(|item| item.id != turn.id).unwrap();
        if interrupted {
            assert!(terminal.text.contains("offline"));
        } else {
            assert_eq!(live[2], "Final.");
            assert_eq!(terminal.text, "Final.");
        }
    }
}

#[tokio::test]
async fn ask_plan_correction_publishes_only_the_saved_plan_and_one_terminal_answer() {
    use tauqe_core::providers::StreamEvent;
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.txt"), "developer work\n").unwrap();
    let mut config: AppConfig = toml::from_str("[repl]\nmax_turn_rounds = 2\n").unwrap();
    config.context.include_repo_map = false;
    let (history, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
    let payload = |valid, message| {
        serde_json::json!({
        "message": message, "scratchpad": "private correction marker",
        "action": {"kind":"plan", "plan_create": {
            "action":"save", "id":"corrected", "title":"Corrected",
            "concept": if valid { vec![serde_json::json!({"id":"c-1","title":"Invariant"})] } else { vec![] },
            "items":[{"id":"1","title":"Step","files":["source.txt"]}]
        }, "commit_message":"create plan", "reason":""}
    }).to_string()
    };
    let provider = Arc::new(MockLlmProvider::scripted([
        metered(payload(false, "Draft.")),
        metered(payload(true, "Saved.")),
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
    released(&app).await;
    let mut rounds = Vec::new();
    let mut visible = String::new();
    let mut saved = 0;
    let mut results = 0;
    let mut finished = 0;
    let mut flushed = false;
    while let Ok((id, event)) = received.try_recv() {
        assert_eq!(id, started.operation_id);
        match event {
            ApplicationEvent::Stream(StreamEvent::TurnRoundStarted {
                round, max_rounds, ..
            }) => {
                assert_eq!(max_rounds, Some(2));
                rounds.push(round);
            }
            ApplicationEvent::Stream(StreamEvent::TextDelta(text)) => visible.push_str(&text),
            ApplicationEvent::Usage(snapshot) => flushed = snapshot.cur.is_none(),
            ApplicationEvent::PlanDocument { plan, .. } => {
                assert_eq!(plan.id, "corrected");
                saved += 1;
            }
            ApplicationEvent::Result { completion, usage } => {
                assert!(flushed);
                assert_eq!(saved, 1);
                assert!(completion.plan_rejections.is_empty());
                assert_eq!(usage.prev.total_tokens, 14);
                results += 1;
            }
            ApplicationEvent::Finished(text) => {
                assert_eq!(results, 1);
                assert_eq!(text, "Saved.");
                finished += 1;
            }
            ApplicationEvent::Edit(_) | ApplicationEvent::Commit { .. } => {
                panic!("document correction opened an edit lifecycle")
            }
            _ => {}
        }
    }
    assert_eq!(rounds, [1, 2]);
    assert_eq!((saved, results, finished), (1, 1, 1));
    assert!(!visible.contains("private correction marker"));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let correction = &requests[1];
    assert!(correction
        .messages
        .iter()
        .any(|m| m.content.contains("[Plan Correction 1]")));
    assert!(correction
        .messages
        .iter()
        .any(|m| m.role == "assistant" && m.content.contains("private correction marker")));
    let schema = serde_json::to_string(&correction.response_format).unwrap();
    for allowed in ["plan_create", "scratchpad", "message", "blocked", "abort"] {
        assert!(schema.contains(allowed));
    }
    for forbidden in [
        "context_requests",
        "repo_searches",
        "review_update",
        "\"patch\"",
    ] {
        assert!(!schema.contains(forbidden));
    }
    assert!(app
        .get_plan(Some("corrected".into()))
        .await
        .unwrap()
        .is_some());
    let reopened = application(root.path());
    let page = reopened.history_page(None, None).await.unwrap();
    assert_eq!(page.total_count, 2);
    let turn = page
        .items
        .iter()
        .find(|item| item.text == "Create a plan")
        .unwrap();
    assert_eq!(turn.rounds.len(), 1);
    assert_eq!(turn.rounds[0].message, "Draft.");
    assert_eq!(
        page.items
            .iter()
            .filter(|item| item.text == "Saved.")
            .count(),
        1
    );
    let journal = std::fs::read_to_string(root.path().join(".tauqe/history.jsonl")).unwrap();
    assert!(!journal.contains("private correction marker"));
    assert!(!journal.contains("plan_create"));
    assert_eq!(app.usage_snapshot().await.operations, 1);
    assert_eq!(
        std::fs::read_to_string(root.path().join("source.txt")).unwrap(),
        "developer work\n"
    );
    assert!(!root.path().join(".tauqe/active-transaction.json").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn ask_heals_verification_before_publishing_a_committed_edit() {
    use crate::application::edit_cycle::EditCycleEvent;
    use tauqe_core::providers::{StreamEvent, TurnRequestReason};
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    // This headless fixture bypasses repository startup. Install its runtime
    // exclusions before history exists and the turn snapshots the worktree.
    tauqe_tools::git::checkpoint::ensure_tauqe_ignored(root.path()).unwrap();
    std::fs::write(root.path().join("a.txt"), "committed\n").unwrap();
    std::fs::write(root.path().join("unrelated.txt"), "original\n").unwrap();
    std::fs::write(root.path().join("tauqe.toml"), "[models]\nrepl = ['openrouter:mock/model']\n[project.verify]\ncommand = 'grep'\nargs = ['-qx', 'accepted', 'a.txt']\n").unwrap();
    tauqe_core::git::cmd::stage_files(
        root.path(),
        &["a.txt".into(), "unrelated.txt".into(), "tauqe.toml".into()],
    )
    .unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(root.path(), "initial", false).unwrap();
    std::fs::write(root.path().join("a.txt"), "developer work\n").unwrap();
    std::fs::write(root.path().join("unrelated.txt"), "keep dirty\n").unwrap();
    let mut config = AppConfig::default();
    config.repl.workflow = WorkflowKind::Git;
    config.context.include_repo_map = false;
    let (history, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
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
    while received.try_recv().is_ok() {}
    let patch = |old, new, message| {
        metered(serde_json::json!({
        "message":message, "scratchpad":"private healing marker",
        "action":{"kind":"edit", "commit_message":"fix: accept content",
            "patch":format!("*** Begin Patch\n*** Update File: a.txt\n@@\n-{old}\n+{new}\n*** End Patch")}
    }).to_string())
    };
    let provider = Arc::new(MockLlmProvider::scripted([
        patch("developer work", "broken", "First attempt."),
        patch("broken", "accepted", "Verified."),
    ]));
    app.test_set_provider(provider.clone()).await;
    app.ask(
        ModelAskInput::Prompt {
            text: "Fix the content".into(),
        },
        observer,
    )
    .await
    .unwrap();
    released(&app).await;
    let mut rounds = Vec::new();
    let mut lifecycle = (0, 0);
    let mut commit = None;
    let mut results = 0;
    let mut finished = 0;
    let mut flushed = false;
    while let Ok((_, event)) = received.try_recv() {
        match event {
            ApplicationEvent::Stream(StreamEvent::TurnRoundStarted { round, reason, .. }) => {
                rounds.push((round, reason))
            }
            ApplicationEvent::Stream(StreamEvent::TextDelta(text)) => {
                assert!(!text.contains("private healing marker"))
            }
            ApplicationEvent::Usage(snapshot) => flushed = snapshot.cur.is_none(),
            ApplicationEvent::Edit(EditCycleEvent::Started) => lifecycle.0 += 1,
            ApplicationEvent::Edit(EditCycleEvent::Finished(outcome)) => {
                assert!(outcome.applied, "healing did not apply: {outcome:?}");
                assert_eq!(
                    outcome.verification,
                    Some(tauqe_core::edits::VerificationVerdict::Passed)
                );
                lifecycle.1 += 1;
            }
            ApplicationEvent::Commit { hash, .. } => {
                assert!(commit.replace(hash).is_none());
            }
            ApplicationEvent::Result { completion, usage } => {
                assert!(flushed);
                assert_eq!(lifecycle, (1, 1));
                assert_eq!(usage.prev.total_tokens, 14);
                let tauqe_core::llm::ModelResult::Edit { outcome, .. } = completion.result else {
                    panic!("expected edit")
                };
                assert!(outcome.applied);
                assert_eq!(outcome.commit_hash, commit);
                results += 1;
            }
            ApplicationEvent::Finished(_) => {
                assert_eq!(results, 1);
                finished += 1;
            }
            _ => {}
        }
    }
    assert_eq!(
        rounds,
        [(1, None), (2, Some(TurnRequestReason::VerificationRepair))]
    );
    assert_eq!((results, finished), (1, 1));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.starts_with(&requests[0].messages));
    assert!(requests[1]
        .messages
        .iter()
        .any(|m| m.content.contains("Code verification failed")));
    assert!(requests[1]
        .messages
        .iter()
        .any(|m| m.content.contains("<current_versions>") && m.content.contains("broken")));
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "accepted\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("unrelated.txt")).unwrap(),
        "keep dirty\n"
    );
    assert_eq!(
        tauqe_core::git::cmd::run_git(Some(root.path()), &["rev-parse", "--short", "HEAD"])
            .unwrap()
            .trim(),
        commit.as_deref().unwrap()
    );
    assert!(!root.path().join(".tauqe/active-transaction.json").exists());
    let reopened = application(root.path());
    let page = reopened.history_page(None, None).await.unwrap();
    assert_eq!(page.total_count, 2);
    let turn = page
        .items
        .iter()
        .find(|item| item.text == "Fix the content")
        .unwrap();
    assert_eq!(
        tauqe_core::git::cmd::run_git(
            Some(root.path()),
            &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
        )
        .unwrap()
        .trim(),
        "a.txt",
        "the final commit must not capture unrelated developer work"
    );
    assert_eq!(
        tauqe_core::git::state::snapshot_worktree(root.path())
            .get("unrelated.txt")
            .map(String::as_str),
        Some("M"),
        "unrelated developer work must remain uncommitted"
    );
    assert_eq!(turn.rounds.len(), 1);
    assert_eq!(turn.rounds[0].message, "First attempt.");
    let answer = page
        .items
        .iter()
        .find(|item| item.text == "Verified.")
        .unwrap();
    assert_eq!(answer.commit_hash, commit);
    assert_eq!(
        answer
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        ["a.txt"],
        "healing history must contain only the edited file; recorded files: {:?}",
        answer.files
    );
    let journal = std::fs::read_to_string(root.path().join(".tauqe/history.jsonl")).unwrap();
    assert!(!journal.contains("private healing marker"));
    assert!(!journal.contains("*** Begin Patch"));
    assert_eq!(app.usage_snapshot().await.operations, 1);
}

#[tokio::test]
async fn ask_advertises_review_decisions_for_resolved_review_focus() {
    use tauqe_core::review::{storage::ReviewStorage, ReviewSession};

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    let session = ReviewSession {
        id: "audit".into(),
        title: "Audit".into(),
        items: vec![
            serde_json::from_value(serde_json::json!({"id": 1, "title": "Finding"})).unwrap(),
        ],
        ..Default::default()
    };
    ReviewStorage::save_session(root.path(), &session).unwrap();
    let app = application(root.path());
    let provider = Arc::new(MockLlmProvider::scripted([metered(
        r#"{"message":"Reply","action":{"kind":"answer"}}"#,
    )]));
    app.test_set_provider(provider.clone()).await;
    let (events, _received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Discussion {
            target: DiscussionTarget::Review { id: session.id },
            focused_items: vec!["1".into()],
            comment: "Discuss this finding".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    released(&app).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let schema = serde_json::to_string(&requests[0].response_format).unwrap();
    assert!(schema.contains("\"review_update\""));
    assert!(!schema.contains("\"plan_update\""));
}

#[tokio::test]
async fn ask_plan_update_correction_advertises_only_the_original_mutation() {
    use tauqe_core::plan::storage::PlanStorage;

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    PlanStorage::save_plan(
        root.path(),
        &Plan::from_parts(
            "focused".into(),
            "Original".into(),
            None,
            0,
            0,
            vec![],
            vec![],
        ),
    )
    .unwrap();
    let app = application(root.path());
    let provider = Arc::new(MockLlmProvider::scripted([
        metered(
            r#"{"message":"Draft.","action":{"kind":"plan","plan_action":{"kind":"modify","plan_update":{"action":"update","id":"focused","items":[{"id":"1","title":"New branch","children":[{"id":"1.1","title":"New leaf","files":["source.txt"]}]}]}}}}"#,
        ),
        metered(
            r#"{"message":"Saved.","action":{"kind":"plan","plan_update":{"action":"update","id":"focused","title":"Corrected"}}}"#,
        ),
    ]));
    app.test_set_provider(provider.clone()).await;
    let (events, _received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Discussion {
            target: DiscussionTarget::Plan {
                id: "focused".into(),
            },
            focused_items: vec![],
            comment: "Update the plan".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    released(&app).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let initial = serde_json::to_string(&requests[0].response_format).unwrap();
    for allowed in ["plan_create", "plan_update", "edit", "research"] {
        assert!(initial.contains(&format!("\"{allowed}\"")), "{allowed}");
    }
    assert!(!initial.contains("\"review_update\""));

    let correction = serde_json::to_string(&requests[1].response_format).unwrap();
    assert!(correction.contains("\"plan_update\""));
    for forbidden in ["plan_create", "edit", "research", "review"] {
        assert!(
            !correction.contains(&format!("\"{forbidden}\"")),
            "{forbidden}"
        );
    }
    assert!(requests[1]
        .messages
        .iter()
        .any(|message| message.content.contains("update may add leaf steps only")));
    let plan = PlanStorage::load_plan(root.path(), "focused")
        .unwrap()
        .unwrap();
    assert_eq!(plan.title, "Corrected");
    assert!(plan.implementation.items.is_empty());
}

#[tokio::test]
async fn ask_compacts_history_only_after_first_request_admission_fails() {
    struct StartObserver(Observer, Arc<MockLlmProvider>);

    impl ApplicationObserver for StartObserver {
        fn publish(&self, id: &str, event: ApplicationEvent) {
            if matches!(&event, ApplicationEvent::Started { .. }) {
                assert!(
                    self.1.requests().is_empty(),
                    "admission precedes the Scribe"
                );
            }
            self.0.publish(id, event);
        }
    }

    let root = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    config.request.max_input_tokens = 22_000;
    config.history.tail_turns = 0;
    config.history.budget_tokens = 100;
    let (history, _history_events) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().to_path_buf(), history);
    for _ in 0..4 {
        app.history_session()
            .record_history_exchange(
                "Previous task ".repeat(1_000),
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
    assert_eq!(
        requests.len(),
        2,
        "{}",
        app.history_page(Some(1), None)
            .await
            .unwrap()
            .items
            .last()
            .unwrap()
            .text
    );
    assert!(requests[0]
        .messages
        .iter()
        .any(|message| message.content.contains("Previous task ")));
    assert!(requests[1]
        .messages
        .iter()
        .any(|message| message.content.contains("Earlier tasks completed.")));
}
#[tokio::test]
async fn compacted_ask_accounts_for_scribe_and_answer_in_one_window() {
    let root = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    config.history.tail_turns = 0;
    config.history.budget_tokens = 100;
    config.request.max_input_tokens = 22_000;
    let (history, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
    for _ in 0..4 {
        app.history_session()
            .record_history_exchange("Old task ".repeat(1_000), vec![], "Done".into(), None)
            .await
            .unwrap();
    }
    let scribe = MockResponse::text(r#"{"summary":"Earlier tasks completed."}"#).with_usage(
        tauqe_core::providers::ModelUsageInfo {
            prompt_tokens: 11,
            completion_tokens: 13,
            total_tokens: 24,
            ..Default::default()
        },
    );
    let provider = Arc::new(MockLlmProvider::scripted([
        scribe,
        metered(r#"{"message":"First answer.","action":{"kind":"answer"}}"#),
        metered(r#"{"message":"Second answer.","action":{"kind":"answer"}}"#),
    ]));
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Current task".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    released(&app).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0]
        .messages
        .iter()
        .any(|m| m.content.contains("Old task")));
    assert!(requests[0]
        .messages
        .iter()
        .all(|m| !m.content.contains("Current task")));
    assert!(requests[1]
        .messages
        .iter()
        .any(|m| m.content.contains("Earlier tasks completed.")));
    let mut closed_before_result = false;
    let mut results = 0;
    while let Ok((_, event)) = received.try_recv() {
        match event {
            ApplicationEvent::Usage(snapshot) => {
                assert!(snapshot.cur.is_none());
                assert_eq!(snapshot.prev.total_tokens, 31);
                closed_before_result = true;
            }
            ApplicationEvent::Result { usage, completion } => {
                assert!(closed_before_result);
                assert!(completion.error.is_none());
                assert!(usage.cur.is_none());
                assert_eq!(usage.prev.total_tokens, 31);
                results += 1;
            }
            _ => {}
        }
    }
    assert_eq!(results, 1);
    let page = app.history_page(Some(100), None).await.unwrap();
    assert_eq!(
        page.items
            .iter()
            .filter(|item| item.text == "Current task")
            .count(),
        1
    );
    assert_eq!(
        page.items
            .iter()
            .filter(|item| item.text == "First answer.")
            .count(),
        1
    );

    let (next_events, mut next_received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Next task".into(),
        },
        Arc::new(Observer(next_events)),
    )
    .await
    .unwrap();
    released(&app).await;
    assert_eq!(provider.requests().len(), 3);
    assert!(
        std::iter::from_fn(|| next_received.try_recv().ok()).any(|(_, event)| {
            matches!(event, ApplicationEvent::Result { usage, .. }
            if usage.cur.is_none() && usage.prev.total_tokens == 7 && usage.operations == 2)
        })
    );
    let usage = app.usage_snapshot().await;
    assert_eq!(usage.session.total_tokens, 38);
    assert_eq!(usage.prev.total_tokens, 7);
    assert_eq!(usage.operations, 2);
}

#[tokio::test]
async fn failed_ask_after_scribe_usage_keeps_cost_without_a_successful_answer() {
    let root = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    config.history.tail_turns = 0;
    config.history.budget_tokens = 100;
    config.request.max_input_tokens = 22_000;
    let (history, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
    for _ in 0..4 {
        app.history_session()
            .record_history_exchange("Old task ".repeat(1_000), vec![], "Done".into(), None)
            .await
            .unwrap();
    }
    let provider = Arc::new(MockLlmProvider::scripted([
        metered(r#"{"summary":"Earlier tasks completed."}"#),
        MockResponse::error("offline after compaction"),
    ]));
    app.test_set_provider(provider.clone()).await;
    let (events, mut received) = mpsc::unbounded_channel();
    app.ask(
        ModelAskInput::Prompt {
            text: "Failed current task".into(),
        },
        Arc::new(Observer(events)),
    )
    .await
    .unwrap();
    released(&app).await;
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(app.usage_snapshot().await.prev.total_tokens, 7);
    assert!(app.usage_snapshot().await.cur.is_none());
    assert!(!std::iter::from_fn(|| received.try_recv().ok()).any(|(_, event)| {
        matches!(event, ApplicationEvent::Finished(_))
            || matches!(event, ApplicationEvent::Result { completion, .. } if completion.error.is_none())
    }));
    let page = app.history_page(Some(100), None).await.unwrap();
    assert_eq!(
        page.items
            .iter()
            .filter(|item| item.text == "Failed current task")
            .count(),
        1
    );
    assert!(page
        .items
        .iter()
        .any(|item| item.text.contains("offline after compaction")));
    assert!(app.try_begin_operation().is_ok());
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
        r#"{"message":"","action":{"kind":"edit","patch":"","commit_message":"","error_message":"","reason":"Already done"}}"#,
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
        observer.clone(),
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
