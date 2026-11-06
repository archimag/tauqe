//! Unit tests for the model turn subsystem.
//!
//! The tests pin the deterministic decisions taken before any provider request
//! is launched: which turn intent a typed discussion target establishes and
//! whether a turn binds the interactive dialogue session. Every assertion
//! targets a pure function, so the suite needs no `AppState`, no provider, and
//! no repository.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauqe_core::config::AppConfig;
use tauqe_core::prompt::protocol::PromptProtocol;
use tauqe_core::providers::{ChatMessage, LlmProvider, ResponseFormat, StreamEvent};
use tauqe_protocol::discussion::DiscussionPlanAction;
use tauqe_protocol::{
    DiscussionFocus, DiscussionRequest, DiscussionTarget, EditProtocolKind, LlmCapability,
    LlmCommand, ModelAskParams, ModelRef, ModelSelection, Plan, ProviderKind, Request, TurnIntent,
};
use tokio::sync::{mpsc, watch};

use super::prompt::{build_model_ask_prompt, intent_for_target};
use super::turn::{execute_model_turn, resolve_session_id, resolve_turn_protocol, StepTurnOptions};
use super::{
    handle_model_cancel, handle_model_clear_history, handle_model_select, start_model_turn,
};
use crate::handlers::common::test_support::scratch_root;
use crate::handlers::history::handle_history_get;
use crate::handlers::PreflightVerdict;
use crate::state::AppState;
/// A plan discussion is strictly read-only: the model reasons about the plan
/// and never touches files.
#[test]
fn plan_discussion_is_readonly() {
    let intent = intent_for_target(&DiscussionTarget::Plan {
        id: "plan-1".to_string(),
    });
    assert!(intent.is_plan_discussion());
    assert!(!intent.is_interactive_repl());
}

/// A review finding dialogue establishes its own regime, because the decision
/// the dialogue records is authorized by that regime alone. The developer still
/// holds the full finding context and may ask for the fix directly.
#[test]
fn review_discussion_is_authorized_by_its_regime() {
    let finding = intent_for_target(&DiscussionTarget::Review {
        id: "review-1".to_string(),
    });
    assert!(finding.is_review_discussion());
    assert!(!finding.is_interactive_repl());
    assert!(!finding.is_plan_discussion());

    // A group dialogue shares the regime: the plan of the group is the unit of
    // remediation, and the decision it records belongs to the same anchored
    // session.
    let group = intent_for_target(&DiscussionTarget::ReviewGroup {
        session_id: "review-1".to_string(),
        group_id: "arch-1".to_string(),
    });
    assert!(group.is_review_discussion());
}

/// A decision recorded for one finding through the Structured Output transport.
const REVIEW_DECISION_JSON: &str = r#"{"message":"Recording the decision.","review_update":{"item_id":7,"status":"rejected","note":"The race cannot happen"}}"#;

/// Decodes `raw` and returns the review decisions that survived decoding.
///
/// The helper keeps every assertion focused on the regime authorization instead
/// of on the surrounding command vocabulary, which differs between the two
/// transports.
fn decoded_review_updates(
    protocol: &dyn PromptProtocol,
    raw: &str,
) -> Vec<tauqe_protocol::review::ReviewUpdateProposal> {
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
    let structured = resolve_turn_protocol(
        TurnIntent::ReviewDiscussion,
        EditProtocolKind::Structured,
        None,
    );
    let surface = structured.capability_set();
    assert!(surface.contains(LlmCapability::ReviewUpdate));

    let decisions = decoded_review_updates(structured.as_ref(), REVIEW_DECISION_JSON);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].item_id, 7);
    assert_eq!(
        decisions[0].status,
        tauqe_protocol::review::ReviewDecisionStatus::Rejected
    );
    assert_eq!(decisions[0].note, "The race cannot happen");
}

/// The decision never leaves the regime that authorizes it, and the XML contour
/// never carries it: the field owns no control tag, so it travels exclusively
/// through Structured Output.
#[test]
fn the_recorded_decision_never_leaves_the_review_regime() {
    for intent in [
        TurnIntent::InteractiveRepl,
        TurnIntent::PlanDiscussion,
        TurnIntent::IsolatedStep,
    ] {
        let protocol = resolve_turn_protocol(intent, EditProtocolKind::Structured, None);
        let surface = protocol.capability_set();
        assert!(!surface.contains(LlmCapability::ReviewUpdate));
        let decisions = decoded_review_updates(protocol.as_ref(), REVIEW_DECISION_JSON);
        assert!(decisions.is_empty());
    }

    // The grant is orthogonal to the transport, but the decision owns no control
    // tag: an XML dialogue advertises the capability and decodes nothing from it.
    let xml = resolve_turn_protocol(TurnIntent::ReviewDiscussion, EditProtocolKind::Xml, None);
    let xml_surface = xml.capability_set();
    assert!(xml_surface.contains(LlmCapability::ReviewUpdate));
    let decisions = decoded_review_updates(xml.as_ref(), REVIEW_DECISION_JSON);
    assert!(decisions.is_empty());
}

