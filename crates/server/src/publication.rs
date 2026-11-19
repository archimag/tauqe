//! Converts application publications into the existing RPC events.
use crate::contracts::Project;
use crate::transport::OutChannel;
use tauqe_flow::api::types::StreamEvent;
use tauqe_flow::api::{ApplicationEvent, ApplicationObserver};
use tauqe_protocol::{
    events, EditFileDoneEvent, EditFileRetryingEvent, EditFileStartedEvent, EditHunkEvent, Event,
    GitCommitCreatedEvent, ModelResultEvent, ModelStartedEvent, ReviewUpdatedEvent,
};

pub(crate) struct Publisher {
    pub(crate) out: OutChannel,
}

#[async_trait::async_trait]
impl ApplicationObserver for Publisher {
    async fn ready(&self, event: &StreamEvent, cancel: &mut tokio::sync::watch::Receiver<bool>) {
        self.out.pace_event(event, cancel).await;
    }

    fn publish(&self, operation_id: &str, event: ApplicationEvent) {
        match event {
            ApplicationEvent::ConfigurationChanged(view) => self.out.send_event(&Event {
                method: events::CONFIG_CHANGED.into(),
                params: serde_json::to_value(crate::handlers::config::config_state(*view)).ok(),
            }),
            ApplicationEvent::ModelChanged { selection, model, roles } => self.out.send_event(&Event {
                method: events::MODEL_CHANGED.into(),
                params: serde_json::to_value(tauqe_protocol::ModelChangedEvent {
                    selection: selection.project(), effective_model: model.project(), roles: Some(roles.project()),
                }).ok(),
            }),
            ApplicationEvent::Focus(focus) => self.out.send_event(&Event {
                method: events::DISCUSSION_FOCUS_CHANGED.to_string(),
                params: serde_json::to_value(tauqe_protocol::DiscussionFocusChangedEvent {
                    focus: crate::discussion::focus_dto(focus),
                }).ok(),
            }),
            ApplicationEvent::Started { model, role, alias, effort, plan_context, work_context, is_batch } => self.out.send_event(&Event {
                method: events::MODEL_STARTED.to_string(),
                params: serde_json::to_value(ModelStartedEvent {
                    operation_id: operation_id.into(),
                    model: model.project(),
                    role: role.map(Project::project),
                    alias,
                    effort: effort.map(Project::project),
                    plan_context: plan_context.map(Project::project),
                    work_context: work_context.map(Project::project),
                    is_batch,
                }).ok(),
            }),
            ApplicationEvent::Stream(event) => publish_stream(&self.out, operation_id, event),
            ApplicationEvent::UsageTotals(snapshot) => crate::usage::publish_snapshot(&self.out, operation_id, &snapshot),
            ApplicationEvent::Usage(snapshot) => crate::usage::publish_legacy_usage(&self.out, operation_id, &snapshot),
            ApplicationEvent::Edit(event) => crate::handlers::edit_cycle::publish(&self.out, operation_id, event),
            ApplicationEvent::Commit { hash, summary, changed_files } => self.out.send_event(&Event {
                method: events::GIT_COMMIT_CREATED.to_string(),
                params: serde_json::to_value(GitCommitCreatedEvent { commit_hash: hash, summary, changed_files }).ok(),
            }),
            ApplicationEvent::Repository(repository) => self.out.send_event(&Event {
                method: events::GIT_STATE_CHANGED.to_string(),
                params: Some(serde_json::json!({"repository":crate::handlers::git::repository_dto(repository)})),
            }),
            ApplicationEvent::SquashCompleted(applied) => self.out.send_event(&Event {
                method: events::GIT_SQUASH_COMPLETED.into(),
                params: serde_json::to_value(crate::handlers::git::squash_result_dto(applied)).ok(),
            }),
            ApplicationEvent::Context(context) => crate::handlers::context::publish_context(&self.out, &context),
            ApplicationEvent::Result { completion, usage } => self.out.send_event(&Event {
                method: events::MODEL_RESULT.to_string(),
                params: Some(serde_json::json!(ModelResultEvent {
                    operation_id: operation_id.into(),
                    result: crate::handlers::model::result::result_dto(completion.result),
                    usage: Some(crate::usage::wire_usage(&usage.prev.to_usage_info())),
                    session_total_cost: Some(usage.session.cost),
                    current_cost: Some(usage.prev.cost),
                    plan_rejections: completion.plan_rejections.into_iter().map(Project::project).collect(),
                    plan_warnings: completion.plan_warnings,
                })),
            }),
            ApplicationEvent::Finished(full_text) => self.out.send_event(&Event {
                method: events::MODEL_FINISHED.to_string(),
                params: Some(serde_json::json!({"operation_id":operation_id, "full_text":full_text})),
            }),
            ApplicationEvent::PlanDocument { plan, is_save } => self.out.send_event(&Event {
                method: events::PLAN_UPDATED.into(),
                params: serde_json::to_value(tauqe_protocol::PlanUpdatedEvent { plan: plan.project(), is_save }).ok(),
            }),
            ApplicationEvent::PlanList { plans, active_id } => self.out.send_event(&Event {
                method: events::PLAN_LIST_CHANGED.into(),
                params: serde_json::to_value(tauqe_protocol::PlanListChangedEvent { plans: plans.into_iter().map(Project::project).collect(), active_id }).ok(),
            }),
            ApplicationEvent::ReviewBatchFinished(report) => {
                publish_review_batch(&self.out, operation_id, report);
            }
            ApplicationEvent::PlanBatchFinished(report) => {
                use tauqe_flow::api::BatchOutcome;
                use tauqe_protocol::BatchFinishStatus;
                let (status, error) = match report.outcome {
                    BatchOutcome::Completed { .. } => (BatchFinishStatus::Completed, None),
                    BatchOutcome::Failed { reason, .. } => (BatchFinishStatus::Failed, Some(reason)),
                    BatchOutcome::Blocked { reason, .. } => (BatchFinishStatus::Blocked, Some(reason)),
                    BatchOutcome::Cancelled { .. } => (BatchFinishStatus::Cancelled, None),
                    BatchOutcome::Stopped { .. } => (BatchFinishStatus::Stopped, None),
                };
                self.out.send_event(&Event {
                    method: events::PLAN_BATCH_FINISHED.into(),
                    params: serde_json::to_value(tauqe_protocol::PlanBatchFinishedEvent {
                        operation_id: operation_id.into(),
                        summary: tauqe_protocol::BatchFinishSummary {
                            owner_id: report.plan_id, status, error, executed_count: report.executed,
                            total_count: report.scheduled, elapsed_seconds: report.elapsed.as_secs(),
                            fixed_count: None, deferred_count: None, failed_count: None,
                        },
                    }).ok(),
                });
            }
            ApplicationEvent::ReviewRun(event) => publish_review(&self.out, operation_id, event),
            ApplicationEvent::ReviewDocument(session) => self.out.send_event(&Event {
                method: events::REVIEW_UPDATED.into(),
                params: serde_json::to_value(ReviewUpdatedEvent { session: session.project() }).ok(),
            }),
            ApplicationEvent::ReviewList { reviews, active_id } => self.out.send_event(&Event {
                method: events::REVIEW_LIST_CHANGED.into(),
                params: serde_json::to_value(tauqe_protocol::ReviewListChangedEvent {
                    reviews: reviews.into_iter().map(Project::project).collect(), active_id,
                }).ok(),
            }),

        }
    }
}

