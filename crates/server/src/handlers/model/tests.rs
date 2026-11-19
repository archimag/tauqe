//! Unit tests for the model turn subsystem.
//!
//! The tests pin the deterministic decisions taken before any provider request
//! is launched: which turn intent a typed discussion target establishes and
//! which commands the selected protocol admits. Every assertion
//! targets a pure function, so the suite needs no `AppState`, no provider, and
//! no repository.

use crate::contracts::Project;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauqe_core::config::AppConfig;
use tauqe_core::llm::LlmCapability;
use tauqe_core::llm::LlmCommand;
use tauqe_core::models::{ModelRef, ModelSelection, ProviderKind};
use tauqe_core::plan::DiscussionPlanAction;
use tauqe_core::prompt::protocol::PromptProtocol;
use tauqe_core::providers::{LlmProvider, StreamEvent};
use tauqe_protocol::{DiscussionFocus, Request};
use tokio::sync::{mpsc, watch};

use super::{handle_model_cancel, handle_model_clear_history, handle_model_select};
use crate::handlers::common::test_support::scratch_root;
use crate::handlers::history::handle_history_get;
use crate::handlers::PreflightVerdict;
use crate::publication::publish_stream;
use crate::state::AppState;
use tauqe_flow::api::turn_prompt_protocol as resolve_turn_protocol;
/// Capability surface of a free interactive dialogue.
fn dialogue_capabilities() -> tauqe_core::llm::CapabilityTree {
    tauqe_core::control::repl_capabilities(None)
}

/// Capability surface of a plan-anchored dialogue.
fn plan_dialogue_capabilities() -> tauqe_core::llm::CapabilityTree {
    tauqe_core::control::repl_capabilities(Some(&tauqe_core::discussion::DiscussionTarget::Plan {
        id: "plan-1".to_string(),
    }))
}

/// Capability surface of a review-anchored dialogue.
fn review_dialogue_capabilities() -> tauqe_core::llm::CapabilityTree {
    tauqe_core::control::repl_capabilities(Some(
        &tauqe_core::discussion::DiscussionTarget::Review {
            id: "review-1".to_string(),
        },
    ))
}

/// A plan-anchored dialogue declares plan mutation beside the dialogue surface,
/// so the turn may record plan changes without leaving the discussion.
#[test]
fn plan_discussion_declares_plan_mutation() {
    let capabilities = plan_dialogue_capabilities();
    assert!(capabilities.contains(LlmCapability::PlanUpdate));
    assert!(capabilities.contains(LlmCapability::TextMessage));
}

/// A review finding dialogue establishes its own regime, because the decision
/// the dialogue records is authorized by that regime alone. The developer still
/// holds the full finding context and may ask for the fix directly.
#[test]
fn review_discussion_is_authorized_by_its_surface() {
    let capabilities = review_dialogue_capabilities();
    assert!(capabilities.contains(LlmCapability::ReviewUpdate));
    assert!(!capabilities.contains(LlmCapability::PlanUpdate));

    // A group dialogue shares the surface: the plan of the group is the unit of
    // remediation, and the decision it records belongs to the same anchored
    // session.
    let group = tauqe_core::discussion::DiscussionTarget::ReviewGroup {
        session_id: "review-1".to_string(),
        group_id: "arch-1".to_string(),
    };
    assert!(
        tauqe_core::control::repl_capabilities(Some(&group)).contains(LlmCapability::ReviewUpdate)
    );
}

/// A decision recorded for one finding through the Structured Output transport.
const REVIEW_DECISION_JSON: &str = r#"{"message":"Recording the decision.","action":{"kind":"review","review_update":{"item_id":7,"status":"rejected","note":"The race cannot happen"}}}"#;

/// Decodes `raw` and returns the review decisions that survived decoding.
///
/// The helper keeps every assertion focused on the regime authorization instead
/// of on the surrounding command vocabulary, which differs between the two
/// transports.
fn decoded_review_updates(
    protocol: &dyn PromptProtocol,
    raw: &str,
) -> Vec<tauqe_core::review::ReviewUpdateProposal> {
    protocol
        .decode(raw, &[])
        .into_iter()
        .filter_map(|command| match command {
            LlmCommand::ReviewUpdate { update } => Some(update),
            _ => None,
        })
        .collect()
}

