//! Unit tests for the model turn subsystem.
//!
//! The tests pin the deterministic decisions taken before any provider request
//! is launched: which turn intent a typed discussion target establishes and
//! whether a turn binds the interactive dialogue session. Every assertion
//! targets a pure function, so the suite needs no `AppState`, no provider, and
//! no repository.

use tauqe_core::config::AppConfig;
use tauqe_core::prompt::protocol::PromptProtocol;
use tauqe_protocol::discussion::DiscussionPlanAction;
use tauqe_protocol::{
    DiscussionTarget, EditProtocolKind, LlmCapability, LlmCommand, ModelRef, ModelSelection,
    Request, TurnIntent,
};

use super::handle_model_select;
use super::prompt::intent_for_target;
use super::turn::{resolve_session_id, resolve_turn_protocol};
use crate::state::AppState;

/// A plan discussion is strictly read-only: the model reasons about the plan
/// and never touches files.
#[test]
fn plan_discussion_is_readonly() {
    let intent = intent_for_target(&DiscussionTarget::Plan {
        id: "plan-1".to_string(),
    });
    assert!(intent.is_readonly_discussion());
    assert!(!intent.is_interactive_repl());
}

/// A review finding dialogue stays interactive, because the developer may ask
/// the model to apply the fix directly.
#[test]
fn review_discussion_stays_interactive() {
    let intent = intent_for_target(&DiscussionTarget::Review {
        id: "review-1".to_string(),
    });
    assert!(intent.is_interactive_repl());
    assert!(!intent.is_readonly_discussion());
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
    assert!(resolve_session_id(TurnIntent::ReadonlyDiscussion, "session-1".to_string()).is_some());
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
    let state = AppState::new(cfg, std::env::temp_dir(), history_sender, out);

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

/// A dialogue anchored on a plan may modify it: the focused interactive turn
/// advertises and decodes the mutation in both encodings.
#[test]
fn plan_focus_grants_plan_update_to_an_interactive_turn() {
    let xml = resolve_turn_protocol(
        TurnIntent::InteractiveRepl,
        EditProtocolKind::Xml,
        true,
        None,
    );
    assert!(xml.capability_set().contains(LlmCapability::PlanUpdate));
    assert_eq!(
        decoded_plan_actions(xml.as_ref(), PLAN_UPDATE_XML),
        vec![DiscussionPlanAction::Update]
    );

    let structured = resolve_turn_protocol(
        TurnIntent::InteractiveRepl,
        EditProtocolKind::Structured,
        true,
        None,
    );
    assert!(structured
        .capability_set()
        .contains(LlmCapability::PlanUpdate));
    assert_eq!(
        decoded_plan_actions(structured.as_ref(), PLAN_UPDATE_JSON),
        vec![DiscussionPlanAction::Update]
    );
}

/// Without a plan focus the mutation is filtered out, while plan authorship
/// stays available, because a user may ask for a plan at any moment of a
/// dialogue.
#[test]
fn interactive_turn_without_plan_focus_filters_plan_update() {
    let xml = resolve_turn_protocol(
        TurnIntent::InteractiveRepl,
        EditProtocolKind::Xml,
        false,
        None,
    );
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
        false,
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

/// Asserts the plan permissions of one isolated step under one focus state.
///
/// The focus is applied to the resolved protocol and the turn-intent narrowing
/// runs afterwards, exactly as the bounded-turn FSM narrows the decoder, so the
/// assertion covers the end-to-end surface of the step.
fn assert_step_has_no_plan_permission(develop: EditProtocolKind, plan_focused: bool) {
    let protocol = resolve_turn_protocol(TurnIntent::IsolatedStep, develop, plan_focused, None);
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

/// An isolated step keeps the plan immutable: neither the mutation granted by a
/// focus nor plan authorship survives, whatever the focus state and transport.
#[test]
fn isolated_step_carries_no_plan_permission_under_any_focus() {
    assert_step_has_no_plan_permission(EditProtocolKind::Xml, false);
    assert_step_has_no_plan_permission(EditProtocolKind::Xml, true);
    assert_step_has_no_plan_permission(EditProtocolKind::Structured, false);
    assert_step_has_no_plan_permission(EditProtocolKind::Structured, true);
}
