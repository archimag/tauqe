use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::watch;
use tauqe_protocol::{ContextAccess, EditOperation, ModelResult};

use tauqe_protocol::ModelRef;

use crate::context::{ContextFileContent, ContextManager};
use crate::edits::protocol::generate_turn_marker;
use crate::edits::{stage_and_validate_edits, EditError, EditProtocol, StagedEditsState};
use crate::history::{HistoryManager, DEFAULT_HISTORY_BUDGET_TOKENS, DEFAULT_TAIL_TURNS_COUNT};
use crate::prompt::PromptAssembly;
use crate::providers::{ChatMessage, LlmProvider, StreamEvent};

pub use super::discovery::*;
pub use super::retry::*;
pub use super::verification::*;

use super::lifecycle::WorkflowOptions;

pub struct ParsedPipelineOutput {
    pub assistant_text: String,
    pub parsed_result: ModelResult,
    pub is_cancelled: bool,
    pub detected_language: Option<String>,
}

/// Options controlling a single invocation step of the LLM pipeline.
#[derive(Debug, Default, Clone)]
pub struct StepParams<'a> {
    pub record_turn: bool,
    pub emit_done: bool,
    pub staged_state: Option<&'a StagedEditsState>,
    pub target_language: Option<&'a str>,
}

/// Compacts session history if estimated tokens exceed the configured threshold using default budgets.
pub async fn compact_history_if_needed(
    history_manager: &mut HistoryManager,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    compact_history_if_needed_with_budget(
        history_manager,
        provider,
        model,
        cancel_rx,
        DEFAULT_HISTORY_BUDGET_TOKENS,
        DEFAULT_TAIL_TURNS_COUNT,
    )
    .await
}

/// Compacts session history if estimated tokens exceed a specific budget threshold.
pub async fn compact_history_if_needed_with_budget(
    history_manager: &mut HistoryManager,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    cancel_rx: watch::Receiver<bool>,
    budget_tokens: u64,
    tail_turns: usize,
) -> anyhow::Result<()> {
    if !history_manager.needs_compaction(budget_tokens)? {
        return Ok(());
    }

    let Some((head, tail)) = history_manager.prepare_compaction(tail_turns)? else {
        return Ok(());
    };

    let mut head_text = String::new();
    for entry in &head {
        if let Ok(line) = entry.to_jsonl_line() {
            head_text.push_str(&line);
            head_text.push('\n');
        }
    }

    let summarization_prompt = format!(
        "Summarize the following interaction history into a concise, structured summary capturing the completed tasks, key code changes, and context:\n\n```jsonl\n{}\n```\nProvide only the summary text without any JSON or XML wrapper.",
        head_text.trim()
    );

    let messages = vec![
        ChatMessage::system("You are an expert software engineering assistant. Your task is to summarize past development session history concisely and accurately."),
        ChatMessage::user(summarization_prompt),
    ];

    let (tx, mut rx) = mpsc::channel(100);
    let client_call = provider.stream_chat(
        &model.name,
        messages,
        None,
        tx,
        cancel_rx,
    );

    let collect_task = tokio::spawn(async move {
        let mut text = String::new();
        while let Some(ev) = rx.recv().await {
            if let StreamEvent::TextDelta(d) = ev {
                text.push_str(&d);
            }
        }
        text
    });

    match client_call.await {
        Ok(_) => {
            let summary_text = collect_task.await.unwrap_or_default();
            let trimmed = summary_text.trim();
            if !trimmed.is_empty() {
                history_manager.apply_compaction(trimmed, tail)?;
            }
        }
        Err(err) => {
            tracing::warn!("Failed to summarize history during compaction: {}", err);
        }
    }

    Ok(())
}

