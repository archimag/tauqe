use super::*;
use crate::api::EditCycleEvent;
use tauqe_core::git::cmd::run_git;
use tauqe_core::review::{
    storage::ReviewStorage, IssueStatus, IssueType, ReviewExecuteTarget, ReviewFindingTarget,
    ReviewIssue, ReviewItem, ReviewSession,
};
use tauqe_core::workflow::batch::BatchStop;

#[tokio::test]
async fn public_plan_and_review_routes_publish_the_durable_batch_result() {
    for review in [false, true] {
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
                None,
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
        while let Ok((id, event)) = received.try_recv() {
            match event {
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
                    assert_eq!(document, commit);
                    terminal += 1;
                }
                ApplicationEvent::ReviewBatchFinished(report) => {
                    assert!(review);
                    assert_eq!(id, operation);
                    assert!(matches!(report.stop, BatchStop::Completed));
                    assert_eq!((report.executed_count, report.fixed_count), (1, 1));
                    assert!(commit.is_some());
                    assert_eq!(document, commit);
                    terminal += 1;
                }
                _ => {}
            }
        }
        assert_eq!(terminal, 1);
        assert_eq!(provider.requests().len(), 1);
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