/// The declared intent authorizes the decision: the structured contour
/// advertises the field and decodes it into the typed command the engine
/// persists against the anchored session.
#[test]
fn review_discussion_decodes_the_recorded_decision() {
    let structured = resolve_turn_protocol(review_dialogue_capabilities());
    let surface = structured.capability_expr();
    assert!(surface.contains(LlmCapability::ReviewUpdate));

    let decisions = decoded_review_updates(structured.as_ref(), REVIEW_DECISION_JSON);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].item_id, 7);
    assert_eq!(
        decisions[0].status,
        tauqe_core::review::ReviewDecisionStatus::Rejected
    );
    assert_eq!(decisions[0].note, "The race cannot happen");
}

/// The decision never leaves the regime that authorizes it, and the XML contour
/// never carries it: the field owns no control tag, so it travels exclusively
/// through Structured Output.
#[test]
fn the_recorded_decision_never_leaves_the_review_regime() {
    for capabilities in [
        dialogue_capabilities(),
        plan_dialogue_capabilities(),
        tauqe_core::control::turn::turn_capability_expr(),
    ] {
        let protocol = resolve_turn_protocol(capabilities);
        let surface = protocol.capability_expr();
        assert!(!surface.contains(LlmCapability::ReviewUpdate));
        let decisions = decoded_review_updates(protocol.as_ref(), REVIEW_DECISION_JSON);
        assert!(decisions.is_empty());
    }

    let review = resolve_turn_protocol(review_dialogue_capabilities());
    assert!(review
        .capability_expr()
        .contains(LlmCapability::ReviewUpdate));
    let decisions = decoded_review_updates(review.as_ref(), REVIEW_DECISION_JSON);
    assert_eq!(decisions.len(), 1);
}
/// Builds a `model/select` request for the given selection.
fn select_request(selection: ModelSelection) -> Request {
    Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: "model/select".to_string(),
        params: Some(
            serde_json::to_value(tauqe_protocol::ModelSelectParams {
                selection: selection.project(),
            })
            .unwrap(),
        ),
    }
}

