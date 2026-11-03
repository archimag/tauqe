//! Server-side bridge between the Architect synthesis contour and RPC telemetry.
//!
//! The contour (`tauqe_core::workflow::fsm::structured`) is provider- and
//! transport-agnostic: it reports every round through `StreamEvent` and resolves
//! a client for another provider through an injected `ProviderResolver`. This
//! module adapts both seams to the daemon:
//!
//! - [`ArchitectProviderResolver`] implements the hook on top of the shared
//!   architect resolution path, so a promoted rung served by another provider
//!   receives a client built from the project configuration.
//! - [`relay_contour_events`] translates the contour stream into the wire events
//!   a client already renders, accumulates the answer the adapter publishes as
//!   `model/finished`, and closes the operation's usage window.
//! - [`run_contour_synthesis`] is the launch skeleton every contour consumer
//!   shares: provider resolution, the usage window, the contoured task with its
//!   abort handle, the telemetry relay, and the context merge back.
//!
//! Plan horizon synthesis now consumes the contour through
//! [`run_contour_synthesis`], while review group decomposition arrives in plan
//! step 6.2: the module re-export therefore keeps a tolerant import allowance
//! until then (the same precedent as `run_workflow_verification` in
//! `crates/core`).

use std::sync::Arc;

use async_trait::async_trait;
use tauqe_core::config::{AppConfig, ModelsConfig};
use tauqe_core::context::ContextManager;
use tauqe_core::providers::{LlmProvider, StreamEvent};
use tauqe_core::workflow::fsm::structured::{
    run_structured_refine, ProviderResolver, StructuredRefineConfig, StructuredRefineOutcome,
};
use tauqe_core::workflow::WorkflowOptions;
use tauqe_protocol::{
    events, ArchitectTier, ModelConnectionRetryingEvent, ModelDeltaEvent, ModelErrorEvent,
    ModelEscalatedEvent, ProviderKind, RepoMapInfoEvent, TurnPhaseEvent,
};
use tokio::sync::mpsc;

use crate::handlers::common::{emit, withdraw_streamed_attempt};
use crate::state::{AppState, OperationGuard};
use crate::usage::UsageGuard;

use super::resolve_architect_provider;

/// Capacity of the contour event channel.
///
/// The contour forwards protocol-scrubbed events at token granularity, so the
/// relay must keep up without ever making a proposal round wait on telemetry.
const CONTOUR_EVENT_CAPACITY: usize = 256;

/// Provider resolver the Architect contour consults when it escalates.
///
/// A provider client is bound to one provider kind, so the contour asks the
/// adapter for a freshly resolved client whenever a promoted rung is served by
/// another provider. The resolver snapshots the configuration the synthesis
/// started on and delegates to the shared architect resolution path, which also
/// serves the provider seam of the handler test suite.
pub(crate) struct ArchitectProviderResolver {
    state: Arc<AppState>,
    config: AppConfig,
}

impl ArchitectProviderResolver {
    /// Binds the resolver to the state and configuration of one synthesis.
    pub(crate) fn new(state: Arc<AppState>, config: AppConfig) -> Self {
        Self { state, config }
    }
}

#[async_trait]
impl ProviderResolver for ArchitectProviderResolver {
    async fn resolve(&self, kind: ProviderKind) -> anyhow::Result<Arc<dyn LlmProvider>> {
        resolve_architect_provider(&self.state, &self.config, kind)
            .await
            .map_err(anyhow::Error::msg)
    }
}

/// Telemetry the relay accumulated for the adapter that started the synthesis.
///
/// The contour owns the provider round, the tier ladder, and the research
/// rounds; the adapter owns the RPC response. This summary is the seam between
/// them: the answer a client needs for `model/finished`, whether the run was
/// cancelled, and the provider failure it reported, if any.
#[derive(Debug, Clone, Default)]
pub(crate) struct ContourRelaySummary {
    /// Every fragment of text the contour streamed, with voided attempts
    /// dropped, ready for the `model/finished` event of the turn.
    pub(crate) assistant_text: String,
    /// Whether the contour reported a cancellation.
    pub(crate) cancelled: bool,
    /// Provider failure the contour reported, if any.
    pub(crate) error: Option<String>,
}