pub(crate) fn publish_stream(out: &OutChannel, op_id: &str, event: StreamEvent) {
    match event {
        StreamEvent::ReasoningDelta(delta) => {
            out.send_event(&Event {
                method: events::MODEL_REASONING_DELTA.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id.to_string(),
                    "delta": delta,
                })),
            });
        }
        StreamEvent::TextDelta(delta) => {
            out.send_event(&Event {
                method: events::MODEL_TEXT_DELTA.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id.to_string(),
                    "delta": delta,
                })),
            });
        }
        StreamEvent::ContextChanged(ctx_state) => {
            crate::handlers::context::publish_context(out, &ctx_state);
        }
        StreamEvent::EditFileStarted { path, op_type } => {
            out.send_event(&Event {
                method: events::EDIT_FILE_STARTED.to_string(),
                params: serde_json::to_value(EditFileStartedEvent {
                    operation_id: op_id.to_string(),
                    path,
                    op_type: op_type.project(),
                })
                .ok(),
            });
        }
        StreamEvent::EditHunk {
            path,
            hunk_index,
            old_start,
            old_lines,
            new_start,
            new_lines,
            context_hint,
            old_text,
            new_text,
        } => {
            out.send_event(&Event {
                method: events::EDIT_HUNK.to_string(),
                params: serde_json::to_value(EditHunkEvent {
                    operation_id: op_id.to_string(),
                    path,
                    hunk_index,
                    old_start,
                    old_lines,
                    new_start,
                    new_lines,
                    context_hint,
                    old_text,
                    new_text,
                })
                .ok(),
            });
        }
        StreamEvent::EditFileDone {
            path,
            status,
            error,
            hunks_count,
        } => {
            out.send_event(&Event {
                method: events::EDIT_FILE_DONE.to_string(),
                params: serde_json::to_value(EditFileDoneEvent {
                    operation_id: op_id.to_string(),
                    path,
                    status: status.project(),
                    error,
                    hunks_count,
                })
                .ok(),
            });
        }
        StreamEvent::EditFileRetrying {
            path,
            attempt,
            max_retries,
            reason,
        } => {
            out.send_event(&Event {
                method: events::EDIT_FILE_RETRYING.to_string(),
                params: serde_json::to_value(EditFileRetryingEvent {
                    operation_id: op_id.to_string(),
                    path,
                    attempt,
                    max_retries,
                    reason,
                })
                .ok(),
            });
        }
        StreamEvent::TurnPhase {
            phase,
            round,
            max_rounds,
            detail,
        } => {
            out.send_event(&Event {
                method: events::TURN_PHASE.to_string(),
                params: serde_json::to_value(tauqe_protocol::TurnPhaseEvent {
                    operation_id: op_id.to_string(),
                    phase: phase.project(),
                    round,
                    max_rounds,
                    detail,
                })
                .ok(),
            });
        }
        StreamEvent::TurnRoundStarted {
            round,
            max_rounds,
            reason,
        } => {
            use tauqe_flow::api::types::TurnRequestReason as CoreReason;
            use tauqe_protocol::model::TurnRequestReason as WireReason;

            let reason = reason.map(|reason| match reason {
                CoreReason::PatchRepair => WireReason::PatchRepair,
                CoreReason::PostHookRepair => WireReason::PostHookRepair,
                CoreReason::VerificationRepair => WireReason::VerificationRepair,
                CoreReason::EmptyResponseRetry => WireReason::EmptyResponseRetry,
            });
            out.send_event(&Event {
                method: events::TURN_ROUND_STARTED.into(),
                params: serde_json::to_value(tauqe_protocol::TurnRoundStartedEvent {
                    operation_id: op_id.into(),
                    round,
                    max_rounds,
                    reason,
                })
                .ok(),
            });
        }
        StreamEvent::ToolchainStarted { command } => {
            out.send_event(&Event {
                method: events::TOOLCHAIN_STARTED.to_string(),
                params: serde_json::to_value(tauqe_protocol::ToolchainStartedEvent {
                    operation_id: op_id.to_string(),
                    command,
                })
                .ok(),
            });
        }
        StreamEvent::ToolchainFinished {
            command,
            success,
            message,
        } => {
            out.send_event(&Event {
                method: events::TOOLCHAIN_FINISHED.to_string(),
                params: serde_json::to_value(tauqe_protocol::ToolchainFinishedEvent {
                    operation_id: op_id.to_string(),
                    command,
                    success,
                    message,
                })
                .ok(),
            });
        }
        StreamEvent::RepoMapTokens(tokens) => {
            out.send_event(&Event {
                method: events::TURN_REPOMAP.to_string(),
                params: serde_json::to_value(tauqe_protocol::RepoMapInfoEvent {
                    operation_id: op_id.to_string(),
                    tokens,
                })
                .ok(),
            });
        }
        StreamEvent::RetryingConnection {
            attempt,
            max_attempts,
            delay_secs,
            reason,
        } => {
            out.send_event(&Event {
                method: events::MODEL_CONNECTION_RETRYING.to_string(),
                params: serde_json::to_value(tauqe_protocol::ModelConnectionRetryingEvent {
                    operation_id: op_id.to_string(),
                    attempt,
                    max_attempts,
                    delay_secs,
                    reason,
                })
                .ok(),
            });
        }
        StreamEvent::TierEscalated {
            tier,
            model,
            role,
            effort,
        } => out.send_event(&Event {
            method: events::MODEL_ESCALATED.to_string(),
            params: serde_json::to_value(tauqe_protocol::ModelEscalatedEvent {
                operation_id: op_id.into(),
                model: model.project(),
                role: Some(role.project()),
                alias: None,
                effort: effort.map(Project::project),
                tier,
            })
            .ok(),
        }),
        StreamEvent::ResetStream => {
            crate::handlers::common::withdraw_streamed_attempt(out, op_id, || ());
        }
        StreamEvent::Cancelled => {
            out.send_event(&Event {
                method: events::MODEL_CANCELLED.to_string(),
                params: Some(serde_json::json!({ "operation_id": op_id.to_string() })),
            });
        }
        StreamEvent::Error(err) => {
            out.send_event(&Event {
                method: events::MODEL_ERROR.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id.to_string(),
                    "message": err.clone(),
                })),
            });
        }
        _ => {}
    }
}