/// Switching the effective model starts a cold dialogue session, because a
/// provider's prefix cache is bound to one concrete model; reselecting the same
/// model keeps the warm session.
#[tokio::test]
async fn model_select_rotates_the_session_confirmed_by_the_effective_model() {
    let mut cfg = AppConfig::default();
    cfg.models.repl = vec![
        ModelRef::openrouter("alpha/model").into(),
        ModelRef::openrouter("beta/model").into(),
    ];

    let (out, _rx) = crate::transport::OutChannel::new();
    let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
    let root = scratch_root("model-select-rotate");
    let state = AppState::new(cfg, root.clone(), history_sender, out);

    let warm = state.session_uuid().await;
    let switched = handle_model_select(
        select_request(ModelSelection::Specific(ModelRef::openrouter("beta/model"))),
        &state,
    )
    .await;
    assert!(switched.error.is_none(), "the switch must succeed");
    assert_ne!(
        warm,
        state.session_uuid().await,
        "a model change must rotate the session"
    );

    let stable = state.session_uuid().await;
    let repeated = handle_model_select(
        select_request(ModelSelection::Specific(ModelRef::openrouter("beta/model"))),
        &state,
    )
    .await;
    assert!(repeated.error.is_none(), "the reselection must succeed");
    assert_eq!(
        stable,
        state.session_uuid().await,
        "an unchanged effective model must keep the session"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Plan mutations emitted through Structured Output by a dialogue that
/// advertises plan authorship alone, so the payload stays at the branch level.
const PLAN_UPDATE_JSON: &str = r#"{"message":"Updating","action":{"kind":"plan","plan_update":{"action":"update","id":"p-1"}}}"#;
const PLAN_SAVE_JSON: &str = r#"{"message":"Saving","action":{"kind":"plan","plan_create":{"action":"save","id":"p-2","title":"P2"}}}"#;

/// Plan mutations of a dialogue that advertises both plan capabilities, so the
/// payload travels inside the `plan_action` variant the round selects.
const PLAN_ACTION_UPDATE_JSON: &str = r#"{"message":"Updating","action":{"kind":"plan","plan_action":{"kind":"modify","plan_update":{"action":"update","id":"p-1"}}}}"#;
const PLAN_ACTION_SAVE_JSON: &str = r#"{"message":"Saving","action":{"kind":"plan","plan_action":{"kind":"create","plan_create":{"action":"save","id":"p-2","title":"P2"}}}}"#;

/// Decodes `raw` and returns the plan actions that survived decoding.
///
/// The helper keeps every assertion focused on the plan permissions matrix
/// instead of on the surrounding command vocabulary, which differs between the
/// two transports.
fn decoded_plan_actions(protocol: &dyn PromptProtocol, raw: &str) -> Vec<DiscussionPlanAction> {
    protocol
        .decode(raw, &[])
        .into_iter()
        .filter_map(|command| match command {
            LlmCommand::Plan { update } => Some(update.action),
            _ => None,
        })
        .collect()
}

/// A plan discussion is a dialogue like any other: the configured transport
/// encodes it, and the transport never shrinks its surface. Both encodings
/// therefore advertise the edit capability and decode the very same plan
/// mutation, which the discussion authorizes by its regime.
/// A plan discussion is a dialogue like any other: the transport never shrinks
/// its surface, so the anchored dialogue advertises the edit capability and
/// decodes the very same plan mutation the discussion authorizes.
#[test]
fn plan_discussion_follows_the_configured_transport() {
    let protocol = resolve_turn_protocol(plan_dialogue_capabilities());
    assert_eq!(
        protocol.kind(),
        tauqe_core::workflow::EditProtocolKind::Structured
    );
    assert!(protocol
        .capability_expr()
        .contains(LlmCapability::EditReplace));
    assert!(protocol
        .capability_expr()
        .contains(LlmCapability::PlanUpdate));
    assert_eq!(
        decoded_plan_actions(protocol.as_ref(), PLAN_ACTION_UPDATE_JSON),
        vec![DiscussionPlanAction::Update]
    );
    assert_eq!(
        decoded_plan_actions(protocol.as_ref(), PLAN_ACTION_SAVE_JSON),
        vec![DiscussionPlanAction::Save]
    );
}

/// The declared intent alone decides plan mutation: an interactive turn
/// composes the interactive regime profile on both transports, so the mutation
/// is neither advertised nor decoded, while plan authorship stays available,
/// because a user may ask for a plan at any moment of a dialogue.
#[test]
fn interactive_turn_never_carries_plan_mutation() {
    let protocol = resolve_turn_protocol(dialogue_capabilities());
    assert!(protocol
        .capability_expr()
        .contains(LlmCapability::PlanCreation));
    assert!(!protocol
        .capability_expr()
        .contains(LlmCapability::PlanUpdate));
    assert!(decoded_plan_actions(protocol.as_ref(), PLAN_UPDATE_JSON).is_empty());
    assert_eq!(
        decoded_plan_actions(protocol.as_ref(), PLAN_SAVE_JSON),
        vec![DiscussionPlanAction::Save]
    );
}

/// Asserts the plan permissions of one isolated step.
///
/// The turn-intent narrowing runs after protocol resolution, exactly as the
/// bounded-turn FSM narrows the decoder, so the assertion covers the
/// end-to-end surface of the step.
fn assert_step_has_no_plan_permission() {
    let protocol = resolve_turn_protocol(tauqe_core::control::turn::turn_capability_expr());
    assert!(!protocol
        .capability_expr()
        .contains(LlmCapability::PlanCreation));
    assert!(!protocol
        .capability_expr()
        .contains(LlmCapability::PlanUpdate));
    for raw in [PLAN_UPDATE_JSON, PLAN_SAVE_JSON] {
        assert!(
            decoded_plan_actions(protocol.as_ref(), raw).is_empty(),
            "an isolated step must decode no plan command"
        );
    }
}

/// An isolated step keeps the plan immutable: neither the mutation a discussion
/// anchor grants nor plan authorship survives, whatever the transport.
#[test]
fn test_isolated_step_has_no_plan_permission() {
    assert_step_has_no_plan_permission();
}
/// Constructs an [`AppState`] over a scratch repository root, isolating each
/// focus test's stored plans and history from every other test.
fn state_over_root(root: std::path::PathBuf) -> std::sync::Arc<AppState> {
    let (out, _rx) = crate::transport::OutChannel::new();
    let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
    AppState::new(AppConfig::default(), root, history_sender, out)
}

// RPC decoding precedes admission, which precedes discussion preparation.
#[tokio::test]
async fn model_ask_decodes_before_admission_and_does_not_prepare_while_busy() {
    let root = scratch_root("ask-busy");
    let (out, mut events) = crate::transport::OutChannel::new();
    let (history, _) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        root.clone(),
        history,
        out,
    );
    state
        .application
        .set_discussion_focus(tauqe_flow::api::DiscussionFocus::Focused {
            target: tauqe_flow::api::DiscussionTarget::Plan {
                id: "missing-plan".into(),
            },
            focused_items: Vec::new(),
        })
        .await;
    let focus = state.application.discussion_focus().await;
    let operation = state.try_begin_operation().unwrap();
    let request = |params| tauqe_protocol::Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::MODEL_ASK.into(),
        params: Some(params),
    };
    let malformed = super::handle_model_ask(
        request(serde_json::json!({"kind":"prompt","text":42})),
        &state,
    )
    .await;
    assert_eq!(
        malformed.error.unwrap().code,
        tauqe_protocol::errors::INVALID_PARAMS
    );
    let busy = super::handle_model_ask(
        request(serde_json::json!({"kind":"prompt","text":"Continue"})),
        &state,
    )
    .await;
    assert_eq!(
        busy.error.unwrap().code,
        tauqe_protocol::errors::OPERATION_IN_PROGRESS
    );
    assert_eq!(state.application.discussion_focus().await, focus);
    assert!(events.try_recv().is_err());
    drop(operation);
    let _ = std::fs::remove_dir_all(root);
}

