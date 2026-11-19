//! RPC adaptation for review post-analysis.
use crate::contracts::Project;
use crate::reap::task_exit_deadline;
use crate::state::AppState;
use std::sync::Arc;
#[cfg(test)]
use tauqe_core::{models::ModelSelection, review::ReviewSession};
use tauqe_flow::api::{
    AnalyzeReviewError, ReviewAnalysisCompletionError, ReviewAnalysisExecutionError,
    ReviewAnalysisLoadError, ReviewAnalysisPersistenceError, ReviewAnalysisPreparationError,
};
#[cfg(test)]
use tauqe_protocol::events;
use tauqe_protocol::{
    errors, Request, Response, ResponseError, ReviewAnalyzeParams, ReviewAnalyzeResult,
};

/// Handles `review/analyze`: classifies the findings of one stored session into
/// independent issues and architectural root-cause groups.
///
/// The session identifier is mandatory and never defaulted, so a run can only
/// classify the session the caller named. The findings themselves are immutable
/// input: the request replaces the classification layer and answers with the
/// session carrying it, so the client renders the new layer without a follow-up
/// fetch.
///
/// The operation slot is claimed before the session is read, so the whole
/// read-modify-write cycle, including the identity healing that rewrites the
/// stored session, runs inside the exclusion window it relies on: a bounded
/// update or a deletion that samples the slot while the round holds it is
/// refused instead of being overwritten by the heal save.
///
/// A session with nothing to classify is refused by the shared procedure before
/// any provider call, exactly as a session that cannot be resolved is refused
/// here. An optional Architect selection travels to that procedure, so the
/// round runs the configuration the caller named; an absent one keeps the
/// base rung of the architect ladder.
pub async fn handle_review_analyze(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewAnalyzeParams = match req.params {
        Some(params) => match serde_json::from_value(params) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid review analyze parameters: {}", err),
                )
            }
        },
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid review analyze params",
            )
        }
    };

    let observer = Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    });
    match state
        .application
        .analyze_review(
            params.review_id,
            params.model.map(Project::project),
            observer,
            task_exit_deadline(),
        )
        .await
    {
        Ok(session) => Response::ok_typed(
            req.id,
            &ReviewAnalyzeResult {
                session: session.project(),
            },
        ),
        Err(error) => {
            let error = analyze_error(error);
            Response::err(req.id, error.code, error.message)
        }
    }
}

#[cfg(test)]
async fn run_review_post_analysis(
    state: &Arc<AppState>,
    repo_root: &std::path::Path,
    session: &tauqe_core::review::ReviewSession,
    selection: Option<&tauqe_core::models::ModelSelection>,
    operation: &crate::state::OperationGuard,
    is_batch: bool,
) -> Result<tauqe_core::review::ReviewSession, ResponseError> {
    let observer = Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    });
    state
        .application
        .test_run_review_analysis(
            operation,
            repo_root,
            session,
            selection,
            observer,
            is_batch,
            task_exit_deadline(),
        )
        .await
        .map_err(analyze_error)
}

fn analyze_error(error: AnalyzeReviewError) -> ResponseError {
    let code = match &error {
        AnalyzeReviewError::Busy => errors::OPERATION_IN_PROGRESS,
        AnalyzeReviewError::Provider(_) => errors::NO_API_KEY,
        AnalyzeReviewError::Load(error) => match error {
            ReviewAnalysisLoadError::MissingReviewId => errors::INVALID_PARAMS,
            ReviewAnalysisLoadError::OperationNotOwned => errors::OPERATION_IN_PROGRESS,
            ReviewAnalysisLoadError::NotFound(_) => errors::NOT_FOUND,
            ReviewAnalysisLoadError::Storage(_) => errors::REVIEW_LOAD_FAILED,
        },
        AnalyzeReviewError::Execution(error) => execution_error_code(error),
    };
    ResponseError::new(code, error.to_string())
}

fn preparation_error_code(error: &ReviewAnalysisPreparationError) -> &'static str {
    match error {
        ReviewAnalysisPreparationError::OperationNotOwned => errors::OPERATION_IN_PROGRESS,
        ReviewAnalysisPreparationError::EmptyFindings(_) => errors::EMPTY_ITEMS,
        ReviewAnalysisPreparationError::Storage(_) => errors::REVIEW_UPDATE_FAILED,
        ReviewAnalysisPreparationError::InvalidModel(_) => errors::INVALID_MODEL,
    }
}