/// An isolated step never inherits the interactive session, so it cannot reuse
/// a prompt-prefix cache built from the dialogue transcript.
#[test]
fn isolated_step_carries_no_dialogue_session() {
    assert_eq!(
        resolve_session_id(TurnIntent::IsolatedStep, "session-1".to_string()),
        None
    );
}

/// A dialogue turn binds the interactive session, so successive rounds reuse the
/// backend's cached prompt prefix.
#[test]
fn interactive_turn_binds_the_dialogue_session() {
    assert!(resolve_session_id(TurnIntent::InteractiveRepl, "session-1".to_string()).is_some());
    assert!(resolve_session_id(TurnIntent::PlanDiscussion, "session-1".to_string()).is_some());
}

/// Builds a `model/select` request for the given selection.
fn select_request(selection: ModelSelection) -> Request {
    Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: "model/select".to_string(),
        params: Some(
            serde_json::to_value(tauqe_protocol::ModelSelectParams { selection }).unwrap(),
        ),
    }
}

/// Switching the effective model starts a cold dialogue session, because a
/// provider's prefix cache is bound to one concrete model; reselecting the same
/// model keeps the warm session.
#[tokio::test]
async fn model_select_rotates_the_session_confirmed_by_the_effective_model() {
    let mut cfg = AppConfig::default();
    cfg.models.primary = vec![
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

/// Plan mutation emitted through the XML transport: an update of an existing
/// plan and the save of a new one.
const PLAN_UPDATE_XML: &str =
    "<plan action=\"update\" id=\"p-1\"><item id=\"1\" status=\"done\" /></plan>";
const PLAN_SAVE_XML: &str = concat!(
    "<plan action=\"save\" id=\"p-2\" title=\"P2\">",
    "<concept><concept_item id=\"c-1\" title=\"Invariant\" /></concept></plan>"
);

/// The same two mutations emitted through the Structured Output transport.
const PLAN_UPDATE_JSON: &str =
    r#"{"message":"Updating","plan_update":{"action":"update","id":"p-1"}}"#;
const PLAN_SAVE_JSON: &str =
    r#"{"message":"Saving","plan_update":{"action":"save","id":"p-2","title":"P2"}}"#;

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
#[test]
fn plan_discussion_follows_the_configured_transport() {
    let xml = resolve_turn_protocol(TurnIntent::PlanDiscussion, EditProtocolKind::Xml, None);
    assert_eq!(xml.kind(), EditProtocolKind::Xml);
    assert!(xml.capability_set().contains(LlmCapability::FileEdits));
    assert!(xml.capability_set().contains(LlmCapability::PlanUpdate));
    assert_eq!(
        decoded_plan_actions(xml.as_ref(), PLAN_UPDATE_XML),
        vec![DiscussionPlanAction::Update]
    );

    let structured = resolve_turn_protocol(
        TurnIntent::PlanDiscussion,
        EditProtocolKind::Structured,
        None,
    );
    assert_eq!(structured.kind(), EditProtocolKind::Structured);
    assert!(structured
        .capability_set()
        .contains(LlmCapability::FileEdits));
    assert!(structured
        .capability_set()
        .contains(LlmCapability::PlanUpdate));
    assert_eq!(
        decoded_plan_actions(structured.as_ref(), PLAN_UPDATE_JSON),
        vec![DiscussionPlanAction::Update]
    );
}

/// The declared intent alone decides plan mutation: an interactive turn
/// composes the interactive regime profile on both transports, so the mutation
/// is neither advertised nor decoded, while plan authorship stays available,
/// because a user may ask for a plan at any moment of a dialogue.
#[test]
fn interactive_turn_never_carries_plan_mutation() {
    let xml = resolve_turn_protocol(TurnIntent::InteractiveRepl, EditProtocolKind::Xml, None);
    assert!(!xml.capability_set().contains(LlmCapability::PlanUpdate));
    assert!(xml.capability_set().contains(LlmCapability::PlanCreation));
    assert!(decoded_plan_actions(xml.as_ref(), PLAN_UPDATE_XML).is_empty());
    assert_eq!(
        decoded_plan_actions(xml.as_ref(), PLAN_SAVE_XML),
        vec![DiscussionPlanAction::Save]
    );

    let structured = resolve_turn_protocol(
        TurnIntent::InteractiveRepl,
        EditProtocolKind::Structured,
        None,
    );
    assert!(!structured
        .capability_set()
        .contains(LlmCapability::PlanUpdate));
    assert!(decoded_plan_actions(structured.as_ref(), PLAN_UPDATE_JSON).is_empty());
    assert_eq!(
        decoded_plan_actions(structured.as_ref(), PLAN_SAVE_JSON),
        vec![DiscussionPlanAction::Save]
    );
}

/// Asserts the plan permissions of one isolated step.
///
/// The turn-intent narrowing runs after protocol resolution, exactly as the
/// bounded-turn FSM narrows the decoder, so the assertion covers the
/// end-to-end surface of the step.
fn assert_step_has_no_plan_permission(transport: EditProtocolKind) {
    let protocol = resolve_turn_protocol(TurnIntent::IsolatedStep, transport, None);
    assert!(!protocol
        .capability_set()
        .contains(LlmCapability::PlanUpdate));

    let narrowed = protocol.with_capability_set(
        TurnIntent::IsolatedStep.narrow_capabilities(&protocol.capability_set()),
    );
    assert!(!narrowed
        .capability_set()
        .contains(LlmCapability::PlanCreation));
    assert!(!narrowed
        .capability_set()
        .contains(LlmCapability::PlanUpdate));
    for raw in [
        PLAN_UPDATE_XML,
        PLAN_SAVE_XML,
        PLAN_UPDATE_JSON,
        PLAN_SAVE_JSON,
    ] {
        assert!(
            decoded_plan_actions(narrowed.as_ref(), raw).is_empty(),
            "an isolated step must decode no plan command"
        );
    }
}

/// An isolated step keeps the plan immutable: neither the mutation a discussion
/// anchor grants nor plan authorship survives, whatever the transport.
#[test]
fn isolated_step_carries_no_plan_permission() {
    assert_step_has_no_plan_permission(EditProtocolKind::Xml);
    assert_step_has_no_plan_permission(EditProtocolKind::Structured);
}
/// Constructs an [`AppState`] over a scratch repository root, isolating each
/// focus test's stored plans and history from every other test.
fn state_over_root(root: std::path::PathBuf) -> std::sync::Arc<AppState> {
    let (out, _rx) = crate::transport::OutChannel::new();
    let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
    AppState::new(AppConfig::default(), root, history_sender, out)
}

/// While a plan focus is active, a plain prompt continues the anchored
/// discussion: the turn is enriched with the focused plan and its intent
/// follows the anchor rather than the free-form text.
#[tokio::test]
async fn plain_prompt_with_active_focus_continues_the_discussion() {
    let root = scratch_root("focus-active");
    let plan = Plan {
        id: "plan-1".to_string(),
        title: "Focus Plan".to_string(),
        ..Default::default()
    };
    tauqe_core::plan::storage::PlanStorage::save_plan(&root, &plan).unwrap();

    let state = state_over_root(root.clone());
    state
        .set_discussion_focus(DiscussionFocus::plan("plan-1", vec!["1".to_string()]))
        .await;

    let (prompt, intent) = build_model_ask_prompt(
        &state,
        ModelAskParams::Prompt {
            text: "keep refining".to_string(),
        },
    )
    .await
    .expect("the focused plan must load");

    assert!(
        intent.is_plan_discussion(),
        "a plain prompt under plan focus must follow the anchored intent"
    );
    assert!(
        prompt.contains("keep refining"),
        "the plain prompt must reach the discussion prompt"
    );
    assert!(
        state.discussion_focus().await.is_active(),
        "a successful enriched turn must keep the anchor"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// An explicit discussion target that loads replaces the focus anchor with
/// the addressed entity and its focused items, turning one-shot comments into
/// a continuous dialogue.
#[tokio::test]
async fn discussion_target_replaces_the_focus_anchor() {
    let root = scratch_root("focus-replace");
    let plan = Plan {
        id: "plan-1".to_string(),
        title: "Focus Plan".to_string(),
        ..Default::default()
    };
    tauqe_core::plan::storage::PlanStorage::save_plan(&root, &plan).unwrap();

    let state = state_over_root(root.clone());
    state
        .set_discussion_focus(DiscussionFocus::plan("other-plan", Vec::new()))
        .await;

    let (prompt, intent) = build_model_ask_prompt(
        &state,
        ModelAskParams::Discussion(DiscussionRequest {
            target: DiscussionTarget::Plan {
                id: "plan-1".to_string(),
            },
            focused_items: vec!["1".to_string()],
            comment: "refine the authentication flow".to_string(),
        }),
    )
    .await
    .expect("the stored plan must load");

    assert!(intent.is_plan_discussion());
    assert!(
        prompt.contains("refine the authentication flow"),
        "the developer comment must reach the discussion prompt"
    );

    let focus = state.discussion_focus().await;
    assert_eq!(
        focus.target().cloned(),
        Some(DiscussionTarget::Plan {
            id: "plan-1".to_string()
        })
    );
    assert_eq!(focus.focused_items(), ["1".to_string()].as_slice());

    let _ = std::fs::remove_dir_all(&root);
}

/// A plain prompt whose focus anchor references a plan that is gone falls
/// back to the raw prompt and clears the stale anchor, so the dialogue is
/// never routed at an entity that no longer exists.
#[tokio::test]
async fn plain_prompt_with_stale_focus_clears_the_anchor() {
    let root = scratch_root("focus-stale");
    let state = state_over_root(root.clone());

    state
        .set_discussion_focus(DiscussionFocus::plan("gone-plan", Vec::new()))
        .await;

    let (prompt, intent) = build_model_ask_prompt(
        &state,
        ModelAskParams::Prompt {
            text: "just answer here".to_string(),
        },
    )
    .await
    .expect("a stale focus must not fail the plain prompt");

    assert_eq!(prompt, "just answer here");
    assert!(intent.is_interactive_repl());
    assert!(
        state.discussion_focus().await.is_none(),
        "the stale anchor must be cleared instead of kept"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// An explicit discussion target that cannot be loaded fails the turn and
/// leaves the previous focus anchor untouched, because the anchor is replaced
/// only after the referenced entity loads successfully.
#[tokio::test]
async fn missing_discussion_target_keeps_the_previous_focus() {
    let root = scratch_root("focus-missing");
    let state = state_over_root(root.clone());

    let anchor = DiscussionFocus::plan("kept-plan", vec!["1".to_string()]);
    state.set_discussion_focus(anchor.clone()).await;

    let err = build_model_ask_prompt(
        &state,
        ModelAskParams::Discussion(DiscussionRequest {
            target: DiscussionTarget::Plan {
                id: "missing-plan".to_string(),
            },
            focused_items: Vec::new(),
            comment: "where did the plan go?".to_string(),
        }),
    )
    .await
    .expect_err("a target that cannot be loaded must fail the turn");

    assert_eq!(err.code, tauqe_protocol::errors::NOT_FOUND.to_string());
    assert_eq!(
        state.discussion_focus().await,
        anchor,
        "the failed discussion must not replace the previous anchor"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A blank free-form prompt is rejected before any focus handling, so the
/// invalid request never mutates the focus anchor.
#[tokio::test]
async fn blank_prompt_is_rejected_without_touching_the_focus() {
    let root = scratch_root("focus-blank");
    let state = state_over_root(root.clone());

    let anchor = DiscussionFocus::plan("plan-1", Vec::new());
    state.set_discussion_focus(anchor.clone()).await;

    let err = build_model_ask_prompt(
        &state,
        ModelAskParams::Prompt {
            text: "   ".to_string(),
        },
    )
    .await
    .expect_err("a blank prompt must be rejected");

    assert_eq!(err.code, tauqe_protocol::errors::INVALID_PARAMS.to_string());
    assert_eq!(
        state.discussion_focus().await,
        anchor,
        "a rejected prompt must leave the focus anchor untouched"
    );

    let _ = std::fs::remove_dir_all(&root);
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
        "[models]\nprimary = [\"openrouter:a/b\"]\n",
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
    let verdict = preflight_with(Some("[models]\nprimary = [\"openrouter:a/b\"]\n")).await;
    assert!(
        matches!(verdict, PreflightVerdict::Passed),
        "a declaration without verification must pass: {verdict:?}"
    );
}

/// A declaration whose verification command resolves passes the gate untouched.
#[tokio::test]
async fn preflight_accepts_a_valid_tauqe_toml() {
    let verdict = preflight_with(Some(
        "[models]\nprimary = [\"openrouter:a/b\"]\n\n[project.verify]\ncommand = \"true\"\n",
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

    async fn stream_chat(
        &self,
        _model: &str,
        _messages: Vec<ChatMessage>,
        _response_format: Option<ResponseFormat>,
        tx: mpsc::Sender<StreamEvent>,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
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
    *state.provider_override.lock().await = Some(injected);

    start_model_turn(
        &state,
        "Explain the repository".to_string(),
        TurnIntent::InteractiveRepl,
        None,
        None,
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
    cfg.models.primary = vec![
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
    let before = state.config.lock().await.selection.clone();

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
        state.config.lock().await.selection,
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
        .mutate_history(|history| history.record_turn("Keep me", Vec::new()).map(|_| ()))
        .await
        .expect("the record runs on the blocking pool")
        .expect("the record must persist");

    // A bounded turn works on a private clone of the manager and adopts it back
    // when it ends, so this clone is exactly what a running turn holds.
    let turn_clone = state.history.lock().await.clone();
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
    state.adopt_history(turn_clone).await;
    let entries = state
        .history
        .lock()
        .await
        .get_entries()
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
            .history
            .lock()
            .await
            .get_entries()
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

/// A decision recorded in a review dialogue is durable review state, so the turn
/// publishes it as it completes instead of leaving the client to refetch the
/// session. The publication is scoped to the findings the turn actually moved.
#[tokio::test]
async fn a_recorded_review_decision_is_published_as_review_updated() {
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::workflow::DirectApplyPolicy;
    use tauqe_protocol::review::{
        IssueStatus, IssueType, ReviewItem, ReviewSession, ReviewSeverity,
    };

    let root = scratch_root("review-decision-sync");
    tauqe_core::git::init_repository(Some(&root), true).expect("initialise the scratch repository");
    std::fs::write(root.join("README.md"), "# scratch\n").expect("write the repository fixture");
    tauqe_core::git::cmd::stage_files(&root, &["README.md".to_string()])
        .expect("stage the fixture");
    tauqe_core::git::cmd::commit_with_developer_author(&root, "Add fixture", false)
        .expect("commit the fixture");

    let session = ReviewSession {
        id: "review-sync".to_string(),
        title: "Decision Audit".to_string(),
        created_at: 1,
        items: vec![ReviewItem {
            id: 7,
            title: "Held guard".to_string(),
            uuid: None,
            commit: None,
            group: None,
            model: None,
            issue_type: IssueType::Bug,
            severity: ReviewSeverity::Critical,
            status: IssueStatus::Discussion,
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            resolution_notes: None,
            body: "The guard is released after the await".to_string(),
        }],
        ..ReviewSession::default()
    };
    tauqe_core::review::ReviewStorage::save_session(&root, &session)
        .expect("save the review session");

    let mut cfg = AppConfig::default();
    cfg.repl.protocol = EditProtocolKind::Structured;
    let (out, mut rx) = crate::transport::OutChannel::new();
    let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(cfg, root.clone(), history_sender, out.clone());
    let script = [MockResponse::text(REVIEW_DECISION_JSON)];
    let provider = Arc::new(MockLlmProvider::scripted(script));
    let injected: Arc<dyn LlmProvider> = provider.clone();
    *state.provider_override.lock().await = Some(injected);
    state
        .set_discussion_focus(DiscussionFocus::review("review-sync", Vec::new()))
        .await;

    let result = execute_model_turn(
        &state,
        "Record the decision we reached".to_string(),
        Box::new(DirectApplyPolicy::new()),
        StepTurnOptions {
            intent: TurnIntent::ReviewDiscussion,
            custom_selection: None,
            plan_context: None,
            work_context: None,
            reasoning_effort: None,
            marker: None,
            session_id: None,
            shared_operation: None,
            phase_out: None,
        },
    )
    .await
    .expect("the review dialogue runs");
    assert!(
        result.error.is_none(),
        "the dialogue failed: {:?}",
        result.error
    );
    assert_eq!(
        provider.remaining(),
        0,
        "the turn decodes a single provider round"
    );

    let stored = tauqe_core::review::ReviewStorage::load_review(&root, "review-sync")
        .expect("load the session")
        .expect("the session is stored");
    assert_eq!(
        stored.find_item(7).expect("the finding is stored").status,
        IssueStatus::Rejected
    );

    let mut published = Vec::new();
    while let Ok(line) = rx.try_recv() {
        let event: serde_json::Value =
            serde_json::from_str(&line).expect("the transport writes JSON lines");
        if event.get("method").and_then(|method| method.as_str())
            == Some(tauqe_protocol::events::REVIEW_UPDATED)
        {
            published.push(event);
        }
    }
    let event = published
        .first()
        .expect("the recorded decision is published to the client");
    assert_eq!(
        published.len(),
        1,
        "one publication is emitted per affected finding"
    );
    let session: ReviewSession = serde_json::from_value(event["params"]["session"].clone())
        .expect("the publication carries the refreshed session");
    assert_eq!(session.id, "review-sync");
    let finding = session
        .find_item(7)
        .expect("the decided finding is published");
    assert_eq!(finding.status, IssueStatus::Rejected);
    assert_eq!(
        finding.resolution_notes.as_deref(),
        Some("The race cannot happen")
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Provider that relays a cancellation and then parks the workflow task inside a
/// synchronous stretch, so the task never reaches the suspension point an abort
/// lands at.
///
/// It reproduces the last-resort hazard: a cancelled turn whose workflow sits
/// between abort landing points outlives both unwind graces while it still
/// holds the session lock it acquired.
struct AbortIgnoringProvider {
    /// Signals that the workflow task entered the synchronous stretch.
    entered: mpsc::UnboundedSender<()>,
    /// Set by the test to let the workflow task reach a suspension point.
    released: Arc<AtomicBool>,
    /// Set once the synchronous stretch has ended.
    exited: Arc<AtomicBool>,
}

impl AbortIgnoringProvider {
    /// Builds the provider together with the receiver that observes its stall.
    fn new() -> (Self, mpsc::UnboundedReceiver<()>) {
        let (entered, receiver) = mpsc::unbounded_channel();
        (
            Self {
                entered,
                released: Arc::new(AtomicBool::new(false)),
                exited: Arc::new(AtomicBool::new(false)),
            },
            receiver,
        )
    }
}

#[async_trait::async_trait]
impl LlmProvider for AbortIgnoringProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenRouter
    }

    async fn stream_chat(
        &self,
        _model: &str,
        _messages: Vec<ChatMessage>,
        _response_format: Option<ResponseFormat>,
        tx: mpsc::Sender<StreamEvent>,
        _cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        // Relay the cancellation the turn reacts to, then hold the task inside a
        // synchronous stretch. An abort only lands at a suspension point, so the
        // task keeps running and keeps holding the session lock. The cancel
        // signal is deliberately never observed here: a workflow that ignores it
        // is exactly what the deferred reaping has to survive.
        let _ = tx.send(StreamEvent::Cancelled).await;
        let _ = self.entered.send(());
        while !self.released.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.exited.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// A workflow task that survives the abort must never be detached: the turn hands
/// the still-owned handle to the shared watchdog, which keeps the operation slot
/// registered until the task has truly exited, so the zombie releases the session
/// lock before any new operation can register.
///
/// The regression runs on a multi-thread runtime because the stand-in parks the
/// workflow task inside a synchronous stretch: that stretch never yields, which
/// is exactly why the abort cannot land there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aborted_workflow_keeps_the_slot_until_the_watchdog_reaps_it() {
    use tauqe_core::workflow::DirectApplyPolicy;

    let root = scratch_root("deferred-reap");
    tauqe_core::git::init_repository(Some(&root), true).expect("initialise the scratch repository");
    std::fs::write(root.join("README.md"), "# scratch\n").expect("write the repository fixture");
    tauqe_core::git::cmd::stage_files(&root, &["README.md".to_string()])
        .expect("stage the fixture");
    tauqe_core::git::cmd::commit_with_developer_author(&root, "Add fixture", false)
        .expect("commit the fixture");

    let state = state_over_root(root.clone());
    let (provider, mut entered) = AbortIgnoringProvider::new();
    let released = Arc::clone(&provider.released);
    let exited = Arc::clone(&provider.exited);
    let injected: Arc<dyn LlmProvider> = Arc::new(provider);
    *state.provider_override.lock().await = Some(injected);

    // The production grace exists to give a real workflow time to unwind. Short
    // graces confined to this turn's task make the second expiry reachable
    // without changing what any concurrently running test observes.
    let grace = Duration::from_millis(150);
    let turn_state = Arc::clone(&state);
    let turn = tokio::spawn(async move {
        super::turn::with_unwind_grace(grace, async move {
            execute_model_turn(
                &turn_state,
                "Explain the repository".to_string(),
                Box::new(DirectApplyPolicy::new()),
                StepTurnOptions {
                    intent: TurnIntent::InteractiveRepl,
                    custom_selection: None,
                    plan_context: None,
                    work_context: None,
                    reasoning_effort: None,
                    marker: None,
                    session_id: None,
                    shared_operation: None,
                    phase_out: None,
                },
            )
            .await
        })
        .await
    });

    tokio::time::timeout(Duration::from_secs(10), entered.recv())
        .await
        .expect("the turn must reach the provider")
        .expect("the provider signals that the workflow task is parked");
    state.hard_cancel_active_operation();

    // Both graces expire while the workflow task sits between abort landing
    // points, so the turn has handed the handle to the watchdog by now.
    tokio::time::sleep(grace * 5).await;
    assert!(
        !exited.load(Ordering::SeqCst),
        "the workflow task must still be inside the synchronous stretch"
    );

    let result = tokio::time::timeout(Duration::from_secs(10), turn)
        .await
        .expect("the turn must return once the reaping is deferred")
        .expect("the turn task joins")
        .expect("the turn returns a cancelled result");
    assert!(result.is_cancelled, "the turn must report the cancellation");
    assert!(
        state.is_operation_active(),
        "the slot must stay registered while the aborted task still runs"
    );
    assert!(
        state.try_begin_operation().is_err(),
        "a new operation must stay refused until the aborted task has exited"
    );
    assert!(
        !exited.load(Ordering::SeqCst),
        "the deferred task must still hold the session lock"
    );

    // Letting the stretch end gives the pending abort a landing point; the
    // watchdog then reaps the task and drops the registration with it.
    released.store(true, Ordering::SeqCst);
    let mut freed = false;
    for _ in 0..500 {
        if !state.is_operation_active() {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        freed,
        "the watchdog must release the slot once the task exits"
    );
    assert!(
        exited.load(Ordering::SeqCst),
        "the workflow task must have reached its own end"
    );
    assert!(
        state.try_begin_operation().is_ok(),
        "the slot must be reusable afterwards"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Provider that streams nothing, ignores the cancellation signal, and never
/// returns on its own, so the only way the turn it serves can end is the
/// last-resort reaping of a wedged workflow task.
struct SilentlyParkedProvider {
    /// Signals that the provider is parked inside its stream.
    entered: mpsc::UnboundedSender<()>,
}

impl SilentlyParkedProvider {
    /// Builds the provider together with the receiver that observes its stall.
    fn new() -> (Self, mpsc::UnboundedReceiver<()>) {
        let (entered, receiver) = mpsc::unbounded_channel();
        (Self { entered }, receiver)
    }
}

#[async_trait::async_trait]
impl LlmProvider for SilentlyParkedProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenRouter
    }

    async fn stream_chat(
        &self,
        _model: &str,
        _messages: Vec<ChatMessage>,
        _response_format: Option<ResponseFormat>,
        _tx: mpsc::Sender<StreamEvent>,
        _cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        // Nothing is relayed and the cancellation is never observed, so the
        // workflow stays parked until the watchdog aborts it: exactly the turn
        // that used to surface as a provider failure.
        let _ = self.entered.send(());
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// A hard-cancelled turn whose workflow ignored the signal is reported as a
/// cancellation, not as a provider failure.
///
/// The force-abort watchdog drops the wedged workflow task, so the turn's relay
/// loop ends without a `Cancelled` event and the join yields an error. The
/// raised cancel signal stays the authoritative classification — the rule every
/// other seam applies — so a client that asked for the cancellation never
/// receives a spurious `model/error` one watchdog cycle later, and a direct
/// caller observes `is_cancelled` instead of a failure.
#[tokio::test]
async fn a_force_aborted_turn_is_reported_as_cancelled() {
    use tauqe_core::workflow::DirectApplyPolicy;

    let root = scratch_root("force-aborted-turn");
    tauqe_core::git::init_repository(Some(&root), true).expect("initialise the scratch repository");
    std::fs::write(root.join("README.md"), "# scratch\n").expect("write the repository fixture");
    tauqe_core::git::cmd::stage_files(&root, &["README.md".to_string()])
        .expect("stage the fixture");
    tauqe_core::git::cmd::commit_with_developer_author(&root, "Add fixture", false)
        .expect("commit the fixture");

    let (out, mut rx) = crate::transport::OutChannel::new();
    let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);
    let (provider, mut entered) = SilentlyParkedProvider::new();
    let injected: Arc<dyn LlmProvider> = Arc::new(provider);
    *state.provider_override.lock().await = Some(injected);

    let turn_state = Arc::clone(&state);
    let turn = tokio::spawn(async move {
        execute_model_turn(
            &turn_state,
            "Explain the repository".to_string(),
            Box::new(DirectApplyPolicy::new()),
            StepTurnOptions {
                intent: TurnIntent::InteractiveRepl,
                custom_selection: None,
                plan_context: None,
                work_context: None,
                reasoning_effort: None,
                marker: None,
                session_id: None,
                shared_operation: None,
                phase_out: None,
            },
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(10), entered.recv())
        .await
        .expect("the turn must reach the provider")
        .expect("the provider signals that it is parked");
    state.cancel_active_operation();
    state
        .force_abort_active_operation(Duration::from_secs(10))
        .await;

    let result = tokio::time::timeout(Duration::from_secs(10), turn)
        .await
        .expect("the aborted turn must return")
        .expect("the turn task joins")
        .expect("the turn reports the cancellation instead of a start failure");
    assert!(
        result.is_cancelled,
        "the raised cancel signal must classify the turn as cancelled"
    );
    assert_eq!(
        result.error.as_deref(),
        Some("Operation cancelled"),
        "a cancelled turn carries the same payload as a cooperative cancellation"
    );
    assert!(
        result.result.is_none(),
        "an aborted turn produces no result"
    );

    let mut published = Vec::new();
    while let Ok(line) = rx.try_recv() {
        let event: serde_json::Value =
            serde_json::from_str(&line).expect("the transport writes JSON lines");
        if let Some(method) = event.get("method").and_then(|method| method.as_str()) {
            published.push(method.to_string());
        }
    }
    assert!(
        published
            .iter()
            .any(|method| method == tauqe_protocol::events::MODEL_CANCELLED),
        "a cancelled turn must report model/cancelled: {published:?}"
    );
    assert!(
        !published
            .iter()
            .any(|method| method == tauqe_protocol::events::MODEL_ERROR),
        "a cancelled turn must never report model/error: {published:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A committed turn applies the change set it proposed and releases the
/// transient context that served the decision.
///
/// The Auto layer is part of the stable prompt prefix exactly like a repository
/// file, so the material discovery gathered for one decision must not survive
/// the commit that made the decision durable: keeping it resident would charge
/// every later request for a prompt prefix that no longer needs it. The
/// dialogue session rotates with the prefix the release shrank, so the next
/// round is never routed to a cache the dialogue no longer sends.
#[tokio::test]
async fn a_committed_turn_applies_its_change_set_and_releases_the_transient_context() {
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::workflow::DirectApplyPolicy;
    use tauqe_protocol::ContextAccess;

    // The change set travels through the XML transport as the canonical
    // unmarked search/replace grammar the decoder accepts.
    const CHANGE_SET: &str = concat!(
        "Rewriting the value.\n",
        "<edits summary=\"Bump the value\">\n",
        "  <edit path=\"target.rs\">\n",
        "    <search>\n",
        "pub fn value() -> i32 { 0 }\n",
        "    </search>\n",
        "    <replace>\n",
        "pub fn value() -> i32 { 42 }\n",
        "    </replace>\n",
        "  </edit>\n",
        "</edits>\n",
    );

    let root = scratch_root("turn-committed-release");
    tauqe_core::git::init_repository(Some(&root), true).expect("initialise the scratch repository");
    std::fs::write(root.join("target.rs"), "pub fn value() -> i32 { 0 }\n")
        .expect("write the edit target");
    std::fs::write(root.join("discovered.rs"), "pub fn discovered() {}\n")
        .expect("write the discovered file");
    let fixtures = vec!["target.rs".to_string(), "discovered.rs".to_string()];
    tauqe_core::git::cmd::stage_files(&root, &fixtures).expect("stage the fixtures");
    tauqe_core::git::cmd::commit_with_developer_author(&root, "Add fixtures", false)
        .expect("commit the fixtures");

    let mut cfg = AppConfig::default();
    cfg.repl.protocol = EditProtocolKind::Xml;
    let (out, _rx) = crate::transport::OutChannel::new();
    let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(cfg, root.clone(), history_sender, out);
    state
        .context
        .write()
        .await
        .add_file("target.rs", ContextAccess::Editable)
        .expect("the edit target is editable");
    state
        .context
        .write()
        .await
        .add_auto_file("discovered.rs", ContextAccess::ReadOnly)
        .expect("the discovered file joins the transient context");
    let script = [
        MockResponse::text(CHANGE_SET),
        MockResponse::text("<success commit_message=\"Bump the value\" />"),
    ];
    let provider = Arc::new(MockLlmProvider::scripted(script));
    let injected: Arc<dyn LlmProvider> = provider.clone();
    *state.provider_override.lock().await = Some(injected);
    let warm_session = state.session_uuid().await;

    let result = execute_model_turn(
        &state,
        "Bump the value to 42".to_string(),
        Box::new(DirectApplyPolicy::new()),
        StepTurnOptions {
            intent: TurnIntent::InteractiveRepl,
            custom_selection: None,
            plan_context: None,
            work_context: None,
            reasoning_effort: None,
            marker: None,
            session_id: None,
            shared_operation: None,
            phase_out: None,
        },
    )
    .await
    .expect("the bounded turn runs");

    assert!(
        result.is_success(),
        "the released turn must apply its change set: {:?}",
        result.result
    );
    assert_eq!(
        std::fs::read_to_string(root.join("target.rs")).expect("read the edit target"),
        "pub fn value() -> i32 { 42 }\n",
        "the change set must reach the working tree"
    );
    assert!(
        state.context.read().await.auto_files().is_empty(),
        "a committed turn must release the transient context that served it"
    );
    assert!(
        !state
            .context
            .read()
            .await
            .contains("discovered.rs")
            .expect("the context resolves the discovered path"),
        "the discovered file leaves the context with the decision"
    );
    assert!(
        state
            .context
            .read()
            .await
            .contains("target.rs")
            .expect("the context resolves the edit target"),
        "the explicitly added file keeps its own entry"
    );
    assert_ne!(
        warm_session,
        state.session_uuid().await,
        "a released prefix starts a fresh dialogue session"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Flips a flag when the owning task future is dropped, so a test can prove that
/// a watchdog released the registration of a task that still ran and that the
/// orphan died once its non-yielding stretch ended.
struct WatchdogExitMarker(Arc<AtomicBool>);

impl Drop for WatchdogExitMarker {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A watchdog reap is bounded in both directions: it waits for the task it
/// aborted, but never longer than the shared task-exit deadline, so a workflow
/// wedged in a non-yielding stretch cannot pin the operation slot until process
/// exit. Releasing the slot leaves the abort delivered, so the orphan dies at the
/// next suspension point its stretch finally reaches.
#[test]
fn a_watchdog_reap_that_expires_releases_the_slot_of_a_pinned_task() {
    let root = scratch_root("watchdog-reap-expiry");

    // The wedged workflow owns a runtime of its own: its whole point is a stretch
    // that never yields back to its scheduler, so it must not be able to pin the
    // scheduler the confirmation runs on.
    let wedged_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("the wedged runtime starts");
    let confirmation_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the confirmation runtime starts");

    let stretch_ends = Arc::new(AtomicBool::new(false));
    let exited = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();

    confirmation_runtime.block_on(async {
        let state = state_over_root(root.clone());
        let guard = Arc::new(state.try_begin_operation().expect("the slot is free"));
        let task = {
            let stretch_ends = Arc::clone(&stretch_ends);
            let exited = Arc::clone(&exited);
            wedged_runtime.spawn(async move {
                let _marker = WatchdogExitMarker(Arc::clone(&exited));
                let _ = entered_tx.send(());
                // A non-yielding stretch: the task reaches no suspension point,
                // so the abort cannot land while it spins.
                while !stretch_ends.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                std::future::pending::<()>().await
            })
        };
        entered_rx
            .await
            .expect("the wedged task must enter its non-yielding stretch");

        // The deadline is resolved where the watchdog is spawned and applied by
        // the confirmation inside it, so a compressed value keeps the expiry path
        // deterministic.
        crate::reap::with_task_exit_deadline(Duration::from_millis(50), async {
            state.defer_abort_owning_operation(guard, task);
        })
        .await;

        // The confirmation expires while the task is still pinned: the slot is
        // released within the bound although the task provably never exited.
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
            "the expired confirmation must release the operation slot"
        );
        assert!(
            !exited.load(Ordering::SeqCst),
            "the wedged task provably never exited before the slot was released"
        );

        // The released slot admits the next operation instead of refusing every
        // request until the process exits.
        assert!(
            state.try_begin_operation().is_ok(),
            "the released slot must admit a new operation"
        );

        // The orphan keeps its abort delivered: it dies at the first suspension
        // point its stretch reaches.
        stretch_ends.store(true, Ordering::SeqCst);
        let stopped = tokio::time::timeout(Duration::from_secs(10), async {
            while !exited.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            stopped.is_ok(),
            "the orphaned task must die once its non-yielding stretch ends"
        );
    });

    let _ = std::fs::remove_dir_all(&root);
}