/// Runs the deterministic pre-flight gate over a scratch repository that
/// declares `config`, then removes it.
async fn preflight_with(config: Option<&str>) -> PreflightVerdict {
    let dir = scratch_root("integrity");
    if let Some(config) = config {
        std::fs::write(dir.join("tauqe.toml"), config).expect("write the project declaration");
    }
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let verdict = crate::handlers::run_preflight_verification(&dir, cancel_rx).await;
    let _ = std::fs::remove_dir_all(&dir);
    verdict
}

/// The pre-flight gate refuses a `tauqe.toml` that is not valid TOML, so no turn
/// starts against a configuration the engine cannot even parse.
#[tokio::test]
async fn preflight_rejects_malformed_tauqe_toml() {
    match preflight_with(Some("[models\nprimary = \"broken\"\n")).await {
        PreflightVerdict::Failed(report) => {
            assert!(report.contains("tauqe.toml"), "got: {report}");
        }
        other => panic!("malformed TOML must be refused before any model call: {other:?}"),
    }
}

/// The gate refuses a declaration that parses as TOML but violates a semantic
/// invariant: here the verification lifecycle routes through a command nobody
/// declares. A missing `[project.verify]` section is not such a violation,
/// because verification is a declared capability rather than a mandatory
/// barrier.
#[tokio::test]
async fn preflight_rejects_semantically_invalid_tauqe_toml() {
    match preflight_with(Some(concat!(
        "[models]\nrepl = [\"openrouter:a/b\"]\n",
        "\n[project.verify]\ncommand = \"@missing\"\n",
    )))
    .await
    {
        PreflightVerdict::Failed(report) => {
            assert!(report.contains("missing"), "got: {report}");
        }
        other => panic!("a broken command chain must be refused: {other:?}"),
    }
}

/// A declaration that only assigns model roles is complete: verification is
/// optional, so the gate passes it instead of demanding a barrier the project
/// never declared.
#[tokio::test]
async fn preflight_accepts_a_declaration_without_verification() {
    let verdict = preflight_with(Some("[models]\nrepl = [\"openrouter:a/b\"]\n")).await;
    assert!(
        matches!(verdict, PreflightVerdict::Passed),
        "a declaration without verification must pass: {verdict:?}"
    );
}

