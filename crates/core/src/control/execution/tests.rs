use super::*;
use crate::control::repl::repl_capabilities;
use crate::llm::LlmCapability;
use crate::prompt::json::JsonPromptProtocol;
use crate::providers::mock::{MockLlmProvider, MockResponse};
use crate::workflow::policy::DirectApplyPolicy;

struct Fixture {
    root: tempfile::TempDir,
    context: ContextManager,
    provider: MockLlmProvider,
    model: ModelRef,
    config: AppConfig,
    cancel: CancellationToken,
    events: EventSink,
    policy: DirectApplyPolicy,
}

impl Fixture {
    fn new(responses: Vec<MockResponse>) -> Self {
        let root = tempfile::tempdir().unwrap();
        crate::git::init_repository(Some(root.path()), true).unwrap();
        let mut config = AppConfig::default();
        config.context.include_repo_map = false;
        Self {
            context: ContextManager::new(root.path().to_path_buf()),
            root,
            provider: MockLlmProvider::scripted(responses),
            model: ModelRef::openrouter("mock/model"),
            config,
            cancel: CancellationToken::new(),
            events: EventSink::noop(),
            policy: DirectApplyPolicy::new(),
        }
    }

    async fn start(
        &mut self,
        unit: &mut ExecutionUnit,
        protocol: &JsonPromptProtocol,
        capabilities: CapabilityTree,
        history: Option<&HistorySnapshot>,
    ) {
        unit.execute_bounded(
            TurnExecutionConfig {
                repo_root: self.root.path(),
                provider: &self.provider,
                model: &self.model,
                protocol,
                config: &self.config,
                session_id: None,
                reasoning_effort: None,
            },
            BoundedUnitRequest {
                context: &mut self.context,
                history,
                capabilities,
                task: "Answer the request".into(),
                completion: TurnCompletion::AnswerAllowed,
                brief: None,
            },
            &TurnLoopConfig::from_app_config(&self.config),
            &mut self.policy,
            &self.cancel,
            &self.events,
        )
        .await
        .unwrap();
    }

    async fn continue_with(
        &mut self,
        unit: &mut ExecutionUnit,
        protocol: &JsonPromptProtocol,
        capabilities: CapabilityTree,
        history: Option<&HistorySnapshot>,
    ) -> Result<ResponseEnvelope, SystemError> {
        unit.continue_round(
            TurnExecutionConfig {
                repo_root: self.root.path(),
                provider: &self.provider,
                model: &self.model,
                protocol,
                config: &self.config,
                session_id: None,
                reasoning_effort: None,
            },
            ContinuationRequest {
                context: &mut self.context,
                history,
                capabilities,
                diagnostics: "Correct the refused payload",
            },
            &mut self.policy,
            &self.cancel,
            &self.events,
        )
        .await
    }
}

#[tokio::test]
async fn continuation_refuses_a_call_after_the_last_available_round() {
    let mut fixture = Fixture::new(vec![MockResponse::text("First answer")]);
    fixture.config.repl.max_turn_rounds = 1;
    let capabilities = repl_capabilities(None);
    let protocol = JsonPromptProtocol::new(capabilities.clone());
    let mut unit = ExecutionUnit::default();
    fixture
        .start(&mut unit, &protocol, capabilities.clone(), None)
        .await;

    assert!(matches!(
        fixture
            .continue_with(&mut unit, &protocol, capabilities, None)
            .await,
        Err(SystemError::RoundBudgetExhausted { spent: 1, .. })
    ));
    assert_eq!(fixture.provider.requests().len(), 1);
}

#[tokio::test]
async fn continuation_keeps_researched_files_and_new_units_do_not_inherit_them() {
    let mut fixture = Fixture::new(vec![
        MockResponse::text(
            serde_json::json!({
                "message": "Read the helper",
                "action": {"kind": "research", "context_requests": [
                    {"path": "helper.rs", "access": "read_only"}
                ]}
            })
            .to_string(),
        ),
        MockResponse::text("Read complete"),
        MockResponse::text("Correction complete"),
        MockResponse::text("New unit"),
    ]);
    std::fs::write(
        fixture.root.path().join("helper.rs"),
        "pub const HELPER: i32 = 42;\n",
    )
    .unwrap();
    crate::git::cmd::stage_files(fixture.root.path(), &["helper.rs".into()]).unwrap();
    crate::git::cmd::commit_with_developer_author(fixture.root.path(), "add helper", false)
        .unwrap();
    let capabilities = repl_capabilities(None);
    let protocol = JsonPromptProtocol::new(capabilities.clone());
    let mut unit = ExecutionUnit::default();
    fixture
        .start(&mut unit, &protocol, capabilities.clone(), None)
        .await;
    assert!(fixture.context.contains("helper.rs").unwrap());
    assert!(fixture
        .continue_with(&mut unit, &protocol, capabilities.clone(), None)
        .await
        .unwrap()
        .has(LlmCapability::TextMessage));
    let requests = fixture.provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[2]
        .messages
        .iter()
        .any(|m| m.content.contains("pub const HELPER: i32 = 42;")));
    assert!(requests[2]
        .messages
        .iter()
        .any(|m| m.content.contains("Correct the refused payload")));

    fixture.context.remove_file("helper.rs").unwrap();
    let mut new_unit = ExecutionUnit::default();
    fixture
        .start(&mut new_unit, &protocol, capabilities, None)
        .await;
    let requests = fixture.provider.requests();
    assert_eq!(requests.len(), 4);
    assert!(!requests[3]
        .messages
        .iter()
        .any(|m| m.content.contains("pub const HELPER: i32 = 42;")));
    assert!(!requests[3]
        .messages
        .iter()
        .any(|m| m.content.contains("Correct the refused payload")));
}

#[tokio::test]
async fn continuation_revokes_capabilities_and_withdraws_history() {
    let mut fixture = Fixture::new(vec![
        MockResponse::text("First"),
        MockResponse::text("Second"),
    ]);
    let history = HistorySnapshot::new(vec![crate::history::HistoryEntry::Summary {
        id: Some(1),
        text: "Prior history marker".into(),
    }]);
    let capabilities = repl_capabilities(None);
    let protocol = JsonPromptProtocol::new(capabilities.clone());
    let mut unit = ExecutionUnit::default();
    fixture
        .start(&mut unit, &protocol, capabilities.clone(), Some(&history))
        .await;
    let narrowed = capabilities
        .filter(&|cap| cap == LlmCapability::TextMessage)
        .unwrap();
    fixture
        .continue_with(&mut unit, &protocol, narrowed, None)
        .await
        .unwrap();
    let requests = fixture.provider.requests();
    assert!(requests[0]
        .messages
        .iter()
        .any(|m| m.content.contains("Prior history marker")));
    assert!(!requests[1]
        .messages
        .iter()
        .any(|m| m.content.contains("Prior history marker")));
    let schema = serde_json::to_string(&requests[1].response_format).unwrap();
    assert!(!schema.contains("context_requests"));
    assert!(!schema.contains("plan_create"));
    assert!(requests[1]
        .messages
        .iter()
        .any(|m| m.content.contains("Correct the refused payload")));
}
