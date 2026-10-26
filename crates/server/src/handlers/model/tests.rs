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
    DiscussionFocus, DiscussionRequest, DiscussionTarget, EditProtocolKind, LlmCapability,
    LlmCommand, ModelAskParams, ModelRef, ModelSelection, Plan, Request, TurnIntent,
};

use super::handle_model_select;
use super::prompt::{build_model_ask_prompt, intent_for_target};
use super::turn::{resolve_session_id, resolve_turn_protocol};
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

/// A review finding dialogue stays interactive, because the developer may ask
/// the model to apply the fix directly.
#[test]
fn review_discussion_stays_interactive() {
    let intent = intent_for_target(&DiscussionTarget::Review {
        id: "review-1".to_string(),
    });
    assert!(intent.is_interactive_repl());
    assert!(!intent.is_plan_discussion());
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

/// A plan discussion is a dialogue like any other: the configured transport
/// encodes it, and the transport never shrinks its surface. Both encodings
/// therefore advertise the edit capability and decode the very same plan
/// mutation, which the discussion authorizes by its regime.
#[test]
fn plan_discussion_follows_the_configured_transport() {
    let xml = resolve_turn_protocol(
        TurnIntent::PlanDiscussion,
        EditProtocolKind::Xml,
        false,
        None,
    );
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
        false,
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

/// Creates a unique scratch repository root for a pre-flight integrity test.
fn scratch_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tauqe-preflight-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create the scratch repository root");
    dir
}

/// Runs the deterministic pre-flight gate over a scratch repository that
/// declares `config`, then removes it.
async fn preflight_with(config: Option<&str>) -> Result<(), String> {
    let dir = scratch_root("integrity");
    if let Some(config) = config {
        std::fs::write(dir.join("tauqe.toml"), config).expect("write the project declaration");
    }
    let verdict = crate::handlers::run_preflight_verification(&dir).await;
    let _ = std::fs::remove_dir_all(&dir);
    verdict
}

/// The pre-flight gate refuses a `tauqe.toml` that is not valid TOML, so no turn
/// starts against a configuration the engine cannot even parse.
#[tokio::test]
async fn preflight_rejects_malformed_tauqe_toml() {
    let err = preflight_with(Some("[models\nprimary = \"broken\"\n"))
        .await
        .expect_err("malformed TOML must be refused before any model call");
    assert!(err.contains("tauqe.toml"), "got: {err}");
}

/// The gate refuses a declaration that parses as TOML but violates a semantic
/// invariant, here the absence of the mandatory verification lifecycle.
#[tokio::test]
async fn preflight_rejects_semantically_invalid_tauqe_toml() {
    let err = preflight_with(Some("[models]\nprimary = [\"openrouter:a/b\"]\n"))
        .await
        .expect_err("a configuration without verification must be refused");
    assert!(err.contains("verify"), "got: {err}");
}

/// A declaration whose verification command resolves passes the gate untouched.
#[tokio::test]
async fn preflight_accepts_a_valid_tauqe_toml() {
    let verdict = preflight_with(Some(
        "[models]\nprimary = [\"openrouter:a/b\"]\n\n[project.verify]\ncommand = \"true\"\n",
    ))
    .await;
    assert!(verdict.is_ok(), "got: {:?}", verdict.err());
}

/// A repository without a declaration is governed by built-in defaults, so the
/// gate has nothing to reject.
#[tokio::test]
async fn preflight_accepts_a_repository_without_a_declaration() {
    let verdict = preflight_with(None).await;
    assert!(verdict.is_ok(), "got: {:?}", verdict.err());
}