/// Records workflow response into HistoryManager based on the final execution result.
pub fn record_workflow_response(
    history_manager: &mut HistoryManager,
    result: &ModelResult,
    assistant_text: &str,
) {
    match result {
        ModelResult::Answer { text } => {
            let candidate = if !text.trim().is_empty() {
                text.clone()
            } else if !assistant_text.trim().is_empty() {
                assistant_text.to_string()
            } else {
                "Answered.".to_string()
            };
            let msg = crate::edits::protocol::structured::clean_raw_json_or_text(&candidate);
            let msg = if msg.trim().is_empty() {
                "Answered.".to_string()
            } else {
                msg
            };
            let _ = history_manager.record_response(msg, None, None, vec![]);
        }
        ModelResult::Edit {
            summary,
            edits,
            proposal,
            applied,
            commit_hash,
            ..
        } => {
            let msg = if let Some(m) = proposal
                .as_ref()
                .map(|p| p.message.trim())
                .filter(|m| !m.is_empty())
            {
                m.to_string()
            } else if !assistant_text.trim().is_empty() {
                crate::edits::protocol::structured::clean_raw_json_or_text(assistant_text)
            } else {
                summary.clone()
            };

            let mut seen = std::collections::HashSet::new();
            let mut files = Vec::new();
            for edit in edits {
                let entry = match edit {
                    EditOperation::Create { path, .. } => format!("[NEW] {}", path),
                    EditOperation::Delete { path } => format!("[DEL] {}", path),
                    EditOperation::Replace { path, .. } => format!("[EDIT] {}", path),
                    EditOperation::Overwrite { path, .. } => format!("[OVERWRITE] {}", path),
                    EditOperation::Move { from, to } => format!("[MOVE] {} -> {}", from, to),
                };
                if seen.insert(entry.clone()) {
                    files.push(entry);
                }
            }

            let commit = if *applied {
                commit_hash.clone()
            } else {
                None
            };

            let _ = history_manager.record_response(
                &msg,
                commit,
                Some(summary.clone()),
                files,
            );
        }
    }
}

