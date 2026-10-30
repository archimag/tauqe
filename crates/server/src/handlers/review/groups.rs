//! Group audit supervisor: one isolated bounded audit per review group.
//!
//! A grouped session is audited group by group under the core [`BatchExecution`]
//! supervisor, so queue semantics stay the ones the engine already owns: ordered
//! traversal, fail-fast on an infrastructure failure, aggregated usage, and
//! cancellation observed between members.
//!
//! Transactional isolation (#c-1) is the reason one group runs as one batch
//! member instead of a single monolithic audit. A group that yields no usable
//! result — an empty answer, an unparsable payload, a path set that selects no
//! repository file — is a *domain* failure: it is reported on that group's
//! terminal event and the queue advances to the next group, because the remaining
//! groups audit their own slices and their findings stay valid. Only an
//! *infrastructure* failure (the provider cannot be reached, the stream breaks,
//! the findings cannot be written) stops the queue, since nothing later can
//! succeed while the transport or the storage is broken. Findings a group already
//! appended stay on disk either way, so a cancelled or failed run never discards
//! completed work.
//!
//! Findings are immutable and reference their origin (#c-3): every group appends
//! through `ReviewStorage::append_group_findings`, which stamps a fresh UUIDv7
//! identity together with the identifier of the group that produced the finding,
//! so a later post-analysis layer can classify findings by reference without
//! rewriting one. The supervisor assigns only the item ordinals, keeping
//! identifiers unique across the whole session.
//!
//! A session that carries no explicit groups is audited as its single implicit
//! `Default` group, so the pre-grouping single audit stays the degenerate case of
//! the same path instead of a second implementation (#c-4).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tauqe_core::edits::stream::json::JsonFieldStreamFilter;
use tauqe_core::providers::{
    create_provider, format_session_id, ChatMessage, LlmProvider, StreamEvent,
    SESSION_PREFIX_REVIEW,
};
use tauqe_core::review::parser::ReviewFindingsOutput;
use tauqe_core::review::{
    build_review_system_prompt, build_review_user_prompt, parse_review_output,
    review_findings_response_format, ReviewStorage,
};
use tauqe_core::workflow::batch::{
    BatchExecution, BatchReport, BatchStep, BatchStepExecutor, BatchStepOutcome,
};
use tauqe_protocol::{
    events, ModelRole, ModelSelection, ModelUsageEvent, ModelUsageInfo, ReviewGroup,
    ReviewGroupFinishedEvent, ReviewGroupStartedEvent, ReviewSession,
};
use tokio::sync::{mpsc, watch};

use crate::state::{AppState, OperationGuard};

use super::common::{emit, offload_review, publish_review};
use super::window::{expand_window, WindowBudgets, WindowContext};

/// Why one group's audit did not produce findings.
///
/// The distinction is what keeps the batch isolated: a domain failure concerns
/// the group alone, while an infrastructure failure makes every later group
/// futile.
enum GroupFailure {
    /// The group produced no usable result; the queue continues.
    Domain(String),
    /// The environment could not deliver the audit; the queue stops.
    Infrastructure(String),
    /// The developer interrupted the run.
    Cancelled,
}

/// Executor that audits one review group per batch step.
struct GroupAuditExecutor<'a> {
    state: &'a Arc<AppState>,
    repo_root: &'a Path,
    /// Snapshot of the audited session: it supplies the model, the identifier
    /// bound to the provider session, and the groups themselves.
    session: &'a ReviewSession,
    /// Groups of that snapshot, matched by the step identifier.
    groups: &'a [ReviewGroup],
    /// Provider every group streams through.
    provider: Arc<dyn LlmProvider>,
    budgets: WindowBudgets,
    /// Registered operation, so a stalled streamed call can be aborted by the
    /// hard-cancel watchdog instead of outliving its turn.
    operation: &'a Arc<OperationGuard>,
    /// Identifier of the registered operation, carried by every published event.
    operation_id: String,
    /// Next item ordinal, kept unique across groups.
    next_item_id: u32,
}