fn execution_error_code(error: &ReviewAnalysisExecutionError) -> &'static str {
    match error {
        ReviewAnalysisExecutionError::Preparation(error) => preparation_error_code(error),
        ReviewAnalysisExecutionError::Completion(error) => match error {
            ReviewAnalysisCompletionError::Cancelled(_) => errors::CANCELLED,
            ReviewAnalysisCompletionError::Provider(_) => errors::MODEL_ERROR,
            ReviewAnalysisCompletionError::EmptyResponse(_)
            | ReviewAnalysisCompletionError::Parse(_) => errors::PARSE_ERROR,
            ReviewAnalysisCompletionError::Schema(_) => errors::SCHEMA_ERROR,
        },
        ReviewAnalysisExecutionError::Persistence(error) => match error {
            ReviewAnalysisPersistenceError::OperationNotOwned => errors::OPERATION_IN_PROGRESS,
            ReviewAnalysisPersistenceError::MismatchedReview => errors::SCHEMA_ERROR,
            ReviewAnalysisPersistenceError::Cancelled(_) => errors::CANCELLED,
            ReviewAnalysisPersistenceError::Storage(_) => errors::REVIEW_UPDATE_FAILED,
            ReviewAnalysisPersistenceError::NotFound(_) => errors::NOT_FOUND,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::scratch_root;
    use crate::handlers::review::tests::{
        await_published, stall_outbound_transport, RecordingSink,
    };
    use tauqe_core::config::{AppConfig, ModelsConfig};
    use tauqe_core::models::{ModelEntry, ModelRef, ReasoningEffort};
    use tauqe_core::providers::mock::MockResponse;
    use tauqe_core::providers::{LlmProvider, MockLlmProvider};
    use tauqe_core::review::ReviewStorage;
    use tauqe_core::review::{IssueStatus, IssueType, ReviewItem, ReviewSeverity};
    use tauqe_core::review::{ReviewGroupKind, LOCAL_REVIEW_GROUP_ID, LOCAL_REVIEW_GROUP_TITLE};

    /// Identity of the finding the core engine group produced.
    const CORE_UUID: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000001";
    /// Identity of the finding the terminal frontend group produced.
    const TUI_UUID: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000002";

    /// Scratch repository and its daemon state.
    struct AnalysisFixture {
        root: std::path::PathBuf,
        state: Arc<AppState>,
        /// Outbound queue of the fixture daemon, retained so a pacing regression
        /// can drive the writer that drains it.
        events: tokio::sync::mpsc::UnboundedReceiver<String>,
    }

    impl Drop for AnalysisFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl AnalysisFixture {
        /// Takes the outbound queue receiver, leaving the receiver of a queue
        /// that never receives, so a pacing regression can drive its own writer.
        fn take_events(&mut self) -> tokio::sync::mpsc::UnboundedReceiver<String> {
            let (_, placeholder) = tokio::sync::mpsc::unbounded_channel();
            std::mem::replace(&mut self.events, placeholder)
        }
    }

    fn fixture(label: &str) -> AnalysisFixture {
        let root = scratch_root(&format!("review-analysis-{label}"));
        let (out, events) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);
        AnalysisFixture {
            root,
            state,
            events,
        }
    }

    /// Runs one post-analysis round of the fixture session through the shared
    /// procedure, so a test names only the Architect selection it exercises.
    async fn analyze(
        fixture: &AnalysisFixture,
        session: &ReviewSession,
        selection: Option<&ModelSelection>,
    ) -> Result<ReviewSession, ResponseError> {
        let operation = fixture.state.try_begin_operation().unwrap();
        run_review_post_analysis(
            &fixture.state,
            &fixture.root,
            session,
            selection,
            &operation,
            false,
        )
        .await
    }

    /// Installs an architect ladder of `entries` over the fixture project.
    async fn configure_architect(fixture: &AnalysisFixture, entries: Vec<ModelEntry>) {
        let mut config = fixture.state.application.configuration().await;
        config.config.models = ModelsConfig {
            repl: vec![ModelEntry::from(ModelRef::openrouter("test/primary"))],
            architect: Some(entries),
            ..Default::default()
        };
        fixture
            .state
            .application
            .test_replace_configuration(config.config, tauqe_flow::api::ReloadSelection::Preserve)
            .await
            .unwrap();
    }

    fn finding(id: u32, uuid: &str, group: &str, status: IssueStatus) -> ReviewItem {
        ReviewItem {
            id,
            title: format!("Finding {id}"),
            uuid: Some(uuid.to_string()),
            commit: None,
            group: Some(group.to_string()),
            model: None,
            issue_type: IssueType::Architecture,
            severity: ReviewSeverity::from(IssueType::Architecture),
            status,
            recommendations: vec!["Introduce one shared transaction boundary".to_string()],
            files: vec!["crates/core/src/git.rs".to_string()],
            readonly_files: vec!["Conventions.md".to_string()],
            resolution_notes: None,
            body: "The change set reaches the worktree without one shared boundary.".to_string(),
        }
    }

    fn session() -> ReviewSession {
        ReviewSession {
            id: "rev-analyze".to_string(),
            title: "Multi-Level Audit".to_string(),
            created_at: 100,
            model: ModelRef::openrouter("test/architect"),
            target_files: vec!["crates/core/src/git.rs".to_string()],
            items: vec![
                finding(1, CORE_UUID, "core-engine", IssueStatus::Todo),
                finding(2, TUI_UUID, "terminal-ui", IssueStatus::Discussion),
            ],
            ..ReviewSession::default()
        }
    }

    /// Renders one well-formed classification payload.
    fn classification(summary: &str, independent: &[&str], groups: &[(&str, &[&str])]) -> String {
        let groups: Vec<serde_json::Value> = groups
            .iter()
            .map(|(title, ids)| {
                serde_json::json!({
                    "title": title,
                    "description": format!("The systemic root cause behind {title}."),
                    "issue_ids": ids,
                })
            })
            .collect();
        serde_json::json!({
            "summary": summary,
            "independent_issues": independent,
            "architectural_groups": groups,
        })
        .to_string()
    }

    /// Re-running the analysis regroups the session in place while the findings
    /// it references keep their exact identities, content, and statuses.
    #[tokio::test]
    async fn post_analysis_replaces_the_layer_without_touching_findings() {
        let fixture = fixture("replace");
        let session = session();
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(classification(
                "One shared transaction boundary explains both findings.",
                &[CORE_UUID],
                &[("Shared commit boundary", &[TUI_UUID])],
            )),
            MockResponse::text(classification(
                "The frontend repeats the same boundary defect.",
                &[TUI_UUID],
                &[("Shared commit boundary", &[CORE_UUID])],
            )),
        ]));
        fixture.state.application.test_set_provider(provider).await;

        let first = analyze(&fixture, &session, None)
            .await
            .expect("the classification is persisted");
        // The classification is the session itself: the verdict becomes the
        // session description, the payload clusters become the session groups,
        // and every finding moves to the group that now owns it. An architecture
        // finding the classification left on its own receives a single-element
        // architectural group instead of the group of independent findings.
        assert_eq!(
            first.description.as_deref(),
            Some("One shared transaction boundary explains both findings.")
        );
        assert_eq!(first.groups.len(), 3);
        assert_eq!(first.groups[0].id, "arch-1");
        assert_eq!(first.groups[0].kind, ReviewGroupKind::Architectural);
        assert_eq!(first.groups[0].title, "Shared commit boundary");
        assert_eq!(
            first.groups[0].description.as_deref(),
            Some("The systemic root cause behind Shared commit boundary.")
        );
        // The architecture finding the classification called independent is
        // planned before it is executed, so it owns a single-element
        // architectural group titled after the finding itself.
        assert_eq!(first.groups[1].id, "arch-2");
        assert_eq!(first.groups[1].kind, ReviewGroupKind::Architectural);
        assert_eq!(first.groups[1].title, "Finding 1");
        assert_eq!(first.groups[2].id, LOCAL_REVIEW_GROUP_ID);
        assert_eq!(first.groups[2].title, LOCAL_REVIEW_GROUP_TITLE);
        assert_eq!(first.groups[2].kind, ReviewGroupKind::Simple);
        assert_eq!(first.items[0].group.as_deref(), Some("arch-2"));
        assert_eq!(first.items[1].group.as_deref(), Some("arch-1"));
        // Findings are immutable input: regrouping reassigns the group of each
        // finding and never rewrites what it reports.
        assert_eq!(first.items[0].title, session.items[0].title);
        assert_eq!(first.items[1].title, session.items[1].title);
        assert_eq!(first.items[0].status, IssueStatus::Todo);
        assert_eq!(first.items[1].status, IssueStatus::Discussion);

        let second = analyze(&fixture, &session, None)
            .await
            .expect("the second classification is persisted");
        // The second classification substitutes the grouping wholesale: no part
        // of the previous one survives, while every finding keeps its identity,
        // its content, and its lifecycle status.
        assert_eq!(
            second.description.as_deref(),
            Some("The frontend repeats the same boundary defect.")
        );
        assert_eq!(second.groups.len(), 3);
        assert_eq!(second.groups[0].id, "arch-1");
        assert_eq!(second.groups[1].id, "arch-2");
        assert_eq!(second.groups[1].title, "Finding 2");
        assert_eq!(second.groups[2].id, LOCAL_REVIEW_GROUP_ID);
        assert_eq!(second.items[0].group.as_deref(), Some("arch-1"));
        assert_eq!(second.items[1].group.as_deref(), Some("arch-2"));

        let stored = ReviewStorage::load_review(&fixture.root, "rev-analyze")
            .unwrap()
            .expect("the session is stored");
        assert_eq!(
            stored.description.as_deref(),
            Some("The frontend repeats the same boundary defect.")
        );
        assert_eq!(stored.groups.len(), 3);
        assert_eq!(stored.groups[0].title, "Shared commit boundary");
        assert_eq!(stored.groups[1].id, "arch-2");
        assert_eq!(stored.groups[1].title, "Finding 2");
        assert_eq!(stored.groups[1].kind, ReviewGroupKind::Architectural);
        assert_eq!(stored.groups[2].id, LOCAL_REVIEW_GROUP_ID);
        assert_eq!(stored.groups[2].title, LOCAL_REVIEW_GROUP_TITLE);
        assert_eq!(stored.groups[2].kind, ReviewGroupKind::Simple);
        assert_eq!(stored.items[0].group.as_deref(), Some("arch-1"));
        assert_eq!(stored.items[1].group.as_deref(), Some("arch-2"));
        assert_eq!(stored.items[0].status, IssueStatus::Todo);
        assert_eq!(stored.items[1].status, IssueStatus::Discussion);
        assert_eq!(stored.items[0].uuid.as_deref(), Some(CORE_UUID));
        assert_eq!(stored.items[1].uuid.as_deref(), Some(TUI_UUID));
    }

    /// An architecture finding never lands in the group of independent findings,
    /// while an independent finding of another type stays exactly where the
    /// classification placed it.
    #[tokio::test]
    async fn post_analysis_keeps_an_independent_architecture_finding_out_of_the_local_group() {
        let fixture = fixture("arch-redirect");
        let mut session = session();
        // The second finding is a quality finding: nothing requires it to be
        // planned before it is executed.
        session.items[1].issue_type = IssueType::Quality;
        session.items[1].severity = ReviewSeverity::from(IssueType::Quality);
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            classification(
                "Both findings stand on their own.",
                &[CORE_UUID, TUI_UUID],
                &[],
            ),
        )]));
        fixture.state.application.test_set_provider(provider).await;

        let stored = analyze(&fixture, &session, None)
            .await
            .expect("the classification is persisted");

        // The architecture finding is planned before it is executed, so it owns a
        // single-element architectural group titled after the finding itself,
        // while the group of independent findings stays non-architectural.
        assert_eq!(stored.groups.len(), 2);
        assert_eq!(stored.groups[0].id, "arch-1");
        assert_eq!(stored.groups[0].title, "Finding 1");
        assert_eq!(stored.groups[0].kind, ReviewGroupKind::Architectural);
        assert_eq!(stored.groups[1].id, LOCAL_REVIEW_GROUP_ID);
        assert_eq!(stored.groups[1].kind, ReviewGroupKind::Simple);
        assert_eq!(stored.items[0].group.as_deref(), Some("arch-1"));
        assert_eq!(
            stored.items[1].group.as_deref(),
            Some(LOCAL_REVIEW_GROUP_ID)
        );
        // Regrouping reassigns the owning group and never rewrites the finding.
        assert_eq!(stored.items[0].uuid.as_deref(), Some(CORE_UUID));
        assert_eq!(stored.items[1].uuid.as_deref(), Some(TUI_UUID));
        assert_eq!(stored.items[0].title, session.items[0].title);
        assert_eq!(stored.items[1].title, session.items[1].title);
        assert_eq!(stored.items[0].status, IssueStatus::Todo);
        assert_eq!(stored.items[1].status, IssueStatus::Discussion);
    }
    /// A classification naming an identity the session does not hold is rejected
    /// by name and never reaches disk.
    #[tokio::test]
    async fn post_analysis_refuses_a_foreign_identifier() {
        let fixture = fixture("foreign");
        let session = session();
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let foreign = "018f9c1e-6b3a-7c1e-8b3a-0000000000ff";
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            classification("Root cause", &[foreign], &[]),
        )]));
        fixture.state.application.test_set_provider(provider).await;

        let err = analyze(&fixture, &session, None)
            .await
            .expect_err("a foreign identifier must be refused");
        assert_eq!(err.code, errors::SCHEMA_ERROR);
        assert!(
            err.message.contains(foreign),
            "the diagnostic must name the foreign uuid: {}",
            err.message
        );

        let stored = ReviewStorage::load_review(&fixture.root, "rev-analyze")
            .unwrap()
            .expect("the session is stored");
        assert!(
            stored.post_analysis.is_none(),
            "a refused classification never reaches disk"
        );
        assert_eq!(stored.items, session.items);
    }

    /// A session with nothing to classify is refused before any provider call.
    #[tokio::test]
    async fn post_analysis_refuses_a_session_without_findings() {
        let fixture = fixture("empty");
        let session = ReviewSession {
            id: "rev-empty".to_string(),
            title: "Empty Audit".to_string(),
            created_at: 1,
            model: ModelRef::openrouter("test/architect"),
            ..ReviewSession::default()
        };
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let err = analyze(&fixture, &session, None)
            .await
            .expect_err("an empty session has nothing to classify");
        assert_eq!(err.code, errors::EMPTY_ITEMS);

        let stored = ReviewStorage::load_review(&fixture.root, "rev-empty")
            .unwrap()
            .expect("the session is stored");
        assert!(stored.post_analysis.is_none());
    }

    /// A re-spelled identity still addresses the finding it names, so the
    /// regrouping moves exactly that finding and leaves its identity intact.
    #[tokio::test]
    async fn post_analysis_stores_the_authoritative_identifier_spelling() {
        let fixture = fixture("respell");
        let session = session();
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let re_spelled = CORE_UUID.to_ascii_uppercase();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            classification(
                "One shared boundary.",
                &[],
                &[("Shared boundary", &[re_spelled.as_str()])],
            ),
        )]));
        fixture.state.application.test_set_provider(provider).await;

        let stored = analyze(&fixture, &session, None)
            .await
            .expect("a re-spelled identity resolves");
        assert_eq!(stored.description.as_deref(), Some("One shared boundary."));
        assert_eq!(stored.groups.len(), 3);
        assert_eq!(stored.groups[0].id, "arch-1");
        assert_eq!(stored.groups[0].title, "Shared boundary");
        assert_eq!(stored.items[0].group.as_deref(), Some("arch-1"));
        assert_eq!(stored.items[0].uuid.as_deref(), Some(CORE_UUID));
        // The finding the classification did not name keeps no stale
        // provenance, and its architecture type still keeps it out of the group
        // of independent findings: it receives a single-element group of its own.
        assert_eq!(stored.groups[1].id, "arch-2");
        assert_eq!(stored.groups[1].title, "Finding 2");
        assert_eq!(stored.groups[1].kind, ReviewGroupKind::Architectural);
        assert_eq!(stored.groups[2].id, LOCAL_REVIEW_GROUP_ID);
        assert_eq!(stored.groups[2].kind, ReviewGroupKind::Simple);
        assert_eq!(stored.items[1].group.as_deref(), Some("arch-2"));
        assert_eq!(stored.items[1].uuid.as_deref(), Some(TUI_UUID));
    }

    /// The identifier is mandatory: an implicit target would rewrite whichever
    /// session happened to be focused.
    #[tokio::test]
    async fn analyze_requires_an_explicit_session_identifier() {
        let fixture = fixture("strict-id");
        let req = Request {
            id: tauqe_protocol::RequestId::Number(3),
            method: tauqe_protocol::methods::REVIEW_ANALYZE.to_string(),
            params: Some(serde_json::json!({})),
        };

        let response = handle_review_analyze(req, &fixture.state).await;
        assert_eq!(
            response.error.expect("the request is refused").code,
            errors::INVALID_PARAMS
        );
    }
    /// The routing is the first lifecycle state a finding holds: an unambiguous
    /// defect becomes executable, while a genuine fork stays with the engineer
    /// until the decision is taken.
    #[tokio::test]
    async fn post_analysis_routes_every_finding_by_the_triage() {
        let fixture = fixture("triage");
        let mut session = session();
        for item in &mut session.items {
            item.status = IssueStatus::Todo;
        }
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let payload = serde_json::json!({
            "summary": "One defect is unambiguous, one decision is open.",
            "triage": [
                {"uuid": CORE_UUID, "status": "todo", "discussion_topic": null},
                {"uuid": TUI_UUID, "status": "discussion", "discussion_topic": "Which API replaces the removed entry point?"},
            ],
            "independent_issues": [],
            "architectural_groups": [{
                "title": "Shared transaction boundary",
                "description": "Both findings manifest one defect.",
                "issue_ids": [CORE_UUID, TUI_UUID],
            }],
        })
        .to_string();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(payload)]));
        fixture.state.application.test_set_provider(provider).await;

        let stored = analyze(&fixture, &session, None)
            .await
            .expect("the classification is persisted");

        assert_eq!(stored.items[0].status, IssueStatus::Todo);
        assert_eq!(stored.items[1].status, IssueStatus::Discussion);
        let reloaded = ReviewStorage::load_review(&fixture.root, "rev-analyze")
            .unwrap()
            .expect("the session is stored");
        assert_eq!(reloaded.items[0].status, IssueStatus::Todo);
        assert_eq!(reloaded.items[1].status, IssueStatus::Discussion);
    }

    /// A finished finding keeps the lifecycle state it earned: re-running the
    /// analysis rewrites the grouping and never reopens a taken decision.
    #[tokio::test]
    async fn post_analysis_never_reopens_a_finished_finding() {
        let fixture = fixture("triage-terminal");
        let mut session = session();
        session.items[0].status = IssueStatus::Fixed;
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let payload = serde_json::json!({
            "summary": "The defect is still unambiguous.",
            "triage": [{"uuid": CORE_UUID, "status": "todo", "discussion_topic": null}],
            "independent_issues": [],
            "architectural_groups": [{
                "title": "Shared transaction boundary",
                "description": "One defect.",
                "issue_ids": [CORE_UUID],
            }],
        })
        .to_string();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(payload)]));
        fixture.state.application.test_set_provider(provider).await;

        let stored = analyze(&fixture, &session, None)
            .await
            .expect("the classification is persisted");

        assert_eq!(stored.items[0].status, IssueStatus::Fixed);
        assert_eq!(stored.items[0].group.as_deref(), Some("arch-1"));
        assert_eq!(stored.items[1].status, IssueStatus::Discussion);
    }

    /// The selected architect rung is the configuration the round runs on: the
    /// model and the reasoning depth of the picked entry reach the provider.
    #[tokio::test]
    async fn post_analysis_runs_on_the_selected_architect_entry() {
        let fixture = fixture("selection");
        let session = session();
        ReviewStorage::save_session(&fixture.root, &session).unwrap();
        configure_architect(
            &fixture,
            vec![
                ModelEntry::from(ModelRef::openrouter("test/architect")),
                ModelEntry::new(
                    ModelRef::openrouter("test/architect-strong"),
                    Some(ReasoningEffort::High),
                ),
            ],
        )
        .await;

        let payload = classification("One shared boundary.", &[CORE_UUID], &[]);
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(payload)]));
        let injected: Arc<dyn LlmProvider> = provider.clone();
        fixture.state.application.test_set_provider(injected).await;
        let selection = ModelSelection::Tier {
            model: ModelRef::openrouter("test/architect-strong"),
            effort: Some(ReasoningEffort::High),
        };

        let stored = analyze(&fixture, &session, Some(&selection))
            .await
            .expect("the selected rung classifies the session");

        let request = provider.last_request().expect("the provider was called");
        assert_eq!(request.model, "test/architect-strong");
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(stored.description.as_deref(), Some("One shared boundary."));
    }

    /// An absent selection keeps the base rung: the round runs the first
    /// architect entry, exactly as the automatic classification after a
    /// grouped audit does.
    #[tokio::test]
    async fn post_analysis_without_a_selection_runs_the_base_architect_rung() {
        let fixture = fixture("base-rung");
        let session = session();
        ReviewStorage::save_session(&fixture.root, &session).unwrap();
        configure_architect(
            &fixture,
            vec![
                ModelEntry::from(ModelRef::openrouter("test/architect")),
                ModelEntry::new(
                    ModelRef::openrouter("test/architect-strong"),
                    Some(ReasoningEffort::High),
                ),
            ],
        )
        .await;

        let payload = classification("One shared boundary.", &[CORE_UUID], &[]);
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(payload)]));
        let injected: Arc<dyn LlmProvider> = provider.clone();
        fixture.state.application.test_set_provider(injected).await;

        let stored = analyze(&fixture, &session, None)
            .await
            .expect("the base rung classifies the session");
        assert_eq!(stored.description.as_deref(), Some("One shared boundary."));

        let request = provider.last_request().expect("the provider was called");
        assert_eq!(request.model, "test/architect");
        assert!(
            request.reasoning_effort.is_none(),
            "the base entry declares no reasoning depth"
        );
    }

    /// A selection the architect role does not carry is refused by name before
    /// any provider call: the round never falls back to the base rung behind an
    /// explicit choice.
    #[tokio::test]
    async fn post_analysis_refuses_an_unconfigured_architect_selection() {
        let fixture = fixture("selection-foreign");
        let session = session();
        ReviewStorage::save_session(&fixture.root, &session).unwrap();
        configure_architect(
            &fixture,
            vec![ModelEntry::from(ModelRef::openrouter("test/architect"))],
        )
        .await;

        let payload = classification("Unused payload.", &[], &[]);
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(payload)]));
        let injected: Arc<dyn LlmProvider> = provider.clone();
        fixture.state.application.test_set_provider(injected).await;
        let selection = ModelSelection::Specific(ModelRef::openrouter("test/stranger"));

        let err = analyze(&fixture, &session, Some(&selection))
            .await
            .expect_err("an unconfigured selection must be refused");
        assert_eq!(err.code, errors::INVALID_MODEL);
        assert!(
            err.message.contains("test/stranger"),
            "the diagnostic must name the requested entry: {}",
            err.message
        );
        assert!(
            provider.requests().is_empty(),
            "a refused selection never reaches the provider"
        );

        let stored = ReviewStorage::load_review(&fixture.root, "rev-analyze")
            .unwrap()
            .expect("the session is stored");
        assert!(
            stored.post_analysis.is_none(),
            "a refused selection rewrites nothing"
        );
    }
    /// One post-analysis round obeys the transport's pacing contract: the verdict
    /// arrives at token granularity, so a client that keeps its pipe open but
    /// stopped reading must pause the relay instead of growing the outbound queue,
    /// and the drain of that queue must release the paused round.
    #[tokio::test]
    async fn post_analysis_backpressures_a_stalled_client_until_the_queue_drains() {
        let mut fixture = fixture("pacing");
        let session = session();
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let injected: Arc<dyn LlmProvider> =
            Arc::new(MockLlmProvider::scripted([MockResponse::text(
                classification("One shared boundary.", &[CORE_UUID], &[]),
            )]));
        fixture.state.application.test_set_provider(injected).await;

        // The client keeps its pipe open but stopped reading its stream.
        stall_outbound_transport(&fixture.state);
        let outbound = fixture.take_events();

        let state = Arc::clone(&fixture.state);
        let root = fixture.root.clone();
        let analyzed_session = session.clone();
        let operation = fixture
            .state
            .try_begin_operation()
            .expect("the operation slot is free");
        let analysis = tokio::spawn(async move {
            run_review_post_analysis(&state, &root, &analyzed_session, None, &operation, false)
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !analysis.is_finished(),
            "a stalled client must pause the post-analysis relay"
        );
        let stalled = ReviewStorage::load_review(&fixture.root, "rev-analyze")
            .unwrap()
            .expect("the session is stored");
        assert!(
            stalled.description.is_none(),
            "a stalled round must not persist its classification"
        );

        // Draining the queue below the low-water mark releases the paused relay.
        let sink = RecordingSink::default();
        let writing = tokio::spawn(fixture.state.out.writer(outbound).run_with(sink.clone()));
        let published = await_published(&sink, events::REVIEW_ANALYZE_FINISHED).await;
        assert!(
            published.contains(events::REVIEW_CONTENT_DELTA),
            "the drained round must publish the verdict it held"
        );

        let analyzed = analysis
            .await
            .expect("the analysis task joins")
            .expect("the released round persists its classification");
        assert_eq!(
            analyzed.description.as_deref(),
            Some("One shared boundary.")
        );

        writing.abort();
        let _ = writing.await;
    }
    /// Builds a `review/analyze` request naming one session.
    fn analyze_request(review_id: &str) -> Request {
        Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::REVIEW_ANALYZE.to_string(),
            params: Some(serde_json::json!({ "review_id": review_id })),
        }
    }

    /// The operation slot is claimed before the session is read, so a round owns
    /// the exclusion window its identity-heal save depends on. The refusal
    /// therefore precedes the resolution: a request naming a session that does
    /// not exist is refused because another operation runs, not because the
    /// session is missing. With a read that precedes the claim, a bounded update
    /// or a deletion landing in that window would be overwritten or undone by
    /// the heal save.
    #[tokio::test]
    async fn analyze_claims_the_operation_slot_before_it_resolves_the_session() {
        let fixture = fixture("claim-before-load");
        let operation = fixture
            .state
            .try_begin_operation()
            .expect("the operation slot is free");

        let refused = handle_review_analyze(analyze_request("rev-absent"), &fixture.state).await;
        assert_eq!(
            refused.error.expect("the request is refused").code,
            errors::OPERATION_IN_PROGRESS,
            "the slot refusal must precede the session resolution"
        );

        drop(operation);
        let missing = handle_review_analyze(analyze_request("rev-absent"), &fixture.state).await;
        assert_eq!(
            missing.error.expect("the request is refused").code,
            errors::NOT_FOUND,
            "the same request resolves the session once the slot is free"
        );
        assert!(
            !fixture.state.is_operation_active(),
            "an unresolvable round must release the slot it claimed"
        );
    }

    /// A round heals a session stored before finding identities existed and
    /// persists the identities inside its exclusion window, before the provider
    /// is called, so a classification can reference them even when the round
    /// itself fails afterwards. Healing rewrites the loaded copy, never the
    /// snapshot the caller passed in.
    #[tokio::test]
    async fn post_analysis_heals_missing_identities_before_the_provider_call() {
        let fixture = fixture("heal");
        let mut session = session();
        for item in &mut session.items {
            item.uuid = None;
        }
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        // A payload whose verdict is empty is refused by the classification
        // contract after the heal save, so the identities survive a round that
        // produced no usable classification.
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            classification("", &[], &[]),
        )]));
        let injected: Arc<dyn LlmProvider> = provider.clone();
        fixture.state.application.test_set_provider(injected).await;

        let err = analyze(&fixture, &session, None)
            .await
            .expect_err("a classification without a verdict is refused");
        assert_eq!(err.code, errors::SCHEMA_ERROR);
        assert_eq!(
            provider.requests().len(),
            1,
            "the heal save precedes the only provider call of the round"
        );

        let stored = ReviewStorage::load_review(&fixture.root, "rev-analyze")
            .unwrap()
            .expect("the session is stored");
        let minted: Vec<String> = stored
            .items
            .iter()
            .map(|item| item.uuid.clone().expect("every finding was healed"))
            .collect();
        assert_eq!(minted.len(), 2);
        for uuid in &minted {
            assert!(
                tauqe_core::review::uuid::is_valid_uuid_v7(uuid),
                "a healed identity must be UUIDv7: {uuid}"
            );
        }
        assert_ne!(
            minted[0], minted[1],
            "every finding receives its own identity"
        );
        assert!(
            session.items.iter().all(|item| item.uuid.is_none()),
            "healing rewrites the round's own copy, never the caller's snapshot"
        );
    }
}