fn publish_review(out: &OutChannel, id: &str, event: tauqe_flow::api::ReviewEvent) {
    use tauqe_flow::api::ReviewEvent;
    use tauqe_protocol::*;
    let (method, params) = match event {
        ReviewEvent::Started {
            model,
            files_count,
            estimated_tokens,
            omitted_files,
        } => (
            events::REVIEW_STARTED,
            serde_json::to_value(ReviewStartedEvent {
                operation_id: id.into(),
                model: model.project(),
                files_count,
                estimated_tokens,
                omitted_files,
            })
            .ok(),
        ),
        ReviewEvent::Finished {
            session,
            usage,
            session_total_cost,
            current_cost,
        } => (
            events::REVIEW_FINISHED,
            serde_json::to_value(ReviewFinishedEvent {
                operation_id: id.into(),
                session: session.project(),
                usage: Some(crate::usage::wire_usage(&usage)),
                session_total_cost: Some(session_total_cost),
                current_cost: Some(current_cost),
            })
            .ok(),
        ),
        ReviewEvent::Cancelled => (
            events::REVIEW_CANCELLED,
            Some(serde_json::json!({"operation_id": id})),
        ),
        ReviewEvent::Error(message) => (
            events::REVIEW_ERROR,
            serde_json::to_value(ReviewErrorEvent {
                operation_id: id.into(),
                message,
            })
            .ok(),
        ),
        ReviewEvent::Reasoning(delta) => (
            events::REVIEW_REASONING_DELTA,
            serde_json::to_value(ReviewReasoningDeltaEvent {
                operation_id: id.into(),
                delta,
            })
            .ok(),
        ),
        ReviewEvent::Content(delta) => (
            events::REVIEW_CONTENT_DELTA,
            serde_json::to_value(ReviewContentDeltaEvent {
                operation_id: id.into(),
                delta,
            })
            .ok(),
        ),
        ReviewEvent::Reset => {
            crate::handlers::common::withdraw_streamed_attempt(out, id, || ());
            return;
        }
        ReviewEvent::AnalyzeStarted {
            model,
            findings_count,
        } => (
            events::REVIEW_ANALYZE_STARTED,
            serde_json::to_value(ReviewAnalyzeStartedEvent {
                operation_id: id.into(),
                model: model.project(),
                findings_count,
            })
            .ok(),
        ),
        ReviewEvent::AnalyzeFinished(session) => (
            events::REVIEW_ANALYZE_FINISHED,
            serde_json::to_value(ReviewAnalyzeFinishedEvent {
                operation_id: id.into(),
                session: session.project(),
            })
            .ok(),
        ),
        ReviewEvent::GroupStarted {
            group,
            files_count,
            estimated_tokens,
        } => (
            events::REVIEW_GROUP_STARTED,
            serde_json::to_value(ReviewGroupStartedEvent {
                operation_id: id.into(),
                group_id: group.id,
                group_title: group.title,
                files_count,
                estimated_tokens,
            })
            .ok(),
        ),
        ReviewEvent::GroupFinished {
            group_id,
            findings_count,
            error,
        } => (
            events::REVIEW_GROUP_FINISHED,
            serde_json::to_value(ReviewGroupFinishedEvent {
                operation_id: id.into(),
                group_id,
                findings_count,
                success: error.is_none(),
                error,
            })
            .ok(),
        ),
    };
    out.send_event(&Event {
        method: method.into(),
        params,
    });
}