/// Relays the contour event stream into the telemetry of the running operation.
///
/// The contour speaks `StreamEvent`, because it is a domain engine that knows
/// nothing about the wire; the client speaks protocol events. The relay is the
/// single place that translates one into the other, accumulates the answer the
/// adapter publishes as `model/finished`, and withdraws the fragments of an
/// attempt the transport voided.
///
async fn relay_contour_events(
    state: Arc<AppState>,
    op_id: String,
    usage: UsageGuard,
    mut events_rx: mpsc::Receiver<StreamEvent>,
) -> ContourRelaySummary {
    let mut summary = ContourRelaySummary::default();
    while let Some(event) = events_rx.recv().await {
        match event {
            StreamEvent::TextDelta(delta) => {
                summary.assistant_text.push_str(&delta);
                emit(
                    &state.out,
                    events::MODEL_TEXT_DELTA,
                    serde_json::to_value(ModelDeltaEvent {
                        operation_id: op_id.clone(),
                        delta,
                    })
                    .ok(),
                );
            }
            StreamEvent::ReasoningDelta(delta) => {
                emit(
                    &state.out,
                    events::MODEL_REASONING_DELTA,
                    Some(serde_json::json!({
                        "operation_id": op_id.as_str(),
                        "delta": delta,
                    })),
                );
            }
            StreamEvent::TierEscalated {
                tier,
                model,
                role,
                effort,
            } => {
                emit(
                    &state.out,
                    events::MODEL_ESCALATED,
                    serde_json::to_value(ModelEscalatedEvent {
                        operation_id: op_id.clone(),
                        model,
                        role: Some(role),
                        alias: None,
                        effort,
                        tier,
                    })
                    .ok(),
                );
            }
            StreamEvent::TurnPhase {
                phase,
                round,
                max_rounds,
                detail,
            } => {
                emit(
                    &state.out,
                    events::TURN_PHASE,
                    serde_json::to_value(TurnPhaseEvent {
                        operation_id: op_id.clone(),
                        phase,
                        round,
                        max_rounds,
                        detail,
                    })
                    .ok(),
                );
            }
            StreamEvent::RepoMapTokens(tokens) => {
                emit(
                    &state.out,
                    events::TURN_REPOMAP,
                    serde_json::to_value(RepoMapInfoEvent {
                        operation_id: op_id.clone(),
                        tokens,
                    })
                    .ok(),
                );
            }
            StreamEvent::RetryingConnection {
                attempt,
                max_attempts,
                delay_secs,
                reason,
            } => {
                emit(
                    &state.out,
                    events::MODEL_CONNECTION_RETRYING,
                    serde_json::to_value(ModelConnectionRetryingEvent {
                        operation_id: op_id.clone(),
                        attempt,
                        max_attempts,
                        delay_secs,
                        reason,
                    })
                    .ok(),
                );
            }
            // The retry decorator withdrew an attempt that broke mid-stream, so
            // the fragments it already streamed belong to an answer nobody will
            // keep: the accumulator drops them together with the client copy.
            StreamEvent::ResetStream => {
                withdraw_streamed_attempt(&state.out, &op_id, || {
                    summary.assistant_text.clear();
                });
            }
            StreamEvent::Usage(reported) => usage.observe(&reported).await,
            StreamEvent::ContextChanged(ctx_state) => {
                emit(
                    &state.out,
                    events::CONTEXT_CHANGED,
                    Some(serde_json::json!({ "state": ctx_state })),
                );
            }
            StreamEvent::Cancelled => {
                summary.cancelled = true;
                emit(
                    &state.out,
                    events::MODEL_CANCELLED,
                    Some(serde_json::json!({ "operation_id": op_id.as_str() })),
                );
            }
            StreamEvent::Error(message) => {
                summary.error = Some(message.clone());
                emit(
                    &state.out,
                    events::MODEL_ERROR,
                    serde_json::to_value(ModelErrorEvent {
                        operation_id: op_id.clone(),
                        message,
                    })
                    .ok(),
                );
            }
            // The contour reports the end of the synthesis once, after it has
            // released the context it widened.
            StreamEvent::Done => break,
            // Editing, toolchain, and escalation telemetry of other contours
            // never reaches an architect synthesis.
            _ => {}
        }
    }

    // The window is opened for the whole synthesis and closed exactly here, so
    // the operation is accounted once no matter how many rounds it spent.
    usage.flush().await;
    summary
}