impl GroupAuditExecutor<'_> {
    /// Audits one group in isolation and appends its findings to the session.
    ///
    /// Returns the number of findings the group appended.
    async fn audit_group(
        &mut self,
        group: &ReviewGroup,
        cancel_rx: &watch::Receiver<bool>,
    ) -> Result<usize, GroupFailure> {
        // The group audit expands its declared paths through the same shared
        // projection the plain launch and the read-only preview consume, so a
        // preset never applies its own selection order or its own limits
        // (#c-1, #c-5).
        let expanded = expand_window(
            self.repo_root,
            WindowContext::empty(),
            &group.paths,
            self.budgets,
        )
        .await
        .map_err(GroupFailure::Infrastructure)?;

        emit_group_started(
            self.state,
            &self.operation_id,
            group,
            expanded.files.len(),
            expanded.estimated_tokens,
        );
        if expanded.window.omitted_files > 0 {
            tracing::warn!(
                review_id = self.session.id.as_str(),
                group_id = group.id.as_str(),
                omitted = expanded.window.omitted_files,
                "review group expansion exhausted the configured file token budget; remaining matched files were not loaded"
            );
        }
        if expanded.files.is_empty() {
            return Err(GroupFailure::Domain(format!(
                "Review group '{}' selected no repository files from paths: {}",
                group.title,
                group.paths.join(", ")
            )));
        }
        let files = expanded.files;

        let messages = vec![
            ChatMessage::system(build_review_system_prompt()),
            ChatMessage::user(build_review_user_prompt(&files, group.prompt.as_deref())),
        ];
        // A group audit is a non-code operation: the enforced JSON schema turns
        // the reviewer output into machine-readable findings.
        let response_format = review_findings_response_format();
        // Every group of one audit extends the same provider prefix cache, so the
        // session identifier is derived from the review, not from the group.
        let session_id = format_session_id(SESSION_PREFIX_REVIEW, &self.session.id);
        let model_name = self.session.model.name.clone();
        let reasoning_effort = {
            let config = self.state.config.lock().await.config.clone();
            config.models.resolve_selection_effort(
                ModelRole::Reviewer,
                &ModelSelection::Specific(self.session.model.clone()),
            )
        };

        // The guard opens the accounting window before the provider call so the
        // group reports both the usage the provider returns and the duration the
        // harness measures itself.
        let usage_guard = self.state.begin_usage().await;

        let (tx, mut rx) = mpsc::channel::<StreamEvent>(100);
        let err_tx = tx.clone();
        let provider = Arc::clone(&self.provider);
        let stream_cancel = cancel_rx.clone();
        let stream_task = tokio::spawn(async move {
            if let Err(err) = provider
                .stream_chat_with_session(
                    &model_name,
                    messages,
                    Some(response_format),
                    reasoning_effort,
                    Some(session_id.as_str()),
                    tx,
                    stream_cancel,
                )
                .await
            {
                let _ = err_tx.send(StreamEvent::Error(err.to_string())).await;
            }
        });
        self.operation.set_abort_handle(stream_task.abort_handle());

        // The provider streams raw Structured Output JSON; the filter decodes the
        // `summary` string so the accumulated body keeps its contract shape while
        // JSON syntax never has to be inspected by hand.
        let mut filter = JsonFieldStreamFilter::<ReviewFindingsOutput>::new("summary");
        let mut usage: Option<ModelUsageInfo> = None;
        let mut failure: Option<String> = None;
        let mut stream_cancelled = false;

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::TextDelta(chunk) => {
                    // A group audit renders no live text, so the decoded delta is
                    // discarded while the filter keeps accumulating the payload.
                    let _ = filter.push_chunk(&chunk);
                }
                StreamEvent::Usage(reported) => usage = Some(reported),
                StreamEvent::Done => break,
                StreamEvent::Cancelled => {
                    stream_cancelled = true;
                    break;
                }
                StreamEvent::Error(err) => {
                    failure = Some(err);
                    break;
                }
                // The retry decorator withdrew the previous attempt because it
                // broke after partial output: the filter holds a prefix of a
                // document the retry regenerates from its beginning, so it is
                // restarted instead of accumulating both payloads into one
                // unparsable body. A group streams no text to the client, so
                // there is nothing to withdraw there.
                StreamEvent::ResetStream => {
                    filter = JsonFieldStreamFilter::<ReviewFindingsOutput>::new("summary");
                }
                _ => {}
            }
        }

        // The streamed call has resolved, so the slot must stop pointing at it: a
        // later hard-cancel would otherwise abort an already finished task.
        self.operation.clear_abort_handle();

        // Account for the tokens, cost, and duration the group consumed before
        // any early return, so a failed or cancelled group is still charged.
        let reported_usage = usage.unwrap_or_default();
        usage_guard.observe(&reported_usage).await;
        usage_guard.flush().await;
        let session_total_cost = self.state.usage.lock().await.session.cost;
        let current_cost = reported_usage.cost.unwrap_or(0.0);
        emit(
            &self.state.out,
            events::MODEL_USAGE,
            serde_json::to_value(ModelUsageEvent {
                operation_id: self.operation_id.clone(),
                usage: reported_usage,
                session_total_cost,
                current_cost: Some(current_cost),
            })
            .ok(),
        );

        if *cancel_rx.borrow() || stream_cancelled {
            stream_task.abort();
            return Err(GroupFailure::Cancelled);
        }
        if let Some(message) = failure {
            stream_task.abort();
            return Err(GroupFailure::Infrastructure(message));
        }

        // Capture the raw payload before finalizing: `finish` consumes the filter,
        // while the shared parser still needs the full body to map the findings.
        let raw_buffer = filter.buffer().to_string();
        if raw_buffer.trim().is_empty() {
            return Err(GroupFailure::Domain(format!(
                "Model returned an empty review for group '{}'",
                group.title
            )));
        }
        // A payload that deserializes strictly into the review schema is
        // authoritative; one that does not is a domain failure of this group
        // rather than a fabricated finding, because the tolerant text fallback
        // would record the raw model prose as a review result.
        if let Err(err) = filter.finish() {
            return Err(GroupFailure::Domain(format!(
                "Review group '{}' returned an unparsable payload: {}",
                group.title, err
            )));
        }
        let mut findings = parse_review_output(&raw_buffer).2;

        // Item identifiers are unique across the whole session: the parser
        // numbers the findings of one group from one, so the supervisor continues
        // the session's sequence instead of letting groups collide on `id`.
        let first_item_id = self.next_item_id;
        for (index, finding) in findings.iter_mut().enumerate() {
            finding.id = first_item_id.saturating_add(index as u32);
        }
        let appended = findings.len();

        let save_root = self.repo_root.to_path_buf();
        let review_id = self.session.id.clone();
        let group_id = group.id.clone();
        let stored = offload_review(move || {
            ReviewStorage::append_group_findings(&save_root, &review_id, &group_id, findings)
        })
        .await;
        match stored {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(GroupFailure::Infrastructure(format!(
                    "Review session '{}' disappeared before group '{}' findings were appended",
                    self.session.id, group.title
                )))
            }
            Err(err) => return Err(GroupFailure::Infrastructure(err)),
        }
        self.next_item_id = first_item_id.saturating_add(appended as u32);

        // The appended findings are the group's durable result: publishing the
        // session and the refreshed list makes them visible immediately, so a
        // cancelled or failed run still shows every completed group.
        publish_review(self.state, self.repo_root, &self.session.id).await;

        Ok(appended)
    }
}