fn publish_review_batch(
    out: &OutChannel,
    operation_id: &str,
    report: tauqe_flow::api::ExecutedReviewBatch,
) {
    use crate::handlers::common::emit;
    use tauqe_protocol::{BatchFinishStatus, BatchFinishSummary, ReviewBatchFinishedEvent};
    let status = match report.stop {
        tauqe_flow::api::types::BatchStop::Completed => BatchFinishStatus::Completed,
        tauqe_flow::api::types::BatchStop::Failed { .. } => BatchFinishStatus::Failed,
        tauqe_flow::api::types::BatchStop::Blocked { .. } => BatchFinishStatus::Blocked,
        tauqe_flow::api::types::BatchStop::Cancelled => BatchFinishStatus::Cancelled,
        tauqe_flow::api::types::BatchStop::Stopped => BatchFinishStatus::Stopped,
    };
    emit(
        out,
        events::REVIEW_BATCH_FINISHED,
        serde_json::to_value(ReviewBatchFinishedEvent {
            operation_id: operation_id.into(),
            summary: BatchFinishSummary {
                owner_id: report.owner_id,
                status,
                executed_count: report.executed_count,
                total_count: report.total_count,
                elapsed_seconds: report.elapsed.as_secs(),
                error: report.error,
                fixed_count: Some(report.fixed_count),
                deferred_count: Some(report.deferred),
                failed_count: Some(report.failed_count),
            },
        })
        .ok(),
    );
}
