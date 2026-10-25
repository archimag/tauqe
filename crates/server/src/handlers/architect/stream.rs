//! Streaming structured-chat turn for the Architect role.

use std::sync::Arc;

use tauqe_core::providers::{ChatMessage, LlmProvider, ResponseFormat, StreamEvent};
use tauqe_protocol::{events, Event, ModelDeltaEvent, ModelErrorEvent};
use tokio::sync::mpsc;

use crate::handlers::common::withdraw_streamed_attempt;
use crate::state::{AppState, OperationGuard};
use crate::usage::UsageGuard;

pub(super) struct ArchitectStreamOutcome {
    pub(super) raw_text: String,
    pub(super) streamed_explanation_bytes: usize,
}

pub(super) enum ArchitectStreamError {
    Cancelled,
    ModelError(String),
}

/// Executes a streaming structured-chat turn with the architect model, streaming
/// reasoning deltas and explanation text deltas to the client, handling cancellation
/// and errors, and committing token usage to session state.
///
/// `session_id` binds the round to the architect's long-lived design context, so
/// the provider keeps one warm prompt-prefix cache across the discovery rounds of
/// a synthesis and the later replanning waves of the same plan or finding.
///
/// `usage` is the window of the whole synthesis, not of this one round: every
/// round reports into the same window, so a multi-round synthesis is accounted
/// as a single operation instead of one artificial operation per round.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_architect_stream(
    state: &Arc<AppState>,
    provider: Arc<dyn LlmProvider>,
    model_name: String,
    messages: Vec<ChatMessage>,
    response_format: Option<ResponseFormat>,
    reasoning_effort: Option<tauqe_protocol::ReasoningEffort>,
    session_id: Option<String>,
    operation: &OperationGuard,
    op_id: &str,
    usage: &UsageGuard,
) -> Result<ArchitectStreamOutcome, ArchitectStreamError> {
    let cancel_rx = operation.cancel_receiver();
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(100);
    let stream_cancel = cancel_rx.clone();
    let provider_stream = Arc::clone(&provider);

    let stream_task = tokio::spawn(async move {
        provider_stream
            .stream_chat_with_session(
                &model_name,
                messages,
                response_format,
                reasoning_effort,
                session_id.as_deref(),
                tx,
                stream_cancel,
            )
            .await
    });

    operation.set_abort_handle(stream_task.abort_handle());

    let mut raw_text = String::new();
    let mut failure = None;
    let mut stream_cancelled = false;
    let mut passthrough: Option<bool> = None;
    let mut explanation_cursor = tauqe_core::edits::stream::json::decode::StringFieldCursor::new();
    let mut streamed_explanation_bytes = 0usize;

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::ReasoningDelta(delta) => {
                state.out.send_event(&Event {
                    method: events::MODEL_REASONING_DELTA.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id,
                        "delta": delta,
                    })),
                });
            }
            StreamEvent::TextDelta(delta) => {
                raw_text.push_str(&delta);

                if passthrough.is_none() {
                    if let Some(c) = raw_text.chars().find(|c| !c.is_whitespace()) {
                        let plain = c != '{' && c != '`';
                        passthrough = Some(plain);
                        if plain {
                            state.out.send_event(&Event {
                                method: events::MODEL_TEXT_DELTA.to_string(),
                                params: serde_json::to_value(ModelDeltaEvent {
                                    operation_id: op_id.to_string(),
                                    delta: raw_text.clone(),
                                })
                                .ok(),
                            });
                        }
                    }
                } else if passthrough == Some(true) {
                    state.out.send_event(&Event {
                        method: events::MODEL_TEXT_DELTA.to_string(),
                        params: serde_json::to_value(ModelDeltaEvent {
                            operation_id: op_id.to_string(),
                            delta,
                        })
                        .ok(),
                    });
                }

                if passthrough != Some(true) {
                    let (field_delta, _) =
                        tauqe_core::edits::stream::json::extract_streamed_string_field(
                            &raw_text,
                            "explanation",
                            &mut explanation_cursor,
                        );
                    if let Some(d) = field_delta {
                        if !d.is_empty() {
                            streamed_explanation_bytes += d.len();
                            state.out.send_event(&Event {
                                method: events::MODEL_TEXT_DELTA.to_string(),
                                params: serde_json::to_value(ModelDeltaEvent {
                                    operation_id: op_id.to_string(),
                                    delta: d,
                                })
                                .ok(),
                            });
                        }
                    }
                }
            }
            StreamEvent::Usage(reported) => usage.observe(&reported).await,
            StreamEvent::Done => break,
            // The provider may report cancellation as an explicit stream event
            // even when the turn's cancel signal has not been observed yet, so
            // the event is recorded instead of inferred from `cancel_rx` alone.
            StreamEvent::Cancelled => {
                stream_cancelled = true;
                break;
            }
            StreamEvent::Error(err) => {
                failure = Some(err);
                break;
            }
            // The retry decorator withdrew the previous attempt because it broke
            // after partial output. Its explanation fragments belong to an answer
            // the retry regenerates from its beginning, so they are discarded
            // together with the raw payload they were decoded from; keeping
            // either would parse the aborted prefix as part of the retried
            // response and burn the round's parse budget.
            StreamEvent::ResetStream => {
                withdraw_streamed_attempt(&state.out, op_id, || {
                    raw_text.clear();
                    passthrough = None;
                    explanation_cursor =
                        tauqe_core::edits::stream::json::decode::StringFieldCursor::new();
                    streamed_explanation_bytes = 0;
                });
            }
            _ => {}
        }
    }

    let cancelled = *cancel_rx.borrow() || stream_cancelled;
    if cancelled {
        stream_task.abort();
        operation.clear_abort_handle();
        state.out.send_event(&Event {
            method: events::MODEL_CANCELLED.to_string(),
            params: Some(serde_json::json!({ "operation_id": op_id })),
        });
        return Err(ArchitectStreamError::Cancelled);
    }
    if let Some(err) = failure {
        stream_task.abort();
        operation.clear_abort_handle();
        state.out.send_event(&Event {
            method: events::MODEL_ERROR.to_string(),
            params: serde_json::to_value(ModelErrorEvent {
                operation_id: op_id.to_string(),
                message: err.clone(),
            })
            .ok(),
        });
        return Err(ArchitectStreamError::ModelError(err));
    }

    // The streamed task has resolved, so its handle is dropped from the
    // operation slot immediately: the discovery loop keeps working between
    // rounds, and a hard-cancel arriving there must not target a task that is
    // already finished.
    let join_result = stream_task.await;
    operation.clear_abort_handle();

    if let Err(join_err) = join_result {
        let err_msg = join_err.to_string();
        state.out.send_event(&Event {
            method: events::MODEL_ERROR.to_string(),
            params: serde_json::to_value(ModelErrorEvent {
                operation_id: op_id.to_string(),
                message: err_msg.clone(),
            })
            .ok(),
        });
        return Err(ArchitectStreamError::ModelError(err_msg));
    }

    // The aggregate snapshot travels through `model/usageTotals`; the legacy
    // per-operation channel is mirrored from that same snapshot, so both
    // channels always agree instead of accumulating independently.
    crate::handlers::model::emit_legacy_usage_event(state, op_id).await;

    Ok(ArchitectStreamOutcome {
        raw_text,
        streamed_explanation_bytes,
    })
}