/// Unified execution pipeline for context discovery, model reasoning, streaming,
/// patch staging, and auto-repair retry loop with explicit budgets.
#[allow(clippy::too_many_arguments)]
pub async fn execute_edit_pipeline(
    prompt: &str,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn EditProtocol,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    options: &WorkflowOptions,
) -> anyhow::Result<ParsedPipelineOutput> {
    let marker = generate_turn_marker();
    let marked_protocol = protocol.with_turn_marker(&marker);
    let protocol: &dyn EditProtocol = marked_protocol.as_deref().unwrap_or(protocol);

    let mut turn_detected_language: Option<String> = None;

    let _ = stream_tx
        .send(StreamEvent::TurnPhase {
            phase: tauqe_protocol::TurnPhase::Proposal,
            round: Some(1),
            max_rounds: None,
            detail: None,
        })
        .await;

    // 1. Initial attempt
    let mut pipeline_out = execute_edit_pipeline_step(
        prompt,
        provider,
        model,
        context_manager,
        history_manager,
        protocol,
        workflow_name,
        stream_tx.clone(),
        cancel_rx.clone(),
        options,
        StepParams {
            record_turn: true,
            emit_done: false,
            staged_state: None,
            target_language: turn_detected_language.as_deref(),
        },
    )
    .await?;

    if pipeline_out.is_cancelled {
        let _ = stream_tx.send(StreamEvent::Done).await;
        return Ok(pipeline_out);
    }

    if let Some(lang) = protocol.parse_user_language(&pipeline_out.assistant_text) {
        turn_detected_language = Some(lang);
    }
    pipeline_out.detected_language = turn_detected_language.clone();

    let repo_root = context_manager.repo_root().to_path_buf();

    // Discovery loop: the model may ask for files from the repo map before proposing edits.
    let mut discovery_round = 0;
    let mut round_prompt = prompt.to_string();
    let mut provided_docs: std::collections::HashSet<&'static str> =
        std::collections::HashSet::new();
    while discovery_round < options.max_discovery_rounds
        && !has_proposed_edits(&pipeline_out.parsed_result)
    {
        let requests = protocol.parse_context_requests(&pipeline_out.assistant_text);
        let mut new_topics: Vec<&'static str> = Vec::new();
        for topic in protocol.parse_doc_requests(&pipeline_out.assistant_text) {
            let topic = crate::docs::resolve_topic(&topic);
            if provided_docs.insert(topic) {
                new_topics.push(topic);
            }
        }
        if requests.is_empty() && new_topics.is_empty() {
            break;
        }
        let added = if requests.is_empty() {
            Vec::new()
        } else {
            let available_files =
                crate::git::list_repository_files(Some(&repo_root)).unwrap_or_default();
            apply_context_requests(
                context_manager,
                &requests,
                &available_files,
                options.max_auto_files_per_round,
            )
        };
        if added.is_empty() && new_topics.is_empty() {
            break;
        }
        discovery_round += 1;
        let _ = stream_tx
            .send(StreamEvent::TurnPhase {
                phase: tauqe_protocol::TurnPhase::Discovery,
                round: Some(discovery_round),
                max_rounds: Some(options.max_discovery_rounds),
                detail: Some(format!("Discovery round {}", discovery_round)),
            })
            .await;
        tracing::info!(
            "Discovery round {}: added {} file(s) to auto context: [{}]; documentation topics: [{}]",
            discovery_round,
            added.len(),
            added.join(", "),
            new_topics.join(", ")
        );
        for topic in &new_topics {
            round_prompt.push_str(&format!(
                "\n\n<system_documentation topic=\"{}\">\n{}\n</system_documentation>",
                topic,
                crate::docs::get_documentation(topic)
            ));
        }

        let ctx_state = context_manager.get_state();
        let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;

        let prev_text = std::mem::take(&mut pipeline_out.assistant_text);
        let mut notes = Vec::new();
        if !added.is_empty() {
            notes.push(format!(
                "Added {} file(s) to context: {}",
                added.len(),
                added.join(", ")
            ));
        }
        if !new_topics.is_empty() {
            notes.push(format!("Loaded documentation: {}", new_topics.join(", ")));
        }
        let round_banner = format!(
            "\n\n---\n**[Round {}]** {}\n\n",
            discovery_round + 1,
            notes.join("; ")
        );
        let _ = stream_tx.send(StreamEvent::TextDelta(round_banner.clone())).await;

        let _ = stream_tx
            .send(StreamEvent::TurnPhase {
                phase: tauqe_protocol::TurnPhase::Proposal,
                round: Some(discovery_round + 1),
                max_rounds: None,
                detail: None,
            })
            .await;

        pipeline_out = execute_edit_pipeline_step(
            &round_prompt,
            provider,
            model,
            context_manager,
            history_manager,
            protocol,
            workflow_name,
            stream_tx.clone(),
            cancel_rx.clone(),
            options,
            StepParams {
                record_turn: false,
                emit_done: false,
                staged_state: None,
                target_language: turn_detected_language.as_deref(),
            },
        )
        .await?;

        if let Some(lang) = protocol.parse_user_language(&pipeline_out.assistant_text) {
            turn_detected_language = Some(lang);
        }
        pipeline_out.detected_language = turn_detected_language.clone();

        if !prev_text.trim().is_empty() {
            let combined = format!("{}{}", prev_text.trim_end(), round_banner);
            if pipeline_out.assistant_text.trim().is_empty() {
                pipeline_out.assistant_text = combined;
            } else {
                pipeline_out.assistant_text = format!(
                    "{}{}",
                    combined,
                    pipeline_out.assistant_text.trim_start()
                );
            }
        }

        if pipeline_out.is_cancelled {
            let _ = stream_tx.send(StreamEvent::Done).await;
            return Ok(pipeline_out);
        }
    }

    if discovery_round >= options.max_discovery_rounds && !has_proposed_edits(&pipeline_out.parsed_result) {
        let remaining_requests = protocol.parse_context_requests(&pipeline_out.assistant_text);
        if !remaining_requests.is_empty() {
            let requested_paths: Vec<String> = remaining_requests.into_iter().map(|r| r.path).collect();
            let warn_msg = format!(
                "Reached maximum context discovery rounds limit ({}). The model requested more files: [{}], but the limit was reached. You can add them to context manually or increase `max_discovery_rounds` in config.",
                options.max_discovery_rounds,
                requested_paths.join(", ")
            );
            tracing::warn!("{}", warn_msg);
            let _ = stream_tx.send(StreamEvent::TextDelta(format!("\n\n⚠️ {}", warn_msg))).await;
            if pipeline_out.assistant_text.trim().is_empty() {
                pipeline_out.assistant_text = warn_msg.clone();
            } else {
                pipeline_out.assistant_text = format!("{}\n\n⚠️ {}", pipeline_out.assistant_text.trim(), warn_msg);
            }
            if let ModelResult::Answer { ref mut text } = pipeline_out.parsed_result {
                if text.trim().is_empty() {
                    *text = warn_msg;
                } else {
                    *text = format!("{}\n\n⚠️ {}", text.trim(), warn_msg);
                }
            }
        }
    }

    let (mut current_summary, mut current_edits, initial_proposal) = match &pipeline_out.parsed_result {
        ModelResult::Edit {
            summary,
            edits,
            proposal,
            error: None,
            ..
        } if !edits.is_empty() => (summary.clone(), edits.clone(), proposal.clone()),
        _ => {
            let _ = stream_tx.send(StreamEvent::Done).await;
            return Ok(pipeline_out);
        }
    };

    let mut staged_state: Option<StagedEditsState> = None;
    let mut cumulative_successful_edits: Vec<EditOperation> = Vec::new();
    let mut succeeded_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_failed_errors: std::collections::HashMap<String, EditError> =
        std::collections::HashMap::new();
    let mut attempt = 0;

    let _ = stream_tx
        .send(StreamEvent::TurnPhase {
            phase: tauqe_protocol::TurnPhase::Staging,
            round: Some(1),
            max_rounds: Some(options.max_retries),
            detail: None,
        })
        .await;

    loop {
        let stage_res = stage_and_validate_edits(
            &repo_root,
            context_manager,
            &current_edits,
            staged_state.as_ref(),
        );

        staged_state = Some(stage_res.staged_state);
        succeeded_files.extend(stage_res.successful_paths);
        cumulative_successful_edits.extend(stage_res.successful_edits);

        for path in &succeeded_files {
            last_failed_errors.remove(path);
        }
        for failed in stage_res.failed_files {
            last_failed_errors.insert(failed.path, failed.error);
        }

        if last_failed_errors.is_empty() {
            let _ = stream_tx.send(StreamEvent::Done).await;
            if cumulative_successful_edits.is_empty() {
                return Ok(ParsedPipelineOutput {
                    assistant_text: pipeline_out.assistant_text.clone(),
                    parsed_result: ModelResult::Answer {
                        text: pipeline_out.assistant_text,
                    },
                    is_cancelled: false,
                    detected_language: turn_detected_language,
                });
            }
            return Ok(ParsedPipelineOutput {
                assistant_text: pipeline_out.assistant_text,
                parsed_result: ModelResult::Edit {
                    summary: current_summary,
                    edits: cumulative_successful_edits,
                    proposal: initial_proposal,
                    applied: false,
                    error: None,
                    changed_files: Vec::new(),
                    commit_hash: None,
                },
                is_cancelled: false,
                detected_language: turn_detected_language,
            });
        }

        if attempt >= options.max_retries {
            break;
        }

        attempt += 1;

        let _ = stream_tx
            .send(StreamEvent::TurnPhase {
                phase: tauqe_protocol::TurnPhase::Staging,
                round: Some(attempt),
                max_rounds: Some(options.max_retries),
                detail: Some(format!("Patch retry {}/{}", attempt, options.max_retries)),
            })
            .await;

        for (path, err) in &last_failed_errors {
            let reason = format_patch_retry_reason(err);
            let _ = stream_tx
                .send(StreamEvent::EditFileRetrying {
                    path: path.clone(),
                    attempt,
                    max_retries: options.max_retries,
                    reason,
                })
                .await;
        }

        let retry_banner = format!(
            "\n\n---\n**[Patch Retry {}/{}]** Correcting failed edits...\n\n",
            attempt, options.max_retries
        );
        let _ = stream_tx.send(StreamEvent::TextDelta(retry_banner.clone())).await;

        let repair_prompt = build_patch_retry_prompt(
            &succeeded_files,
            &last_failed_errors,
            turn_detected_language.as_deref(),
        );

        let step_out = execute_edit_pipeline_step(
            &repair_prompt,
            provider,
            model,
            context_manager,
            history_manager,
            protocol,
            workflow_name,
            stream_tx.clone(),
            cancel_rx.clone(),
            options,
            StepParams {
                record_turn: false,
                emit_done: false,
                staged_state: staged_state.as_ref(),
                target_language: turn_detected_language.as_deref(),
            },
        )
        .await?;

        if step_out.is_cancelled {
            let _ = stream_tx.send(StreamEvent::Done).await;
            return Ok(step_out);
        }

        if let Some(lang) = protocol.parse_user_language(&step_out.assistant_text) {
            turn_detected_language = Some(lang);
        }
        let step_assistant_text = protocol.clean_assistant_text(&step_out.assistant_text);

        let prev_assistant = std::mem::take(&mut pipeline_out.assistant_text);
        pipeline_out.assistant_text = format!(
            "{}{}{}",
            prev_assistant.trim_end(),
            retry_banner,
            step_assistant_text.trim_start()
        );

        match step_out.parsed_result {
            ModelResult::Edit { summary, edits, .. } => {
                if !summary.is_empty() {
                    current_summary = summary;
                }
                current_edits = edits;
            }
            ModelResult::Answer { .. } => {
                current_edits = Vec::new();
            }
        }

        retain_retried_failed_files(&mut last_failed_errors, context_manager, &current_edits);

        if let Some(ref mut st) = staged_state {
            prepare_staged_for_retry(
                st,
                &mut cumulative_successful_edits,
                &mut succeeded_files,
                context_manager,
                &current_edits,
            );
        }
    }

    let mut err_lines = Vec::new();
    let mut failed_sorted: Vec<(&String, &EditError)> = last_failed_errors.iter().collect();
    failed_sorted.sort_by_key(|(p, _)| *p);
    for (path, err) in failed_sorted {
        err_lines.push(format!("{}: {}", path, err));
    }
    let error_desc = format!(
        "Failed to apply edits after {} retries:\n{}",
        options.max_retries,
        err_lines.join("\n")
    );

    let _ = stream_tx.send(StreamEvent::Done).await;

    Ok(ParsedPipelineOutput {
        assistant_text: pipeline_out.assistant_text,
        parsed_result: ModelResult::Edit {
            summary: current_summary,
            edits: cumulative_successful_edits,
            proposal: initial_proposal,
            applied: false,
            error: Some(error_desc),
            changed_files: Vec::new(),
            commit_hash: None,
        },
        is_cancelled: false,
        detected_language: turn_detected_language,
    })
}