#[async_trait]
impl BatchStepExecutor for GroupAuditExecutor<'_> {
    async fn execute_step(
        &mut self,
        step: &BatchStep,
        _stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<BatchStepOutcome> {
        // The slice is a shared reference with its own lifetime, so the lookup
        // result does not borrow the executor and `audit_group` can take `&mut self`.
        let groups = self.groups;
        let Some(group) = groups.iter().find(|group| group.id == step.id) else {
            return Ok(BatchStepOutcome::failed(format!(
                "Review group '{}' is not part of the audited session '{}'",
                step.id, self.session.id
            )));
        };

        let outcome = match self.audit_group(group, &cancel_rx).await {
            Ok(findings) => {
                emit_group_finished(self.state, &self.operation_id, group, findings, None);
                BatchStepOutcome::succeeded()
            }
            Err(GroupFailure::Domain(reason)) => {
                // A domain failure is confined to its own group: the terminal
                // event carries the diagnostic and the step reports success, so
                // the supervisor advances to the next group instead of
                // invalidating work the other groups can still complete.
                tracing::warn!(
                    review_id = self.session.id.as_str(),
                    group_id = group.id.as_str(),
                    reason = %reason,
                    "review group audit failed; the batch continues"
                );
                emit_group_finished(self.state, &self.operation_id, group, 0, Some(reason));
                BatchStepOutcome::succeeded()
            }
            Err(GroupFailure::Infrastructure(reason)) => {
                tracing::error!(
                    review_id = self.session.id.as_str(),
                    group_id = group.id.as_str(),
                    reason = %reason,
                    "review infrastructure failure stopped the group batch"
                );
                emit_group_finished(
                    self.state,
                    &self.operation_id,
                    group,
                    0,
                    Some(reason.clone()),
                );
                BatchStepOutcome::failed(reason)
            }
            Err(GroupFailure::Cancelled) => {
                emit_group_finished(
                    self.state,
                    &self.operation_id,
                    group,
                    0,
                    Some(format!(
                        "Review group '{}' audit was cancelled",
                        group.title
                    )),
                );
                BatchStepOutcome::cancelled()
            }
        };
        Ok(outcome)
    }
}