/// A declaration whose verification command resolves passes the gate untouched.
#[tokio::test]
async fn preflight_accepts_a_valid_tauqe_toml() {
    let verdict = preflight_with(Some(
        "[models]\nrepl = [\"openrouter:a/b\"]\n\n[project.verify]\ncommand = \"true\"\n",
    ))
    .await;
    assert!(
        matches!(verdict, PreflightVerdict::Passed),
        "got: {verdict:?}"
    );
}

/// A repository without a declaration is governed by built-in defaults, so the
/// gate has nothing to reject.
#[tokio::test]
async fn preflight_accepts_a_repository_without_a_declaration() {
    let verdict = preflight_with(None).await;
    assert!(
        matches!(verdict, PreflightVerdict::Passed),
        "got: {verdict:?}"
    );
}

/// Provider that streams one fragment and then parks until the harness cancels
/// the turn, holding a bounded turn open at a deterministic point.
struct StallingProvider {
    /// Signals that the provider is suspended mid-stream.
    entered: mpsc::UnboundedSender<()>,
    /// Set while the provider is parked on the cancellation signal.
    parked: Arc<AtomicBool>,
}

impl StallingProvider {
    /// Builds the provider together with the receiver that observes its stall.
    fn new() -> (Self, mpsc::UnboundedReceiver<()>) {
        let (entered, receiver) = mpsc::unbounded_channel();
        (
            Self {
                entered,
                parked: Arc::new(AtomicBool::new(false)),
            },
            receiver,
        )
    }

    /// Reports whether the provider is currently parked mid-stream.
    fn is_parked(&self) -> bool {
        self.parked.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl LlmProvider for StallingProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenRouter
    }