/// Executes a single atomic model interaction step with token budgets applied from `WorkflowOptions`.
#[allow(clippy::too_many_arguments)]
pub async fn execute_edit_pipeline_step<'a>(
    prompt: &str,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn EditProtocol,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    options: &WorkflowOptions,
    params: StepParams<'a>,
) -> anyhow::Result<ParsedPipelineOutput> {
    let repo_state = crate::git::get_repository_state(Some(context_manager.repo_root()));
    let ctx_state = context_manager.get_state();
    let mut context_files = context_manager.read_context_files();

    if let Some(staged) = params.staged_state {
        for (path, content) in &staged.staged_files {
            if let Some(f) = context_files.iter_mut().find(|cf| &cf.path == path) {
                f.content = content.clone();
            } else {
                context_files.push(ContextFileContent {
                    path: path.clone(),
                    access: ContextAccess::Editable,
                    content: content.clone(),
                });
            }
        }
        context_files.retain(|f| !staged.deleted_paths.contains(&f.path));
        context_files.sort_by(|a, b| a.path.cmp(&b.path));
    }

    if params.record_turn {
        let _ = compact_history_if_needed_with_budget(
            history_manager,
            provider,
            model,
            cancel_rx.clone(),
            options.history_budget_tokens,
            options.history_tail_turns,
        )
        .await;
        let context_paths: Vec<String> = ctx_state.items.iter().map(|it| it.path.clone()).collect();
        let _ = history_manager.record_turn(prompt, context_paths);
    }

    let history_tag = history_manager.format_history_tag().ok();

    let editable_paths: Vec<String> = context_files
        .iter()
        .filter(|f| f.access == ContextAccess::Editable)
        .map(|f| f.path.clone())
        .collect();

    let repo_root = context_manager.repo_root().to_path_buf();

    let available_files = crate::git::list_repository_files(Some(&repo_root)).unwrap_or_default();
    let context_paths: Vec<String> = context_files.iter().map(|f| f.path.clone()).collect();
    let repo_map = crate::repomap::generate_repo_map(
        &repo_root,
        &available_files,
        &context_paths,
        options.repomap_token_budget,
    )
    .ok()
    .filter(|m| !m.trim().is_empty());

    let mut assembly = PromptAssembly::new(
        Some(repo_state),
        ctx_state.revision,
        context_files,
        workflow_name,
        protocol.name(),
        history_tag,
        repo_map,
    );
    assembly.target_language = params.target_language.map(|s| s.to_string());

    let active_findings = crate::review::ReviewStorage::load_latest(&repo_root)
        .ok()
        .flatten()
        .map(|session| {
            session
                .items
                .into_iter()
                .filter(|it| it.is_checked)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assembly = assembly.with_active_review_findings(active_findings);

    let active_plan = crate::plan::storage::PlanStorage::load_active_id(&repo_root)
        .ok()
        .flatten()
        .and_then(|id| crate::plan::storage::PlanStorage::load_plan(&repo_root, &id).ok().flatten());
    assembly = assembly.with_active_plan(active_plan);

    let assembled_messages = assembly.assemble_chat_messages(prompt, protocol);

    let (llm_tx, mut llm_rx) = mpsc::channel::<StreamEvent>(100);
    let response_format = protocol.response_format(&editable_paths);

    let client_call = provider.stream_chat(
        &model.name,
        assembled_messages.clone(),
        response_format,
        llm_tx,
        cancel_rx.clone(),
    );

    let editable_paths_for_filter = editable_paths.clone();
    let staged_for_filter = params
        .staged_state
        .map(|s| s.staged_files.clone())
        .unwrap_or_default();
    let forward_stream_tx = stream_tx.clone();
    let mut stream_filter = protocol.create_stream_filter(
        editable_paths_for_filter,
        repo_root,
        staged_for_filter,
    );
    let has_emitted_edits = Arc::new(AtomicBool::new(false));
    let has_emitted_edits_forward = has_emitted_edits.clone();

    let forward_task = tokio::spawn(async move {
        let mut assistant_text = String::new();
        let mut cancelled = false;

        while let Some(event) = llm_rx.recv().await {
            match event {
                StreamEvent::TextDelta(ref delta) => {
                    assistant_text.push_str(delta);
                    let filtered_events = stream_filter.push_chunk(delta);
                    for ev in filtered_events {
                        if matches!(
                            ev,
                            StreamEvent::EditStarted
                                | StreamEvent::EditFileStarted { .. }
                                | StreamEvent::EditHunk { .. }
                        ) {
                            has_emitted_edits_forward.store(true, Ordering::SeqCst);
                        }
                        if forward_stream_tx.send(ev).await.is_err() {
                            break;
                        }
                    }
                }
                StreamEvent::Cancelled => {
                    cancelled = true;
                    let _ = forward_stream_tx.send(StreamEvent::Cancelled).await;
                }
                StreamEvent::Done => {}
                other => {
                    if matches!(
                        other,
                        StreamEvent::EditStarted
                            | StreamEvent::EditFileStarted { .. }
                            | StreamEvent::EditHunk { .. }
                    ) {
                        has_emitted_edits_forward.store(true, Ordering::SeqCst);
                    }
                    if forward_stream_tx.send(other).await.is_err() {
                        break;
                    }
                }
            }
        }

        for ev in stream_filter.finish() {
            if matches!(
                ev,
                StreamEvent::EditStarted
                    | StreamEvent::EditFileStarted { .. }
                    | StreamEvent::EditHunk { .. }
            ) {
                has_emitted_edits_forward.store(true, Ordering::SeqCst);
            }
            let _ = forward_stream_tx.send(ev).await;
        }

        (assistant_text, cancelled)
    });

    let client_res = client_call.await;
    let (assistant_text, cancelled) = forward_task.await.unwrap_or_default();

    client_res?;

    if cancelled {
        return Ok(ParsedPipelineOutput {
            assistant_text: assistant_text.clone(),
            parsed_result: ModelResult::Answer {
                text: assistant_text,
            },
            is_cancelled: true,
            detected_language: params.target_language.map(|s| s.to_string()),
        });
    }

    let parsed_result = protocol.parse_output(&assistant_text, &editable_paths);

    let parsed_result = if has_emitted_edits.load(Ordering::SeqCst) {
        match parsed_result {
            ModelResult::Answer { .. } => ModelResult::Edit {
                summary: "Edits could not be finalized".to_string(),
                edits: Vec::new(),
                proposal: None,
                applied: false,
                error: Some(
                    "Edits were streamed but the final model response could not be parsed into a valid edit proposal"
                        .to_string(),
                ),
                changed_files: Vec::new(),
                commit_hash: None,
            },
            other => other,
        }
    } else {
        parsed_result
    };

    if !has_emitted_edits.load(Ordering::SeqCst) {
        if let ModelResult::Edit { ref edits, .. } = parsed_result {
            emit_semantic_events_for_edits(edits, &stream_tx).await;
        }
    }

    if params.emit_done {
        let _ = stream_tx.send(StreamEvent::Done).await;
    }

    let detected_language = protocol.parse_user_language(&assistant_text)
        .or_else(|| params.target_language.map(|s| s.to_string()));

    Ok(ParsedPipelineOutput {
        assistant_text,
        parsed_result,
        is_cancelled: false,
        detected_language,
    })
}

async fn emit_semantic_events_for_edits(
    edits: &[EditOperation],
    stream_tx: &mpsc::Sender<StreamEvent>,
) {
    if edits.is_empty() {
        return;
    }
    let _ = stream_tx.send(StreamEvent::EditStarted).await;

    for (idx, edit) in edits.iter().enumerate() {
        match edit {
            EditOperation::Create { path, content } => {
                let _ = stream_tx
                    .send(StreamEvent::EditFileStarted {
                        path: path.clone(),
                        op_type: "create".to_string(),
                    })
                    .await;
                if !content.is_empty() {
                    let _ = stream_tx
                        .send(StreamEvent::EditHunk {
                            path: path.clone(),
                            hunk_index: idx,
                            old_text: String::new(),
                            new_text: content.clone(),
                        })
                        .await;
                }
                let _ = stream_tx
                    .send(StreamEvent::EditFileDone {
                        path: path.clone(),
                        status: "ok".to_string(),
                        error: None,
                        hunks_count: 1,
                    })
                    .await;
            }
            EditOperation::Overwrite { path, content } => {
                let _ = stream_tx
                    .send(StreamEvent::EditFileStarted {
                        path: path.clone(),
                        op_type: "overwrite".to_string(),
                    })
                    .await;
                let _ = stream_tx
                    .send(StreamEvent::EditHunk {
                        path: path.clone(),
                        hunk_index: idx,
                        old_text: String::new(),
                        new_text: content.clone(),
                    })
                    .await;
                let _ = stream_tx
                    .send(StreamEvent::EditFileDone {
                        path: path.clone(),
                        status: "ok".to_string(),
                        error: None,
                        hunks_count: 1,
                    })
                    .await;
            }
            EditOperation::Delete { path } => {
                let _ = stream_tx
                    .send(StreamEvent::EditFileStarted {
                        path: path.clone(),
                        op_type: "delete".to_string(),
                    })
                    .await;
                let _ = stream_tx
                    .send(StreamEvent::EditFileDone {
                        path: path.clone(),
                        status: "ok".to_string(),
                        error: None,
                        hunks_count: 0,
                    })
                    .await;
            }
            EditOperation::Replace {
                path,
                old_text,
                new_text,
            } => {
                let _ = stream_tx
                    .send(StreamEvent::EditFileStarted {
                        path: path.clone(),
                        op_type: "replace".to_string(),
                    })
                    .await;
                let _ = stream_tx
                    .send(StreamEvent::EditHunk {
                        path: path.clone(),
                        hunk_index: idx,
                        old_text: old_text.clone(),
                        new_text: new_text.clone(),
                    })
                    .await;
                let _ = stream_tx
                    .send(StreamEvent::EditFileDone {
                        path: path.clone(),
                        status: "ok".to_string(),
                        error: None,
                        hunks_count: 1,
                    })
                    .await;
            }
            EditOperation::Move { from, to } => {
                let display = format!("{} -> {}", from, to);
                let _ = stream_tx
                    .send(StreamEvent::EditFileStarted {
                        path: display.clone(),
                        op_type: "move".to_string(),
                    })
                    .await;
                let _ = stream_tx
                    .send(StreamEvent::EditFileDone {
                        path: display,
                        status: "ok".to_string(),
                        error: None,
                        hunks_count: 0,
                    })
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_workflow_response_structured_strips_edits() {
        let dir = tempfile::tempdir().unwrap();
        let mut hm = HistoryManager::new(dir.path().to_path_buf());
        let raw_json = r#"{
            "message": "Refactored module A",
            "changes": [
                {
                    "op": "replace",
                    "path": "src/a.rs",
                    "old_text": "fn old() {}",
                    "new_text": "fn new() {}",
                    "content": ""
                }
            ],
            "context_requests": [],
            "suggested_actions": []
        }"#;

        let edit_res = ModelResult::Edit {
            summary: "Refactored module A".to_string(),
            edits: vec![EditOperation::Replace {
                path: "src/a.rs".to_string(),
                old_text: "fn old() {}".to_string(),
                new_text: "fn new() {}".to_string(),
            }],
            proposal: None,
            applied: true,
            error: None,
            changed_files: vec!["src/a.rs".to_string()],
            commit_hash: Some("1234567".to_string()),
        };

        record_workflow_response(&mut hm, &edit_res, raw_json);

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            crate::history::entry::HistoryEntry::Response {
                message,
                commit,
                summary,
                files,
                ..
            } => {
                assert_eq!(message, "Refactored module A");
                assert_eq!(commit.as_deref(), Some("1234567"));
                assert_eq!(summary.as_deref(), Some("Refactored module A"));
                assert_eq!(files, &vec!["[EDIT] src/a.rs"]);
            }
            _ => panic!("Expected Response entry"),
        }
    }

    #[test]
    fn test_record_workflow_response_discovery_concatenation_no_raw_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut hm = HistoryManager::new(dir.path().to_path_buf());
        let raw_discovery = "{\"message\":\"Need file a.rs\",\"changes\":[],\"context_requests\":[\"editable:a.rs\"],\"suggested_actions\":[]}\n\n{\"message\":\"Updated file a.rs\",\"changes\":[{\"op\":\"replace\",\"path\":\"a.rs\",\"old_text\":\"old\",\"new_text\":\"new\",\"content\":\"\"}],\"context_requests\":[],\"suggested_actions\":[]}";

        let edit_res = ModelResult::Edit {
            summary: "Update file a.rs".to_string(),
            edits: vec![EditOperation::Replace {
                path: "a.rs".to_string(),
                old_text: "old".to_string(),
                new_text: "new".to_string(),
            }],
            proposal: None,
            applied: true,
            error: None,
            changed_files: vec!["a.rs".to_string()],
            commit_hash: Some("abcdef1".to_string()),
        };

        record_workflow_response(&mut hm, &edit_res, raw_discovery);

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            crate::history::entry::HistoryEntry::Response {
                message,
                files,
                ..
            } => {
                assert_eq!(message, "Need file a.rs\n\nUpdated file a.rs");
                assert_eq!(files, &vec!["[EDIT] a.rs"]);
                assert!(!message.contains("changes"));
                assert!(!message.contains("old_text"));
            }
            _ => panic!("Expected Response entry"),
        }
    }

    #[test]
    fn test_context_request_preserved_before_clean() {
        let proto = crate::edits::XmlEditProtocol;
        let raw = "I need files.\n<context_request path=\"crates/tui/src/markdown.rs\" access=\"read_only\" />\n";
        let reqs = proto.parse_context_requests(raw);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].path, "crates/tui/src/markdown.rs");

        let cleaned = proto.clean_assistant_text(raw);
        assert!(!cleaned.contains("<context_request"));
        assert!(proto.parse_context_requests(&cleaned).is_empty());
    }
}