/// Emits the event announcing that one group started auditing its own slice.
fn emit_group_started(
    state: &AppState,
    operation_id: &str,
    group: &ReviewGroup,
    files_count: usize,
    estimated_tokens: usize,
) {
    emit(
        &state.out,
        events::REVIEW_GROUP_STARTED,
        serde_json::to_value(ReviewGroupStartedEvent {
            operation_id: operation_id.to_string(),
            group_id: group.id.clone(),
            group_title: group.title.clone(),
            files_count,
            estimated_tokens,
        })
        .ok(),
    );
}

/// Emits the terminal event of one group, carrying its failure when it has one.
fn emit_group_finished(
    state: &AppState,
    operation_id: &str,
    group: &ReviewGroup,
    findings_count: usize,
    error: Option<String>,
) {
    emit(
        &state.out,
        events::REVIEW_GROUP_FINISHED,
        serde_json::to_value(ReviewGroupFinishedEvent {
            operation_id: operation_id.to_string(),
            group_id: group.id.clone(),
            findings_count,
            success: error.is_none(),
            error,
        })
        .ok(),
    );
}

/// Audits every group of a session snapshot as an isolated batch member.
///
/// The steps are the session's effective groups, so a session that carries no
/// explicit groups audits its single implicit `Default` group and keeps the
/// pre-grouping behavior (#c-4). The returned report is what a caller needs to
/// decide whether a post-analysis layer may follow: the terminal stop reason and
/// the steps that actually ran.
pub(crate) async fn run_group_audits(
    state: &Arc<AppState>,
    repo_root: &Path,
    session: &ReviewSession,
    operation: Arc<OperationGuard>,
) -> Result<BatchReport, String> {
    let groups = session.effective_groups();
    let steps: Vec<BatchStep> = groups
        .iter()
        .map(|group| BatchStep::new(group.id.clone(), group.title.clone()))
        .collect();

    let (provider, budgets) = {
        let config = state.config.lock().await.config.clone();
        let budgets = WindowBudgets {
            files_tokens: config.context.files_token_budget,
        };
        // Deterministic unit tests inject a scripted provider through the state's
        // test seam; production builds construct a real network provider from
        // configuration.
        #[cfg(test)]
        let provider: Arc<dyn LlmProvider> = match state.provider_override.lock().await.clone() {
            Some(provider) => provider,
            None => match create_provider(session.model.provider, &config) {
                Ok(provider) => Arc::from(provider),
                Err(err) => return Err(err.to_string()),
            },
        };
        #[cfg(not(test))]
        let provider: Arc<dyn LlmProvider> = match create_provider(session.model.provider, &config)
        {
            Ok(provider) => Arc::from(provider),
            Err(err) => return Err(err.to_string()),
        };
        (provider, budgets)
    };

    let mut executor = GroupAuditExecutor {
        state,
        repo_root,
        session,
        groups: &groups,
        provider,
        budgets,
        operation: &operation,
        operation_id: format!("op-{}", operation.id()),
        next_item_id: session
            .items
            .iter()
            .map(|item| item.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1),
    };

    let (stream_tx, _stream_rx) = mpsc::channel::<StreamEvent>(1);
    // The batch borrows the caller's registered operation, so a cancellation
    // delivered between groups stops the queue and a soft stop drains it after the
    // group that is already running — the engine's supervisor semantics.
    let cancel_rx = operation.cancel_receiver();
    let soft_stop_rx = operation.soft_stop_receiver();

    let report = BatchExecution::new()
        .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
        .await;

    tracing::info!(
        review_id = session.id.as_str(),
        groups = steps.len(),
        executed = report.executed.len(),
        stop = ?report.stop,
        total_tokens = report.usage.total_tokens,
        "review group batch finished"
    );

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::Value;
    use tauqe_core::config::AppConfig;
    use tauqe_core::providers::mock::MockResponse;
    use tauqe_core::providers::{MockLlmProvider, ResponseFormat};
    use tauqe_core::workflow::batch::BatchStop;
    use tauqe_protocol::{ModelRef, ProviderKind};

    /// Scratch repository, its daemon state, and the published event stream.
    struct GroupFixture {
        root: std::path::PathBuf,
        state: Arc<AppState>,
        events: tokio::sync::mpsc::UnboundedReceiver<String>,
    }

    impl Drop for GroupFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Builds a scratch repository holding one file per audit group, plus the
    /// daemon state bound to it.
    fn fixture(label: &str) -> GroupFixture {
        let root = std::env::temp_dir().join(format!(
            "tauqe-review-groups-{}-{}-{}",
            std::process::id(),
            label,
            crate::state::next_operation_id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::create_dir_all(root.join("beta")).unwrap();
        std::fs::write(root.join("alpha/a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(root.join("beta/b.rs"), "fn beta() {}\n").unwrap();

        // The audit reads tracked files only, so the fixture commits them; the
        // identity is local to the scratch repository so no workstation
        // configuration takes part.
        tauqe_core::git::init_repository(Some(&root), false).unwrap();
        for args in [
            vec!["config", "user.name", "Tauqe Test"],
            vec!["config", "user.email", "test@tauqe.local"],
            vec!["add", "-A"],
            vec!["commit", "-m", "chore: group fixture"],
        ] {
            tauqe_core::git::run_git(Some(&root), &args).unwrap();
        }

        let (out, events) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);
        GroupFixture {
            root,
            state,
            events,
        }
    }

    fn group(id: &str, title: &str, paths: &[&str]) -> ReviewGroup {
        ReviewGroup {
            id: id.to_string(),
            title: title.to_string(),
            paths: paths.iter().map(|path| path.to_string()).collect(),
            prompt: None,
            ..Default::default()
        }
    }

    fn session(groups: Vec<ReviewGroup>) -> ReviewSession {
        ReviewSession {
            id: "rev-groups".to_string(),
            title: "Grouped Audit".to_string(),
            created_at: 100,
            model: ModelRef::openrouter("test/reviewer"),
            target_files: vec!["alpha/a.rs".to_string(), "beta/b.rs".to_string()],
            groups,
            ..ReviewSession::default()
        }
    }

    /// A single well-formed finding payload produced by a group audit.
    fn findings_payload(title: &str) -> String {
        serde_json::json!({
            "title": "Group Audit",
            "summary": "The audited slice is sound.",
            "findings": [{
                "title": title,
                "type": "quality",
                "description": "Strings are cloned inside the loop.",
                "recommendations": ["Borrow instead of cloning"],
                "files": ["alpha/a.rs"],
                "readonly_files": []
            }]
        })
        .to_string()
    }

    /// Drains the published event stream into `(method, params)` pairs.
    ///
    /// A drain consumes the queue, so the whole run is collected once and
    /// filtered afterwards: draining per method would discard every event the
    /// previous drain did not match.
    fn drain_events(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    ) -> Vec<(String, Value)> {
        let mut collected = Vec::new();
        while let Ok(raw) = events.try_recv() {
            let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let Some(method) = value.get("method").and_then(|value| value.as_str()) else {
                continue;
            };
            let params = value.get("params").cloned().unwrap_or_default();
            collected.push((method.to_string(), params));
        }
        collected
    }

    /// Returns the parameters of every drained event of one method.
    fn events_of(drained: &[(String, Value)], method: &str) -> Vec<Value> {
        drained
            .iter()
            .filter(|(name, _)| name.as_str() == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    /// Provider double that cancels the run on the call it is told to.
    struct CancellingProvider {
        state: Arc<AppState>,
        payload: String,
        cancel_on_call: usize,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for CancellingProvider {
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
            let call = self.calls.fetch_add(1, Ordering::SeqCst).saturating_add(1);
            if call >= self.cancel_on_call {
                // The developer interrupted the audit while this group was in
                // flight: the group reports the cancellation and the queue stops.
                self.state.cancel_active_operation();
                let _ = tx.send(StreamEvent::Cancelled).await;
                return Ok(());
            }
            let _ = tx.send(StreamEvent::TextDelta(self.payload.clone())).await;
            let _ = tx.send(StreamEvent::Done).await;
            Ok(())
        }
    }

    /// A group that produces no usable result is a domain failure: it is
    /// reported on its own terminal event and the queue advances, so the findings
    /// the next group produces are neither lost nor invalidated (#c-1).
    #[tokio::test]
    async fn a_failed_group_does_not_invalidate_the_next_group() {
        let mut fixture = fixture("domain-isolation");
        let session = session(vec![
            group("grp-alpha", "Alpha", &["alpha/"]),
            group("grp-beta", "Beta", &["beta/"]),
        ]);
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(""),
            MockResponse::text(findings_payload("Beta cloning")),
        ]));
        *fixture.state.provider_override.lock().await = Some(provider);

        let operation = Arc::new(fixture.state.try_begin_operation().unwrap());
        let report = run_group_audits(&fixture.state, &fixture.root, &session, operation)
            .await
            .expect("the group batch runs");

        assert_eq!(
            report.stop,
            BatchStop::Completed,
            "a domain failure must not stop the queue"
        );
        assert_eq!(report.executed.len(), 2);

        let published = drain_events(&mut fixture.events);
        assert_eq!(events_of(&published, events::REVIEW_GROUP_STARTED).len(), 2);
        let finished = events_of(&published, events::REVIEW_GROUP_FINISHED);
        assert_eq!(finished.len(), 2);

        let alpha = finished
            .iter()
            .find(|params| params["group_id"] == serde_json::json!("grp-alpha"))
            .expect("the failed group reports its terminal event");
        assert_eq!(alpha["success"], serde_json::json!(false));
        assert!(alpha["error"]
            .as_str()
            .unwrap_or_default()
            .contains("empty"));

        let beta = finished
            .iter()
            .find(|params| params["group_id"] == serde_json::json!("grp-beta"))
            .expect("the surviving group reports its terminal event");
        assert_eq!(beta["success"], serde_json::json!(true));
        assert_eq!(beta["findings_count"], serde_json::json!(1));

        let stored = ReviewStorage::load_review(&fixture.root, "rev-groups")
            .unwrap()
            .expect("the session is stored");
        assert_eq!(stored.items.len(), 1);
        assert_eq!(stored.items[0].group.as_deref(), Some("grp-beta"));
        assert!(
            stored.items[0].uuid.is_some(),
            "an appended finding carries its own identity"
        );
    }

    /// A group that finished before the interruption stays on disk, so a
    /// cancelled run never discards completed work (#c-1, #c-3).
    #[tokio::test]
    async fn cancellation_keeps_the_groups_already_appended() {
        let mut fixture = fixture("cancel");
        let session = session(vec![
            group("grp-alpha", "Alpha", &["alpha/"]),
            group("grp-beta", "Beta", &["beta/"]),
        ]);
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let provider = Arc::new(CancellingProvider {
            state: Arc::clone(&fixture.state),
            payload: findings_payload("Alpha cloning"),
            cancel_on_call: 2,
            calls: AtomicUsize::new(0),
        });
        *fixture.state.provider_override.lock().await = Some(provider);

        let operation = Arc::new(fixture.state.try_begin_operation().unwrap());
        let report = run_group_audits(&fixture.state, &fixture.root, &session, operation)
            .await
            .expect("the group batch runs");

        assert_eq!(report.stop, BatchStop::Cancelled);
        assert_eq!(report.executed.len(), 2);

        let published = drain_events(&mut fixture.events);
        let finished = events_of(&published, events::REVIEW_GROUP_FINISHED);
        let alpha = finished
            .iter()
            .find(|params| params["group_id"] == serde_json::json!("grp-alpha"))
            .expect("the completed group reports its terminal event");
        assert_eq!(alpha["success"], serde_json::json!(true));
        assert_eq!(alpha["findings_count"], serde_json::json!(1));
        let beta = finished
            .iter()
            .find(|params| params["group_id"] == serde_json::json!("grp-beta"))
            .expect("the cancelled group reports its terminal event");
        assert_eq!(beta["success"], serde_json::json!(false));

        let stored = ReviewStorage::load_review(&fixture.root, "rev-groups")
            .unwrap()
            .expect("the session is stored");
        assert_eq!(stored.items.len(), 1);
        assert_eq!(stored.items[0].group.as_deref(), Some("grp-alpha"));
    }

    /// Findings of different groups share one session, so their item identifiers
    /// must stay unique and every finding must keep the provenance of its group.
    #[tokio::test]
    async fn every_group_appends_its_findings_with_unique_identifiers() {
        let fixture = fixture("unique-ids");
        let session = session(vec![
            group("grp-alpha", "Alpha", &["alpha/"]),
            group("grp-beta", "Beta", &["beta/"]),
        ]);
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(findings_payload("Alpha cloning")),
            MockResponse::text(findings_payload("Beta cloning")),
        ]));
        *fixture.state.provider_override.lock().await = Some(provider);

        let operation = Arc::new(fixture.state.try_begin_operation().unwrap());
        let report = run_group_audits(&fixture.state, &fixture.root, &session, operation)
            .await
            .expect("the group batch runs");
        assert!(report.is_success());
        assert_eq!(report.executed.len(), 2);

        let stored = ReviewStorage::load_review(&fixture.root, "rev-groups")
            .unwrap()
            .expect("the session is stored");
        assert_eq!(stored.items.len(), 2);

        let mut ids: Vec<u32> = stored.items.iter().map(|item| item.id).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![1, 2],
            "appended findings stay addressable by a unique id"
        );
        let provenance: Vec<&str> = stored
            .items
            .iter()
            .map(|item| item.group.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(provenance, vec!["grp-alpha", "grp-beta"]);
        for item in &stored.items {
            assert!(
                item.uuid.is_some(),
                "every appended finding carries its own identity"
            );
        }
    }
}