    async fn stream(
        &self,
        _request: tauqe_core::providers::LlmRequest<'_>,
        tx: mpsc::Sender<StreamEvent>,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> tauqe_core::providers::ProviderResult {
        // Stream a fragment first, so the turn is genuinely mid-stream when the
        // test dispatches its requests.
        if tx
            .send(StreamEvent::TextDelta("Reading the repository".to_string()))
            .await
            .is_err()
        {
            return Ok(());
        }
        self.parked.store(true, Ordering::SeqCst);
        let _ = self.entered.send(());
        // Park until the harness cancels the turn: the request loop has to stay
        // free to deliver `model/cancel` while this stream is open.
        while !*cancel_rx.borrow() {
            if cancel_rx.changed().await.is_err() {
                break;
            }
        }
        self.parked.store(false, Ordering::SeqCst);
        let _ = tx.send(StreamEvent::Cancelled).await;
        Ok(())
    }
}

/// Builds a `history/get` request for the stored session.
fn history_get_request() -> Request {
    Request {
        id: tauqe_protocol::RequestId::Number(2),
        method: tauqe_protocol::methods::HISTORY_GET.to_string(),
        params: None,
    }
}

/// Builds an immediate `model/cancel` request.
fn cancel_request() -> Request {
    Request {
        id: tauqe_protocol::RequestId::Number(3),
        method: tauqe_protocol::methods::MODEL_CANCEL.to_string(),
        params: None,
    }
}

/// Builds a `model/cancel` request carrying `params`.
fn cancel_request_with(params: serde_json::Value) -> Request {
    Request {
        id: tauqe_protocol::RequestId::Number(5),
        method: tauqe_protocol::methods::MODEL_CANCEL.to_string(),
        params: Some(params),
    }
}

/// A present-but-malformed cancellation payload is refused instead of silently
/// degrading to the default hard stop: a mistyped soft stop must not
/// hard-cancel the running operation and arm its abort watchdog.
#[tokio::test]
async fn model_cancel_refuses_a_malformed_payload_without_cancelling() {
    let root = scratch_root("cancel-malformed");
    let state = state_over_root(root.clone());
    let operation = state
        .try_begin_operation()
        .expect("operation slot must be free");
    let cancel_rx = operation.cancel_receiver();
    let soft_rx = operation.soft_stop_receiver();

    // An unknown mode string and a wrongly typed field are both malformed
    // payloads, so both are answered with the parameter error.
    for params in [
        serde_json::json!({ "mode": "sometimes" }),
        serde_json::json!({ "mode": 7 }),
    ] {
        let refused = handle_model_cancel(cancel_request_with(params), &state).await;
        assert_eq!(
            refused
                .error
                .expect("the malformed payload must be refused")
                .code,
            tauqe_protocol::errors::INVALID_PARAMS
        );
        assert!(
            !*cancel_rx.borrow(),
            "a refused payload must not hard-cancel the operation"
        );
        assert!(
            !*soft_rx.borrow(),
            "a refused payload must not request a soft stop"
        );
    }
    assert!(
        state.is_operation_active(),
        "the running operation must keep owning the slot"
    );

    drop(operation);
    let _ = std::fs::remove_dir_all(&root);
}

/// A correctly spelled soft stop drains the operation and never raises the
/// hard-cancel signal, while an absent payload keeps the default immediate
/// abort of a client that predates two-mode cancellation.
#[tokio::test]
async fn model_cancel_soft_payload_drains_and_absent_payload_hard_cancels() {
    let root = scratch_root("cancel-modes");
    let state = state_over_root(root.clone());

    let operation = state
        .try_begin_operation()
        .expect("operation slot must be free");
    let cancel_rx = operation.cancel_receiver();
    let soft_rx = operation.soft_stop_receiver();
    let soft = handle_model_cancel(
        cancel_request_with(serde_json::json!({ "mode": "soft" })),
        &state,
    )
    .await;
    assert!(
        soft.error.is_none(),
        "a soft stop must be accepted: {:?}",
        soft.error
    );
    assert!(*soft_rx.borrow(), "the soft stop must reach the supervisor");
    assert!(!*cancel_rx.borrow(), "a soft stop must not hard-cancel");
    assert!(
        state.is_operation_active(),
        "a soft stop leaves the operation registered"
    );
    drop(operation);

    let operation = state
        .try_begin_operation()
        .expect("operation slot must be free");
    let cancel_rx = operation.cancel_receiver();
    let hard = handle_model_cancel(cancel_request(), &state).await;
    assert!(
        hard.error.is_none(),
        "an absent payload must keep the immediate abort: {:?}",
        hard.error
    );
    assert!(
        *cancel_rx.borrow(),
        "an absent payload must raise the hard-cancel signal"
    );
    drop(operation);

    let _ = std::fs::remove_dir_all(&root);
}

/// A bounded turn must not hold the shared history guard for its duration.
///
/// The daemon serves requests from one stdin loop and awaits every inline
/// handler, so a turn that kept the exclusive history guard would block
/// `history/get` for minutes and would stop reading `model/cancel` — the only
/// channel that can interrupt it. The turn therefore works on a private clone
/// of the manager and adopts it back when it ends.
#[tokio::test]
async fn history_read_and_cancellation_are_served_while_a_turn_streams() {
    let root = scratch_root("turn-history");
    tauqe_core::git::init_repository(Some(&root), true).expect("initialise the scratch repository");
    std::fs::write(root.join("README.md"), "# scratch\n").expect("write the repository fixture");
    tauqe_core::git::cmd::stage_files(&root, &["README.md".to_string()])
        .expect("stage the fixture");
    tauqe_core::git::cmd::commit_with_developer_author(&root, "Add fixture", false)
        .expect("commit the fixture");

    let state = state_over_root(root.clone());
    let (provider, mut entered) = StallingProvider::new();
    let provider = Arc::new(provider);
    let injected: Arc<dyn LlmProvider> = provider.clone();
    state.application.test_set_provider(injected).await;

    state
        .application
        .ask(
            tauqe_flow::api::ModelAskInput::Prompt {
                text: "Explain the repository".into(),
            },
            Arc::new(crate::publication::Publisher {
                out: state.out.clone(),
            }),
        )
        .await
        .expect("the turn starts");

    tokio::time::timeout(Duration::from_secs(10), entered.recv())
        .await
        .expect("the turn must reach the provider")
        .expect("the provider signals that it is parked");

    // The turn is parked mid-stream on no shared guard, so the history read is
    // answered while the turn runs instead of waiting for it to finish.
    let read = tokio::time::timeout(
        Duration::from_secs(10),
        handle_history_get(history_get_request(), &state),
    )
    .await
    .expect("history/get must not wait for the running turn");
    assert!(read.error.is_none(), "history/get failed: {:?}", read.error);
    let page: tauqe_protocol::HistoryGetResult =
        serde_json::from_value(read.result.expect("history/get result"))
            .expect("history/get result deserializes");
    assert!(
        page.items
            .iter()
            .any(|item| item.text == "Explain the repository"),
        "the running turn's prompt must already be visible in the journal"
    );

    // Cancellation travels through the same loop and must reach the parked
    // stream.
    let cancelled = tokio::time::timeout(
        Duration::from_secs(10),
        handle_model_cancel(cancel_request(), &state),
    )
    .await
    .expect("model/cancel must not wait for the running turn");
    assert!(cancelled.error.is_none(), "model/cancel failed");

    let mut released = false;
    for _ in 0..500 {
        if !state.is_operation_active() {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        released,
        "the cancelled turn must release the operation slot"
    );
    assert!(
        !provider.is_parked(),
        "the provider must observe the cancellation"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Builds a `model/clearHistory` request.
fn clear_history_request() -> Request {
    Request {
        id: tauqe_protocol::RequestId::Number(4),
        method: tauqe_protocol::methods::MODEL_CLEAR_HISTORY.to_string(),
        params: None,
    }
}

/// `model/select` mutates the shared selection and rotates the dialogue session
/// that `config/set` protects by claiming the slot, so the identical state change
/// is guarded with the identical strength: while an operation owns the slot the
/// switch is refused and the selection a running turn resolved stays as it was.
#[tokio::test]
async fn model_select_is_refused_while_an_operation_is_registered() {
    let mut cfg = AppConfig::default();
    cfg.models.repl = vec![
        ModelRef::openrouter("alpha/model").into(),
        ModelRef::openrouter("beta/model").into(),
    ];

    let (out, _rx) = crate::transport::OutChannel::new();
    let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
    let root = scratch_root("model-select-guard");
    let state = AppState::new(cfg, root.clone(), history_sender, out);

    let operation = state
        .try_begin_operation()
        .expect("operation slot must be free");
    let before = state.application.configuration().await.selection.clone();

    let refused = handle_model_select(
        select_request(ModelSelection::Specific(ModelRef::openrouter("beta/model"))),
        &state,
    )
    .await;
    assert_eq!(
        refused.error.expect("the switch must be refused").code,
        tauqe_protocol::errors::OPERATION_IN_PROGRESS
    );
    assert_eq!(
        state.application.configuration().await.selection,
        before,
        "a refused switch must not change the selection"
    );
    assert!(
        state.is_operation_active(),
        "the running operation must keep owning the slot"
    );

    drop(operation);
    assert!(!state.is_operation_active());
    let _ = std::fs::remove_dir_all(&root);
}

/// Clearing the journal truncates `history.jsonl` and rotates the dialogue
/// session. A turn records its result against a private clone of the history
/// manager and adopts that clone back when it ends, so a turn that registered in
/// an advisory check's window would swap the pre-truncation clone over the
/// cleared journal and resurrect every removed record. The handler claims the
/// slot instead, which makes the two mutually exclusive: while the turn is
/// registered the clear is refused, the journal stays intact for the turn's
/// adoption, and the same request succeeds once the slot is free.
#[tokio::test]
async fn model_clear_history_is_refused_and_never_resurrects_a_cleared_journal() {
    let root = scratch_root("clear-history-exclusive");
    let state = state_over_root(root.clone());

    state
        .application
        .record_history_turn("Keep me".into(), Vec::new())
        .await
        .expect("the record must persist");

    // A bounded turn works on a private clone of the manager and adopts it back
    // when it ends, so this clone is exactly what a running turn holds.
    let turn_clone = state
        .application
        .test_mutate_history(|history| history.clone())
        .await
        .unwrap();
    let session = state.session_uuid().await;
    let operation = state
        .try_begin_operation()
        .expect("operation slot must be free");

    let refused = handle_model_clear_history(clear_history_request(), &state).await;
    assert_eq!(
        refused.error.expect("the clear must be refused").code,
        tauqe_protocol::errors::OPERATION_IN_PROGRESS
    );

    // The journal was never truncated, so the turn's adoption cannot resurrect a
    // cleared transcript, and the session the turn resolved is still current.
    state
        .application
        .test_mutate_history(move |history| *history = turn_clone)
        .await
        .unwrap();
    let entries = state
        .application
        .test_mutate_history(|history| history.get_entries())
        .await
        .unwrap()
        .expect("history reads back");
    assert_eq!(
        entries.len(),
        1,
        "the refused clear must leave the recorded turn in place"
    );
    assert_eq!(session, state.session_uuid().await);

    drop(operation);

    let cleared = handle_model_clear_history(clear_history_request(), &state).await;
    assert!(
        cleared.error.is_none(),
        "the clear must succeed once the slot is free: {:?}",
        cleared.error
    );
    assert!(
        state
            .application
            .test_mutate_history(|history| history.get_entries())
            .await
            .unwrap()
            .expect("history reads back")
            .is_empty(),
        "the clear must truncate the journal once it owns the slot"
    );
    assert_ne!(
        session,
        state.session_uuid().await,
        "a completed clear rotates the dialogue session"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn step_event_forwarding_streams_edit_lifecycle_events() {
    let (out, mut rx) = crate::transport::OutChannel::new();
    let edit_cycle = crate::handlers::edit_cycle::edit_cycle(out.clone(), "op-step-edit");
    edit_cycle.open();
    publish_stream(
        &out,
        "op-step-edit",
        StreamEvent::EditFileStarted {
            path: "src/main.rs".to_string(),
            op_type: tauqe_core::edits::EditFileOp::Replace,
        },
    );
    publish_stream(
        &out,
        "op-step-edit",
        StreamEvent::EditHunk {
            path: "src/main.rs".to_string(),
            hunk_index: 0,
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 2,
            context_hint: None,
            old_text: "fn main() {}".to_string(),
            new_text: "fn main() {\n    println!();\n}".to_string(),
        },
    );
    publish_stream(
        &out,
        "op-step-edit",
        StreamEvent::EditFileDone {
            path: "src/main.rs".to_string(),
            status: tauqe_core::edits::EditFileStatus::Ok,
            error: None,
            hunks_count: 1,
        },
    );

    let events = published_events(&mut rx);
    let methods: Vec<&str> = events.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(
        methods,
        vec![
            tauqe_protocol::events::EDIT_STARTED,
            tauqe_protocol::events::EDIT_FILE_STARTED,
            tauqe_protocol::events::EDIT_HUNK,
            tauqe_protocol::events::EDIT_FILE_DONE,
        ]
    );
    assert_eq!(events[1].1["path"], "src/main.rs");
    assert_eq!(events[2].1["hunk_index"], 0);
    assert_eq!(events[3].1["hunks_count"], 1);
}

/// Drains the transport queue into the ordered list of published events.
fn published_events(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<(String, serde_json::Value)> {
    let mut events = Vec::new();
    while let Ok(line) = rx.try_recv() {
        let event: serde_json::Value =
            serde_json::from_str(&line).expect("the transport writes JSON lines");
        let method = event
            .get("method")
            .and_then(|method| method.as_str())
            .unwrap_or_default()
            .to_string();
        let params = event.get("params").cloned().unwrap_or_default();
        events.push((method, params));
    }
    events
}

#[tokio::test]
async fn history_clear_preserves_the_rpc_result_and_emits_focus_detach_once() {
    let root = scratch_root("clear-history-focus-event");
    let (out, mut events) = crate::transport::OutChannel::new();
    let (history_sender, _history_rx) = mpsc::unbounded_channel();
    let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);
    state
        .set_discussion_focus(DiscussionFocus::plan("plan-1", vec![]))
        .await;
    while events.try_recv().is_ok() {}
    let response = handle_model_clear_history(clear_history_request(), &state).await;
    assert!(response.error.is_none());
    assert_eq!(response.result, Some(serde_json::json!({"cleared":true})));
    let mut detached = 0;
    while let Ok(line) = events.try_recv() {
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        if event["method"] == tauqe_protocol::events::DISCUSSION_FOCUS_CHANGED {
            assert_eq!(event["params"]["focus"]["kind"], "none");
            detached += 1;
        }
    }
    assert_eq!(detached, 1);
    handle_model_clear_history(clear_history_request(), &state).await;
    assert!(events.try_recv().is_err());
    let _ = std::fs::remove_dir_all(root);
}