/// Runs one Architect synthesis on the shared contour and relays its telemetry.
///
/// This is the launch skeleton every contour consumer shares, so neither the
/// plan horizon synthesis nor the review group decomposition re-implements the
/// provider resolution, the usage window, the abort registration, or the context
/// merge.
///
/// The contour works on its own clone of the context and owns the auto layer it
/// widened, so the clone is merged back afterwards: the material research paid
/// for survives the run, while the layers the synthesis only borrowed are
/// restored exactly as the core engine leaves them.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_contour_synthesis(
    state: &Arc<AppState>,
    operation: &OperationGuard,
    op_id: &str,
    task: String,
    context: ContextManager,
    options: WorkflowOptions,
    models: ModelsConfig,
    tier: ArchitectTier,
    intro_header: String,
    budget_hint: String,
) -> (anyhow::Result<StructuredRefineOutcome>, ContourRelaySummary) {
    let config = state.config.lock().await.config.clone();
    let provider = match resolve_architect_provider(state, &config, tier.model.provider).await {
        Ok(provider) => provider,
        Err(err) => {
            emit(
                &state.out,
                events::MODEL_ERROR,
                serde_json::to_value(ModelErrorEvent {
                    operation_id: op_id.to_string(),
                    message: err.clone(),
                })
                .ok(),
            );
            return (
                Err(anyhow::anyhow!(err.clone())),
                ContourRelaySummary {
                    error: Some(err),
                    ..ContourRelaySummary::default()
                },
            );
        }
    };

    let repo_root = context.repo_root().to_path_buf();
    let (events_tx, events_rx) = mpsc::channel::<StreamEvent>(CONTOUR_EVENT_CAPACITY);
    let cancel_rx = operation.cancel_receiver();
    let resolver = Arc::new(ArchitectProviderResolver::new(Arc::clone(state), config));

    // The usage window wraps the whole synthesis, so every round and every tier
    // it climbs report into the same accounting window. The relay closes it.
    let usage = state.begin_usage().await;
    let relay = tokio::spawn(relay_contour_events(
        Arc::clone(state),
        op_id.to_string(),
        usage,
        events_rx,
    ));

    let contour = tokio::spawn(async move {
        let mut contour_context = context;
        let result = run_structured_refine(
            &mut contour_context,
            provider,
            &task,
            StructuredRefineConfig {
                repo_root: repo_root.as_path(),
                options: &options,
                models: &models,
                tier,
                providers: resolver.as_ref(),
                intro_header: intro_header.as_str(),
                budget_hint: budget_hint.as_str(),
            },
            events_tx,
            cancel_rx,
        )
        .await;
        (contour_context, result)
    });
    // A hard cancel reaches the task that really runs, not just the future that
    // waits for it, so the abort handle is installed for the whole synthesis and
    // dropped the moment the contour settles.
    operation.set_abort_handle(contour.abort_handle());
    let joined = contour.await;
    operation.clear_abort_handle();

    let summary = match relay.await {
        Ok(summary) => summary,
        Err(join_error) => {
            tracing::warn!(
                operation_id = op_id,
                error = %join_error,
                "Architect telemetry relay terminated unexpectedly"
            );
            ContourRelaySummary::default()
        }
    };

    let (contour_context, outcome) = match joined {
        Ok(joined) => joined,
        Err(join_error) => {
            return (
                Err(anyhow::anyhow!(
                    "The architect synthesis task terminated unexpectedly: {join_error}"
                )),
                summary,
            );
        }
    };

    // The synthesis owns the auto context layer it widened: the material is the
    // Architect's investigation rather than the developer's dialogue, so it is
    // never kept. Everything the contour observed — the files research added
    // and the restored layers — is merged back so a partial run keeps the work
    // that was already paid for.
    let merged = state
        .mutate_context(move |shared| {
            shared.merge_turn_context(&contour_context);
            shared.get_state()
        })
        .await;
    match merged {
        Ok(ctx_state) => emit(
            &state.out,
            events::CONTEXT_CHANGED,
            Some(serde_json::json!({ "state": ctx_state })),
        ),
        Err(err) => {
            tracing::warn!(
                operation_id = op_id,
                error = %err,
                "Failed to merge the architect context back into the shared state"
            );
        }
    }

    (outcome, summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::workflow::fsm::structured::StructuredRefineStatus;
    use tauqe_protocol::{
        ContextAccess, ModelRef, ModelRole, ModelUsageInfo, ReasoningEffort, TurnPhase,
    };

    use crate::transport::OutChannel;

    /// Builds a daemon state bound to its own scratch repository root.
    fn test_state(
        label: &str,
    ) -> (
        Arc<AppState>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        PathBuf,
    ) {
        let root = std::env::temp_dir().join(format!(
            "tauqe-architect-bridge-{}-{label}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create the scratch repository root");
        let (out, rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);
        (state, rx, root)
    }

    /// Decodes the parameters of the events carrying `method`.
    fn collect(raw_events: &[String], method: &str) -> Vec<serde_json::Value> {
        raw_events
            .iter()
            .filter_map(|raw| {
                let value: serde_json::Value = serde_json::from_str(raw).ok()?;
                if value.get("method").and_then(|value| value.as_str()) != Some(method) {
                    return None;
                }
                value.get("params").cloned()
            })
            .collect()
    }

    /// Builds one Architect payload declaring the given status.
    fn success_payload(explanation: &str) -> String {
        serde_json::json!({
            "status": "success",
            "explanation": explanation,
            "reason": null,
            "context_requests": [],
            "repo_searches": [],
            "repo_maps": [],
            "items": []
        })
        .to_string()
    }

    /// The relay is the single translation seam between the contour vocabulary
    /// and the wire: every streamed coordinate reaches the client, a withdrawn
    /// attempt drops the fragments it already streamed, and the usage window is
    /// closed exactly once.
    #[tokio::test]
    async fn relay_translates_contour_events_and_closes_the_usage_window() {
        let (state, mut rx, root) = test_state("relay");
        let usage = state.begin_usage().await;

        let (events_tx, events_rx) = mpsc::channel(64);
        let relay = tokio::spawn(relay_contour_events(
            Arc::clone(&state),
            "op-relay".to_string(),
            usage,
            events_rx,
        ));

        let scripted = [
            StreamEvent::TextDelta("First attempt.".to_string()),
            StreamEvent::ReasoningDelta("Weighing the invariants".to_string()),
            StreamEvent::TierEscalated {
                tier: 1,
                model: ModelRef::openrouter("mock/strong"),
                role: ModelRole::Architect,
                effort: Some(ReasoningEffort::High),
            },
            StreamEvent::TurnPhase {
                phase: TurnPhase::Research,
                round: Some(1),
                max_rounds: Some(3),
                detail: Some("Research round 1".to_string()),
            },
            StreamEvent::RepoMapTokens(512),
            StreamEvent::RetryingConnection {
                attempt: 1,
                max_attempts: 3,
                delay_secs: 2,
                reason: "connection reset mid-stream".to_string(),
            },
            StreamEvent::Usage(ModelUsageInfo {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..ModelUsageInfo::default()
            }),
            StreamEvent::ResetStream,
            StreamEvent::TextDelta("Second attempt.".to_string()),
            StreamEvent::Done,
        ];
        for event in scripted {
            events_tx.send(event).await.expect("the relay is listening");
        }
        drop(events_tx);

        let summary = relay.await.expect("the relay joins");
        assert_eq!(summary.assistant_text, "Second attempt.");
        assert!(!summary.cancelled);
        assert!(summary.error.is_none());
        assert_eq!(
            state.usage.lock().await.operations,
            1,
            "the relay must close the usage window of the synthesis"
        );

        let mut raw_events = Vec::new();
        while let Ok(raw) = rx.try_recv() {
            raw_events.push(raw);
        }

        let deltas: Vec<String> = collect(&raw_events, events::MODEL_TEXT_DELTA)
            .into_iter()
            .filter_map(|params| {
                params
                    .get("delta")
                    .and_then(|delta| delta.as_str())
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(
            deltas,
            vec!["First attempt.".to_string(), "Second attempt.".to_string()]
        );
        assert_eq!(
            collect(&raw_events, events::MODEL_STREAM_RESET).len(),
            1,
            "a voided attempt must withdraw the fragments it streamed"
        );

        let reasoning = collect(&raw_events, events::MODEL_REASONING_DELTA);
        assert_eq!(reasoning.len(), 1);
        assert_eq!(reasoning[0]["delta"], "Weighing the invariants");

        let escalated = collect(&raw_events, events::MODEL_ESCALATED);
        assert_eq!(escalated.len(), 1);
        assert_eq!(escalated[0]["operation_id"], "op-relay");
        assert_eq!(escalated[0]["model"]["name"], "mock/strong");
        assert_eq!(escalated[0]["role"], "architect");
        assert_eq!(escalated[0]["effort"], "high");
        assert_eq!(escalated[0]["tier"], 1);

        let phases = collect(&raw_events, events::TURN_PHASE);
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0]["phase"], "research");
        assert_eq!(phases[0]["round"], 1);
        assert_eq!(phases[0]["max_rounds"], 3);

        let maps = collect(&raw_events, events::TURN_REPOMAP);
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0]["tokens"], 512);

        let retrying = collect(&raw_events, events::MODEL_CONNECTION_RETRYING);
        assert_eq!(retrying.len(), 1);
        assert_eq!(retrying[0]["attempt"], 1);
        assert_eq!(retrying[0]["delay_secs"], 2);
        assert_eq!(retrying[0]["reason"], "connection reset mid-stream");

        // The observed usage is folded into the session totals the final
        // snapshot publishes.
        let snapshots = collect(&raw_events, events::MODEL_USAGE_TOTALS);
        let last = snapshots.last().expect("the relay publishes the snapshot");
        assert_eq!(last["snapshot"]["session"]["total_tokens"], 15);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Cancellation and failure are reported to the client and carried on the
    /// summary, so the adapter can end the operation deterministically.
    #[tokio::test]
    async fn relay_reports_cancellation_and_failure_on_the_summary() {
        let (state, mut rx, root) = test_state("relay-cancel");
        let usage = state.begin_usage().await;

        let (events_tx, events_rx) = mpsc::channel(16);
        let relay = tokio::spawn(relay_contour_events(
            Arc::clone(&state),
            "op-cancel".to_string(),
            usage,
            events_rx,
        ));
        events_tx
            .send(StreamEvent::Error(
                "provider rejected the request".to_string(),
            ))
            .await
            .expect("the relay is listening");
        events_tx
            .send(StreamEvent::Cancelled)
            .await
            .expect("the relay is listening");
        drop(events_tx);

        let summary = relay.await.expect("the relay joins");
        assert!(summary.cancelled);
        assert_eq!(
            summary.error.as_deref(),
            Some("provider rejected the request")
        );

        let mut raw_events = Vec::new();
        while let Ok(raw) = rx.try_recv() {
            raw_events.push(raw);
        }
        assert_eq!(collect(&raw_events, events::MODEL_ERROR).len(), 1);
        assert_eq!(collect(&raw_events, events::MODEL_CANCELLED).len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The skeleton resolves the initial client through the injected seam, runs
    /// the contour to a typed synthesized outcome, accounts the run as one
    /// operation, and merges the context the contour worked on back into the
    /// shared state.
    ///
    /// The current-thread flavor is the deliberate host: no seam of the launch
    /// skeleton may require the daemon's multi-threaded runtime.
    #[tokio::test]
    async fn contour_skeleton_synthesizes_and_merges_the_context() {
        let (state, _events, root) = test_state("skeleton");
        tauqe_core::git::init_repository(Some(&root), true).expect("init the fixture repository");
        std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").expect("write the fixture");

        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            success_payload("The horizon follows from the gathered material"),
        )]));
        *state.provider_override.lock().await = Some(provider.clone() as Arc<dyn LlmProvider>);

        let config = state.config.lock().await.config.clone();
        let models = config.models.clone();
        let tier = models.architect_tier(0);
        let options = WorkflowOptions::from_app_config(&config);

        // The adapter hands the contour a snapshot of the shared context, so the
        // synthesis works on its own clone and reports the result back.
        let mut snapshot = state.context.read().await.clone();
        snapshot
            .add_file("a.rs", ContextAccess::Editable)
            .expect("admit the fixture into the snapshot");
        assert!(
            !state
                .context
                .read()
                .await
                .contains("a.rs")
                .expect("the shared context reads"),
            "the snapshot entry must not be visible before the merge"
        );

        let operation = state
            .try_begin_operation()
            .expect("the operation slot is free");
        let (outcome, summary) = run_contour_synthesis(
            &state,
            &operation,
            "op-skeleton",
            "Synthesize the unfinished horizon".to_string(),
            snapshot,
            options,
            models,
            tier,
            String::new(),
            String::new(),
        )
        .await;

        let outcome = outcome.expect("the synthesis runs to completion");
        assert_eq!(outcome.status, StructuredRefineStatus::Synthesized);
        assert!(outcome.output.is_some());
        assert!(!summary.cancelled);
        assert!(summary.error.is_none());
        assert_eq!(
            provider.requests().len(),
            1,
            "one accepted round spends one provider request"
        );
        assert_eq!(
            state.usage.lock().await.operations,
            1,
            "the whole synthesis is accounted as one operation"
        );

        assert!(
            state
                .context
                .read()
                .await
                .contains("a.rs")
                .expect("the shared context reads"),
            "the contour context must be merged back into the shared state"
        );

        drop(operation);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The resolver is awaited inside the proposal round, so it must resolve on
    /// whatever runtime polls the contour. A current-thread runtime is the exact
    /// host that used to panic inside the bridge's blocking hop.
    #[tokio::test]
    async fn resolver_resolves_an_installed_client_on_a_current_thread_runtime() {
        let (state, _events, root) = test_state("resolver");
        let installed: Arc<dyn LlmProvider> =
            Arc::new(MockLlmProvider::scripted([MockResponse::text("unused")]));
        *state.provider_override.lock().await = Some(installed.clone());

        let config = state.config.lock().await.config.clone();
        let resolver = ArchitectProviderResolver::new(Arc::clone(&state), config);

        let resolved = resolver
            .resolve(ProviderKind::OpenRouter)
            .await
            .expect("the installed client resolves on a current-thread runtime");
        assert!(
            std::ptr::eq(&*resolved, &*installed),
            "the resolver must return the client installed through the test seam"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
