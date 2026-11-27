use super::*;
use crate::api::EditCycleEvent;
use tauqe_core::git::cmd::run_git;
use tauqe_core::review::{
    storage::ReviewStorage, IssueStatus, IssueType, ReviewExecuteTarget, ReviewFindingTarget,
    ReviewIssue, ReviewItem, ReviewSession,
};
use tauqe_core::workflow::batch::BatchStop;

#[tokio::test]
async fn preset_audit_keeps_completed_groups_and_analyzes_after_a_domain_failure() {
    use tauqe_core::review::PostAnalysisMode;

    for second_group_has_files in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        tauqe_core::git::init_repository(Some(path), true).unwrap();
        std::fs::write(path.join("source.rs"), "fn source() {}\n").unwrap();
        tauqe_core::git::cmd::stage_files(path, &["source.rs".into()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(path, "Add audit source", false)
            .unwrap();
        let mut config: AppConfig = toml::from_str(&format!(
            "[[review.presets]]\nname = 'audit'\npost_analysis = 'automatic'\n\
             [[review.presets.groups]]\nname = 'first'\npaths = ['source.rs']\n\
             [[review.presets.groups]]\nname = 'second'\npaths = ['{}']\n",
            if second_group_has_files {
                "source.rs"
            } else {
                "missing.rs"
            }
        ))
        .unwrap();
        config.context.include_repo_map = false;
        let (history, _) = mpsc::unbounded_channel();
        let app = Application::for_repository(config, path.into(), history);
        let finding = metered(
            r#"{"summary":"One finding.","findings":[{"title":"Boundary","type":"bug","description":"Keep the boundary explicit.","recommendations":[],"files":["source.rs"],"readonly_files":[]}]}"#,
        );
        let analysis = || {
            metered(
                r#"{"summary":"No systemic defects.","triage":[],"independent_issues":[],"architectural_groups":[]}"#,
            )
        };
        let responses = if second_group_has_files {
            vec![
                finding,
                metered(r#"{"summary":"No additional findings.","findings":[]}"#),
                analysis(),
            ]
        } else {
            // A domain failure is reported for the missing group but does not
            // stop the queue; the next provider call is post-analysis.
            vec![finding, analysis()]
        };
        let provider = Arc::new(MockLlmProvider::scripted(responses));
        app.test_set_provider(provider.clone()).await;
        let (events, mut received) = mpsc::unbounded_channel();
        let id = app
            .start_review(
                crate::api::ReviewStartInput {
                    preset: Some("audit".into()),
                    model: None,
                    path_patterns: vec![],
                    prompt: None,
                },
                Arc::new(Observer(events)),
                DEADLINE,
            )
            .await
            .unwrap();
        tokio::time::timeout(DEADLINE, async {
            let mut completed_groups = 0;
            let mut failed_groups = 0;
            let mut last_saved = None;
            loop {
                let (event_id, event) = received.recv().await.expect("review must finish");
                match event {
                    ApplicationEvent::ReviewRun(crate::api::ReviewEvent::GroupFinished {
                        error,
                        ..
                    }) => {
                        if error.is_some() {
                            failed_groups += 1;
                        } else {
                            completed_groups += 1;
                        }
                    }
                    ApplicationEvent::ReviewDocument(session) => last_saved = Some(session),
                    ApplicationEvent::ReviewRun(crate::api::ReviewEvent::Finished {
                        session,
                        ..
                    }) => {
                        assert_eq!(event_id, id);
                        assert_eq!(completed_groups, if second_group_has_files { 2 } else { 1 });
                        assert_eq!(failed_groups, usize::from(!second_group_has_files));
                        assert_eq!(
                            last_saved.as_ref().map(|saved| saved.id.as_str()),
                            Some(session.id.as_str())
                        );
                        assert_eq!(session.description.as_deref(), Some("No systemic defects."));
                        assert_eq!(session.items.len(), 1);
                        assert_eq!(session.groups.len(), 1);
                        assert_eq!(session.groups[0].id, "local");
                        assert_eq!(
                            session.launch_config.as_ref().unwrap().mode,
                            PostAnalysisMode::Automatic
                        );
                        assert_eq!(
                            app.get_review(Some(session.id.clone())).await.unwrap(),
                            Some(session)
                        );
                        break;
                    }
                    ApplicationEvent::ReviewRun(crate::api::ReviewEvent::Error(error)) => {
                        panic!("{error}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        released(&app).await;
        assert_eq!(
            provider.requests().len(),
            if second_group_has_files { 3 } else { 2 }
        );
        let usage = app.usage_snapshot().await;
        assert_eq!(
            usage.session.total_tokens,
            if second_group_has_files { 21 } else { 14 }
        );
    }
}

#[tokio::test]
async fn public_plan_and_review_routes_publish_the_durable_batch_result() {
    use tauqe_core::models::{ModelEntry, ModelRef, ModelRole, ModelSelection, ReasoningEffort};

    for (review, explicit_coder) in [(false, false), (true, false), (true, true)] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        tauqe_core::git::init_repository(Some(path), true).unwrap();
        tauqe_tools::git::checkpoint::ensure_tauqe_ignored(path).unwrap();
        std::fs::write(path.join("source.txt"), "before\n").unwrap();
        tauqe_core::git::cmd::stage_files(path, &["source.txt".into()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(path, "initial", false).unwrap();
        let mut config = AppConfig::default();
        config.repl.workflow = WorkflowKind::Git;
        config.context.include_repo_map = false;
        config.models.repl = vec![ModelEntry::new(
            ModelRef::openrouter("session/repl"),
            Some(ReasoningEffort::Low),
        )];
        config.models.coder = Some(vec![
            ModelEntry::new(
                ModelRef::openrouter("implementation/default"),
                Some(ReasoningEffort::High),
            ),
            ModelEntry::new(
                ModelRef::openrouter("implementation/selected"),
                Some(ReasoningEffort::Max),
            ),
        ]);
        let (expected_model, expected_effort) = if explicit_coder {
            (
                ModelRef::openrouter("implementation/selected"),
                ReasoningEffort::Max,
            )
        } else {
            (
                ModelRef::openrouter("implementation/default"),
                ReasoningEffort::High,
            )
        };
        let (history, _) = mpsc::unbounded_channel();
        let app = Application::for_repository(config, path.into(), history);
        let provider = Arc::new(MockLlmProvider::scripted([metered(serde_json::json!({
            "message": "Updated source.",
            "action": {"kind": "edit", "commit_message": "fix: update source",
                "patch": "*** Begin Patch\n*** Update File: source.txt\n@@\n-before\n+after\n*** End Patch"}
        }).to_string())]));
        app.test_set_provider(provider.clone()).await;
        let (events, mut received) = mpsc::unbounded_channel();
        let observer = Arc::new(Observer(events));
        let operation = if review {
            ReviewStorage::save_session(
                path,
                &ReviewSession {
                    id: "audit".into(),
                    title: "Audit".into(),
                    items: vec![ReviewItem::from(ReviewIssue {
                        id: 1,
                        title: "Update source".into(),
                        issue_type: IssueType::Quality,
                        status: IssueStatus::Todo,
                        files: vec!["source.txt".into()],
                        uuid: None,
                        group: None,
                        model: None,
                        description: String::new(),
                        recommendations: vec![],
                        readonly_files: vec![],
                        commit: None,
                        note: None,
                    })],
                    ..Default::default()
                },
            )
            .unwrap();
            app.execute_review(
                ReviewExecuteTarget::Finding(ReviewFindingTarget {
                    review_id: "audit".into(),
                    item_id: 1,
                }),
                explicit_coder.then(|| ModelSelection::Tier {
                    model: expected_model.clone(),
                    effort: Some(expected_effort),
                }),
                observer,
            )
            .await
            .unwrap()
            .operation_id
        } else {
            let plan = Plan::from_parts(
                "plan".into(),
                "Plan".into(),
                None,
                0,
                0,
                vec![],
                vec![PlanItem {
                    id: "1".into(),
                    title: "Update source".into(),
                    status: PlanItemStatus::Todo,
                    files: vec!["source.txt".into()],
                    ..Default::default()
                }],
            );
            app.save_plan(plan, observer.as_ref()).await.unwrap();
            app.execute_plan("plan".into(), PlanBatchOptions::default(), observer)
                .await
                .unwrap()
                .operation_id
        };
        released(&app).await;

        let mut commit = None;
        let mut document = None;
        let mut terminal = 0;
        let mut starts = 0;
        while let Ok((id, event)) = received.try_recv() {
            match event {
                ApplicationEvent::Started {
                    model,
                    role,
                    effort,
                    ..
                } => {
                    assert_eq!(model, expected_model);
                    assert_eq!(effort, Some(expected_effort));
                    assert_eq!(role, (!explicit_coder).then_some(ModelRole::Coder));
                    starts += 1;
                }
                ApplicationEvent::Edit(EditCycleEvent::Finished(outcome)) if !review => {
                    assert!(outcome.applied, "plan edit did not settle: {outcome:?}");
                    assert_eq!(outcome.changed_files, ["source.txt"]);
                    let hash = outcome.commit_hash.expect("the plan edit must commit");
                    assert!(commit.replace(hash).is_none());
                }
                ApplicationEvent::Commit {
                    hash,
                    changed_files,
                    ..
                } if review => {
                    assert_eq!(changed_files, ["source.txt"]);
                    assert!(commit.replace(hash).is_none());
                }
                ApplicationEvent::PlanDocument { plan, .. } => {
                    let leaf = plan.implementation.find_item("1").unwrap();
                    if leaf.status == PlanItemStatus::Done {
                        document.clone_from(&leaf.commit);
                    }
                }
                ApplicationEvent::ReviewDocument(session) => {
                    let finding = session.find_item(1).unwrap();
                    if finding.status == IssueStatus::Fixed {
                        document.clone_from(&finding.commit);
                    }
                }
                ApplicationEvent::PlanBatchFinished(report) => {
                    assert!(!review);
                    assert_eq!(id, operation);
                    assert_eq!(report.outcome, BatchOutcome::Completed { steps: 1 });
                    assert!(commit.is_some());
                    assert_eq!(
                        document, commit,
                        "the plan snapshot must precede the terminal event"
                    );
                    terminal += 1;
                }
                ApplicationEvent::ReviewBatchFinished(report) => {
                    assert!(review);
                    assert_eq!(id, operation);
                    assert!(matches!(report.stop, BatchStop::Completed));
                    assert_eq!((report.executed_count, report.fixed_count), (1, 1));
                    assert!(commit.is_some());
                    assert_eq!(
                        document, commit,
                        "the review snapshot must precede the terminal event"
                    );
                    terminal += 1;
                }
                _ => {}
            }
        }
        assert_eq!(terminal, 1);
        assert_eq!(starts, 1);
        assert_eq!(provider.requests().len(), 1);
        let request = provider.last_request().unwrap();
        assert_eq!(request.model, expected_model.name);
        assert_eq!(request.reasoning_effort, Some(expected_effort));
        let reopened = application(path);
        let saved_commit = if review {
            let session = reopened
                .get_review(Some("audit".into()))
                .await
                .unwrap()
                .unwrap();
            let finding = session.find_item(1).unwrap();
            assert_eq!(finding.status, IssueStatus::Fixed);
            finding.commit.clone()
        } else {
            let plan = reopened
                .get_plan(Some("plan".into()))
                .await
                .unwrap()
                .unwrap();
            let leaf = plan.implementation.find_item("1").unwrap();
            assert_eq!(leaf.status, PlanItemStatus::Done);
            leaf.commit.clone()
        };
        assert_eq!(saved_commit, commit);
        let head = run_git(Some(path), &["rev-parse", "--short", "HEAD"]).unwrap();
        assert_eq!(Some(head.trim()), commit.as_deref());
        assert_eq!(
            run_git(Some(path), &["show", "HEAD:source.txt"])
                .unwrap()
                .trim_end(),
            "after"
        );
        assert!(run_git(Some(path), &["status", "--porcelain"])
            .unwrap()
            .is_empty());
        assert!(!path.join(".tauqe/active-transaction.json").exists());
    }
}
