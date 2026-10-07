use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::watch;
use tauqe_protocol::{ContextAccess, EditOperation, ModelResult};

use crate::context::{ContextFileContent, ContextManager};
use crate::edits::protocol::{generate_turn_marker, ContextRequest};
use crate::edits::stream::JsonStreamFilter;
use crate::edits::{
    stage_and_validate_edits, EditError, EditProtocol, StagedEditsState, XmlStreamFilter,
};
use crate::history::{HistoryManager, DEFAULT_HISTORY_BUDGET_TOKENS, DEFAULT_TAIL_TURNS_COUNT};
use crate::model::gateway::{ChatMessage, StreamEvent};
use crate::model::openrouter::OpenRouterClient;
use crate::prompt::PromptAssembly;

pub struct ParsedPipelineOutput {
    pub assistant_text: String,
    pub parsed_result: ModelResult,
    pub completed_tool_calls: Vec<crate::model::gateway::ToolCall>,
    pub is_cancelled: bool,
}

pub struct VerificationOutcome {
    pub success: bool,
    pub report: String,
}

/// Executes project verification pipeline according to model's `<verify .../>` request.
/// When `stream_output` is false (e.g. during automated edit validation), failure logs are not
/// emitted directly to the user's stream, giving the auto-healing loop a chance to fix them first.
pub async fn run_workflow_verification(
    repo_root: &std::path::Path,
    verify_req: &crate::edits::protocol::xml::VerifyRequest,
    stream_tx: &mpsc::Sender<StreamEvent>,
    stream_output: bool,
) -> VerificationOutcome {
    let target_str = match verify_req.target {
        crate::edits::protocol::xml::VerifyTarget::Check => "check",
        crate::edits::protocol::xml::VerifyTarget::Clippy => "clippy",
        crate::edits::protocol::xml::VerifyTarget::Test => "test",
        crate::edits::protocol::xml::VerifyTarget::All => "all",
    };

    if stream_output {
        let start_msg = format!("\n\n🔍 Running verification (`{}`)...\n", target_str);
        let _ = stream_tx.send(StreamEvent::TextDelta(start_msg)).await;
    }

    let (ok, results) = crate::toolchain::run_verification_pipeline(repo_root, &verify_req.target).await;

    if results.is_empty() {
        let note = "⚠️ No build/test toolchain detected in repository for verification.\n".to_string();
        if stream_output {
            let _ = stream_tx.send(StreamEvent::TextDelta(note.clone())).await;
        }
        return VerificationOutcome {
            success: true,
            report: note,
        };
    }

    let mut report = String::new();
    if ok {
        let success_msg = format!("✅ Code verification (`{}`) passed.\n", target_str);
        if stream_output || verify_req.on_success == crate::edits::protocol::xml::VerifyOnSuccess::Report {
            let _ = stream_tx.send(StreamEvent::TextDelta(success_msg.clone())).await;
        }

        if verify_req.on_success == crate::edits::protocol::xml::VerifyOnSuccess::Report {
            for r in &results {
                if !r.combined_output.trim().is_empty() {
                    report.push_str(&format!("`{}` output:\n```\n{}\n```\n\n", r.command, r.combined_output.trim()));
                }
            }
            if report.is_empty() {
                report = success_msg;
            } else {
                report = format!("{}\n{}", success_msg, report);
            }
        }
    } else {
        report.push_str(&format!("❌ Code verification (`{}`) failed:\n\n", target_str));
        for r in results.iter().filter(|r| !r.success) {
            report.push_str(&format!("Command `{}` failed:\n```\n{}\n```\n\n", r.command, r.combined_output.trim()));
        }
        if stream_output {
            let _ = stream_tx.send(StreamEvent::TextDelta(report.clone())).await;
        }
    }

    VerificationOutcome {
        success: ok,
        report,
    }
}

/// Constructs a prompt asking the model to fix compiler, clippy, or test failures.
pub fn build_verification_retry_prompt(
    report: &str,
    target_language: Option<&str>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("Code verification failed with the following errors:\n\n```\n");
    prompt.push_str(report.trim());
    prompt.push_str("\n```\n\nPlease fix the errors by proposing corrected edits.");
    if let Some(lang) = target_language {
        prompt.push_str(&format!(
            "\n\nNote: Formulate your explanations in {} (the language of the user's request), while conducting all reasoning strictly in English.",
            lang
        ));
    }
    prompt
}

/// Compacts session history if estimated tokens exceed the configured threshold.
pub async fn compact_history_if_needed(
    history_manager: &mut HistoryManager,
    client: &OpenRouterClient,
    model_name: &str,
    cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    if !history_manager.needs_compaction(DEFAULT_HISTORY_BUDGET_TOKENS)? {
        return Ok(());
    }

    let Some((head, tail)) = history_manager.prepare_compaction(DEFAULT_TAIL_TURNS_COUNT)? else {
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
    let client_call = client.stream_chat_with_tools(
        model_name,
        messages,
        None,
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

/// Extracts conversational "message" strings from structured JSON output blocks.
fn extract_json_messages(raw: &str) -> Vec<String> {
    let mut messages = Vec::new();
    let mut from = 0;
    while let Some(rel) = raw[from..].find('{') {
        let start = from + rel;
        if let Some(end) = balanced_object_end(raw, start) {
            let slice = &raw[start..end];
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(slice) {
                if let Some(msg) = val.get("message").and_then(|m| m.as_str()) {
                    let trimmed = msg.trim();
                    if !trimmed.is_empty()
                        && (val.get("changes").is_some()
                            || val.get("context_requests").is_some()
                            || val.get("suggested_actions").is_some())
                    {
                        messages.push(trimmed.to_string());
                    }
                }
            }
            from = end;
        } else {
            from = start + 1;
        }
    }
    messages
}

fn balanced_object_end(raw: &str, start: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            match b {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn is_raw_json(s: &str) -> bool {
    let trimmed = s.trim();
    if (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
    {
        serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
    } else if let Some(stripped) = trimmed.strip_prefix("```json") {
        if let Some(inner) = stripped.strip_suffix("```") {
            serde_json::from_str::<serde_json::Value>(inner.trim()).is_ok()
        } else {
            false
        }
    } else {
        false
    }
}

/// Records workflow response into HistoryManager based on the final execution result.
pub fn record_workflow_response(
    history_manager: &mut HistoryManager,
    result: &ModelResult,
    assistant_text: &str,
) {
    match result {
        ModelResult::Answer { text } => {
            let json_msgs = extract_json_messages(text);
            let candidate = if !json_msgs.is_empty() {
                json_msgs.join("\n\n")
            } else if !text.trim().is_empty() {
                text.clone()
            } else {
                let json_from_ast = extract_json_messages(assistant_text);
                if !json_from_ast.is_empty() {
                    json_from_ast.join("\n\n")
                } else if !assistant_text.trim().is_empty() {
                    assistant_text.to_string()
                } else {
                    "Answered.".to_string()
                }
            };
            let cleaned = crate::edits::protocol::xml::extract_conversational_text(&candidate);
            let msg = if !cleaned.trim().is_empty() && !is_raw_json(&cleaned) {
                cleaned
            } else if !candidate.trim().is_empty() && !is_raw_json(&candidate) {
                candidate
            } else {
                "Answered.".to_string()
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
            let json_msgs = extract_json_messages(assistant_text);
            let msg = if !json_msgs.is_empty() {
                json_msgs.join("\n\n")
            } else if let Some(m) = proposal
                .as_ref()
                .map(|p| p.message.trim())
                .filter(|m| !m.is_empty())
            {
                m.to_string()
            } else {
                let extracted =
                    crate::edits::protocol::xml::extract_conversational_text(assistant_text);
                if !extracted.trim().is_empty() && !is_raw_json(&extracted) {
                    extracted
                } else {
                    summary.clone()
                }
            };

            let mut seen = std::collections::HashSet::new();
            let mut files = Vec::new();
            for edit in edits {
                let entry = match edit {
                    EditOperation::Create { path, .. } => format!("[NEW] {}", path),
                    EditOperation::Delete { path } => format!("[DEL] {}", path),
                    EditOperation::Replace { path, .. } => format!("[EDIT] {}", path),
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

/// Filters out failed files that the model chose not to retry in the current set of edits.
pub fn retain_retried_failed_files(
    last_failed_errors: &mut std::collections::HashMap<String, EditError>,
    context_manager: &ContextManager,
    edits: &[EditOperation],
) {
    let mut retried_paths = std::collections::HashSet::new();
    for edit in edits {
        match edit {
            EditOperation::Replace { path, .. }
            | EditOperation::Create { path, .. }
            | EditOperation::Delete { path } => {
                let clean = context_manager.normalize_path(path).unwrap_or_else(|_| path.clone());
                retried_paths.insert(clean);
            }
            EditOperation::Move { from, to } => {
                let clean_from = context_manager.normalize_path(from).unwrap_or_else(|_| from.clone());
                let clean_to = context_manager.normalize_path(to).unwrap_or_else(|_| to.clone());
                retried_paths.insert(clean_from);
                retried_paths.insert(clean_to);
            }
        }
    }
    last_failed_errors.retain(|path, _| retried_paths.contains(path));
}

/// Evicts any files that are being re-edited in the current retry from previously staged state
/// and cumulative edits, so they can be freshly staged and validated against their clean base.
pub fn prepare_staged_for_retry(
    staged_state: &mut StagedEditsState,
    cumulative_edits: &mut Vec<EditOperation>,
    succeeded_files: &mut std::collections::HashSet<String>,
    context_manager: &ContextManager,
    new_edits: &[EditOperation],
) {
    let mut touched_paths = std::collections::HashSet::new();
    for edit in new_edits {
        match edit {
            EditOperation::Replace { path, .. }
            | EditOperation::Create { path, .. }
            | EditOperation::Delete { path } => {
                let clean = context_manager
                    .normalize_path(path)
                    .unwrap_or_else(|_| path.clone());
                touched_paths.insert(clean);
            }
            EditOperation::Move { from, to } => {
                let clean_from = context_manager
                    .normalize_path(from)
                    .unwrap_or_else(|_| from.clone());
                let clean_to = context_manager
                    .normalize_path(to)
                    .unwrap_or_else(|_| to.clone());
                touched_paths.insert(clean_from);
                touched_paths.insert(clean_to);
            }
        }
    }

    for path in &touched_paths {
        staged_state.staged_files.remove(path);
        staged_state.deleted_paths.remove(path);
        succeeded_files.remove(path);
    }

    cumulative_edits.retain(|op| {
        let op_path = match op {
            EditOperation::Replace { path, .. }
            | EditOperation::Create { path, .. }
            | EditOperation::Delete { path } => context_manager
                .normalize_path(path)
                .unwrap_or_else(|_| path.clone()),
            EditOperation::Move { from, to } => {
                let clean_from = context_manager
                    .normalize_path(from)
                    .unwrap_or_else(|_| from.clone());
                let clean_to = context_manager
                    .normalize_path(to)
                    .unwrap_or_else(|_| to.clone());
                if touched_paths.contains(&clean_from) || touched_paths.contains(&clean_to) {
                    return false;
                }
                return true;
            }
        };
        !touched_paths.contains(&op_path)
    });
}

fn access_satisfies(current: Option<ContextAccess>, requested: ContextAccess) -> bool {
    match current {
        Some(ContextAccess::Editable) => true,
        Some(ContextAccess::ReadOnly) => requested == ContextAccess::ReadOnly,
        None => false,
    }
}

/// Registers model-requested repository files in the Auto layer.
/// Returns the paths that actually widened the effective context.
pub fn apply_context_requests(
    context_manager: &mut ContextManager,
    requests: &[ContextRequest],
    available_files: &[String],
    max_auto_files: Option<usize>,
) -> Vec<String> {
    let mut added = Vec::new();
    for request in requests {
        if let Some(limit) = max_auto_files {
            if added.len() >= limit {
                break;
            }
        }
        let Ok(path) = context_manager.normalize_path(&request.path) else {
            continue;
        };
        if !available_files.contains(&path) {
            continue;
        }
        if access_satisfies(context_manager.effective_access_for(&path), request.access) {
            continue;
        }
        if context_manager.add_auto_file(&path, request.access).is_ok()
            && access_satisfies(context_manager.effective_access_for(&path), request.access)
        {
            added.push(path);
        }
    }
    added
}

fn has_proposed_edits(result: &ModelResult) -> bool {
    matches!(result, ModelResult::Edit { edits, .. } if !edits.is_empty())
}

/// Formats a human-readable failure reason for `EditFileRetrying` event and prompts.
pub fn format_patch_retry_reason(err: &EditError) -> String {
    match err {
        EditError::NoMatch { .. } => {
            "Search block not found (0 matches). Check indentation and line endings.".to_string()
        }
        EditError::AmbiguousMatch { count, .. } => {
            format!(
                "Search block matches {} times. Provide more unique context.",
                count
            )
        }
        other => other.to_string(),
    }
}

/// Constructs a specialized prompt asking the model to fix failed edit blocks.
pub fn build_patch_retry_prompt(
    succeeded_files: &std::collections::HashSet<String>,
    failed_files: &std::collections::HashMap<String, EditError>,
    target_language: Option<&str>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("Some of the proposed edits failed to apply.\n\n");

    if !succeeded_files.is_empty() {
        let mut succ_sorted: Vec<&String> = succeeded_files.iter().collect();
        succ_sorted.sort();
        prompt.push_str("Successfully applied edits for:\n");
        for path in succ_sorted {
            prompt.push_str(&format!("- {}\n", path));
        }
        prompt.push_str("(Do NOT regenerate changes for the above files; their updated versions are already present in <context>).\n\n");
    }

    prompt.push_str("Failed files and errors:\n");
    let mut failed_sorted: Vec<(&String, &EditError)> = failed_files.iter().collect();
    failed_sorted.sort_by_key(|(p, _)| *p);
    for (path, err) in failed_sorted {
        let explanation = match err {
            EditError::NoMatch { .. } => {
                "Search block not found (0 matches). Verify exact line content, indentation, and context."
            }
            EditError::AmbiguousMatch { count, .. } => {
                &format!(
                    "Search block matches {} times. Include more surrounding lines to make the match uniquely identifiable.",
                    count
                )
            }
            other => &other.to_string(),
        };
        prompt.push_str(&format!("- `{}`: {}\n", path, explanation));
    }

    prompt.push_str("\nPlease provide corrected edit blocks ONLY for the failed files listed above.");
    if let Some(lang) = target_language {
        prompt.push_str(&format!(
            "\n\nNote: Formulate your explanations in {} (the language of the user's request), while conducting all reasoning strictly in English.",
            lang
        ));
    }
    prompt
}

/// Common execution pipeline for streaming, XML filtering, parsing model response,
/// and automatically recovering from failed search/replace blocks using an in-memory retry loop.
#[allow(clippy::too_many_arguments)]
pub async fn execute_edit_pipeline(
    prompt: &str,
    client: &OpenRouterClient,
    model_name: &str,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn EditProtocol,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    max_retries: usize,
) -> anyhow::Result<ParsedPipelineOutput> {
    execute_edit_pipeline_full(
        prompt,
        client,
        model_name,
        context_manager,
        history_manager,
        protocol,
        workflow_name,
        stream_tx,
        cancel_rx,
        max_retries,
        crate::config::DEFAULT_MAX_DISCOVERY_ROUNDS,
        None,
        true,
    )
    .await
}

/// Execution pipeline with configurable turn recording, using default discovery settings.
#[allow(clippy::too_many_arguments)]
pub async fn execute_edit_pipeline_with_turn(
    prompt: &str,
    client: &OpenRouterClient,
    model_name: &str,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn EditProtocol,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    max_retries: usize,
    record_turn: bool,
) -> anyhow::Result<ParsedPipelineOutput> {
    execute_edit_pipeline_full(
        prompt,
        client,
        model_name,
        context_manager,
        history_manager,
        protocol,
        workflow_name,
        stream_tx,
        cancel_rx,
        max_retries,
        crate::config::DEFAULT_MAX_DISCOVERY_ROUNDS,
        None,
        record_turn,
    )
    .await
}

/// Full execution pipeline with configurable discovery rounds, auto file limits, and patch retry loop.
#[allow(clippy::too_many_arguments)]
pub async fn execute_edit_pipeline_full(
    prompt: &str,
    client: &OpenRouterClient,
    model_name: &str,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn EditProtocol,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    max_retries: usize,
    max_discovery_rounds: usize,
    max_auto_files_per_round: Option<usize>,
    record_turn: bool,
) -> anyhow::Result<ParsedPipelineOutput> {
    let marker = generate_turn_marker();
    let marked_protocol = protocol.with_turn_marker(&marker);
    let protocol: &dyn EditProtocol = marked_protocol.as_deref().unwrap_or(protocol);

    // 1. Initial attempt
    let mut pipeline_out = execute_edit_pipeline_step_opt(
        prompt,
        client,
        model_name,
        context_manager,
        history_manager,
        protocol,
        workflow_name,
        stream_tx.clone(),
        cancel_rx.clone(),
        record_turn,
        false,
        None,
    )
    .await?;

    if pipeline_out.is_cancelled {
        let _ = stream_tx.send(StreamEvent::Done).await;
        return Ok(pipeline_out);
    }

    if let Some(lang) = crate::edits::protocol::xml::extract_user_language(&pipeline_out.assistant_text) {
        history_manager.set_detected_language(lang);
    }
    pipeline_out.assistant_text = crate::edits::protocol::xml::strip_user_language_tags(&pipeline_out.assistant_text);

    let repo_root = context_manager.repo_root().to_path_buf();

    // Discovery loop: the model may ask for files from the repo map before proposing edits.
    // Fast path: a response with edits, or without any context request, needs no extra LLM call.
    let mut discovery_round = 0;
    while discovery_round < max_discovery_rounds
        && !has_proposed_edits(&pipeline_out.parsed_result)
    {
        let requests = protocol.parse_context_requests(&pipeline_out.assistant_text);
        if requests.is_empty() {
            break;
        }
        let available_files =
            crate::git::list_repository_files(Some(&repo_root)).unwrap_or_default();
        let added = apply_context_requests(
            context_manager,
            &requests,
            &available_files,
            max_auto_files_per_round,
        );
        if added.is_empty() {
            break;
        }
        discovery_round += 1;
        tracing::info!(
            "Discovery round {}: added {} file(s) to auto context: {}",
            discovery_round,
            added.len(),
            added.join(", ")
        );

        let ctx_state = context_manager.get_state();
        let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;

        let prev_text = std::mem::take(&mut pipeline_out.assistant_text);
        let round_banner = format!(
            "\n\n---\n**[Round {}]** Added {} file(s) to context: {}\n\n",
            discovery_round + 1,
            added.len(),
            added.join(", ")
        );
        let _ = stream_tx.send(StreamEvent::TextDelta(round_banner.clone())).await;

        pipeline_out = execute_edit_pipeline_step_opt(
            prompt,
            client,
            model_name,
            context_manager,
            history_manager,
            protocol,
            workflow_name,
            stream_tx.clone(),
            cancel_rx.clone(),
            false,
            false,
            None,
        )
        .await?;

        if let Some(lang) = crate::edits::protocol::xml::extract_user_language(&pipeline_out.assistant_text) {
            history_manager.set_detected_language(lang);
        }
        pipeline_out.assistant_text = crate::edits::protocol::xml::strip_user_language_tags(&pipeline_out.assistant_text);

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

    // If discovery exhausted without producing edits, check if model was still requesting files
    if discovery_round >= max_discovery_rounds && !has_proposed_edits(&pipeline_out.parsed_result) {
        let remaining_requests = protocol.parse_context_requests(&pipeline_out.assistant_text);
        if !remaining_requests.is_empty() {
            let requested_paths: Vec<String> = remaining_requests.into_iter().map(|r| r.path).collect();
            let warn_msg = format!(
                "Reached maximum context discovery rounds limit ({}). The model requested more files: [{}], but the limit was reached. You can add them to context manually or increase `max_discovery_rounds` in config.",
                max_discovery_rounds,
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

    // Check if initial output has edits to validate
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

        // Remove any files that succeeded in this attempt from failed list
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
                    completed_tool_calls: pipeline_out.completed_tool_calls,
                    is_cancelled: false,
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
                completed_tool_calls: pipeline_out.completed_tool_calls,
                is_cancelled: false,
            });
        }

        if attempt >= max_retries {
            break;
        }

        attempt += 1;

        // Emit EditFileRetrying for each failing file
        for (path, err) in &last_failed_errors {
            let reason = format_patch_retry_reason(err);
            let _ = stream_tx
                .send(StreamEvent::EditFileRetrying {
                    path: path.clone(),
                    attempt,
                    max_retries,
                    reason,
                })
                .await;
        }

        let retry_banner = format!(
            "\n\n---\n**[Patch Retry {}/{}]** Correcting failed edits...\n\n",
            attempt, max_retries
        );
        let _ = stream_tx.send(StreamEvent::TextDelta(retry_banner.clone())).await;

        let repair_prompt = build_patch_retry_prompt(
            &succeeded_files,
            &last_failed_errors,
            history_manager.detected_language(),
        );

        let step_out = execute_edit_pipeline_step_opt(
            &repair_prompt,
            client,
            model_name,
            context_manager,
            history_manager,
            protocol,
            workflow_name,
            stream_tx.clone(),
            cancel_rx.clone(),
            false,
            false,
            staged_state.as_ref(),
        )
        .await?;

        if step_out.is_cancelled {
            let _ = stream_tx.send(StreamEvent::Done).await;
            return Ok(step_out);
        }

        if let Some(lang) = crate::edits::protocol::xml::extract_user_language(&step_out.assistant_text) {
            history_manager.set_detected_language(lang);
        }
        let step_assistant_text = crate::edits::protocol::xml::strip_user_language_tags(&step_out.assistant_text);

        let prev_assistant = std::mem::take(&mut pipeline_out.assistant_text);
        pipeline_out.assistant_text = format!(
            "{}{}{}",
            prev_assistant.trim_end(),
            retry_banner,
            step_assistant_text.trim_start()
        );
        pipeline_out.completed_tool_calls = step_out.completed_tool_calls;

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

        // Failed files that the model chose not to touch in this retry are retracted/dropped
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

    // Exhausted retries with remaining errors
    let mut err_lines = Vec::new();
    let mut failed_sorted: Vec<(&String, &EditError)> = last_failed_errors.iter().collect();
    failed_sorted.sort_by_key(|(p, _)| *p);
    for (path, err) in failed_sorted {
        err_lines.push(format!("{}: {}", path, err));
    }
    let error_desc = format!(
        "Failed to apply edits after {} retries:\n{}",
        max_retries,
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
        completed_tool_calls: pipeline_out.completed_tool_calls,
        is_cancelled: false,
    })
}

/// Execution pipeline step with optional user turn recording (used by auto-healing loops).
#[allow(clippy::too_many_arguments)]
pub async fn execute_edit_pipeline_step(
    prompt: &str,
    client: &OpenRouterClient,
    model_name: &str,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn EditProtocol,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    record_turn: bool,
) -> anyhow::Result<ParsedPipelineOutput> {
    execute_edit_pipeline_step_opt(
        prompt,
        client,
        model_name,
        context_manager,
        history_manager,
        protocol,
        workflow_name,
        stream_tx,
        cancel_rx,
        record_turn,
        true,
        None,
    )
    .await
}

/// Internal pipeline step with control over emitting the final StreamEvent::Done and staged context overlay.
#[allow(clippy::too_many_arguments)]
pub async fn execute_edit_pipeline_step_opt(
    prompt: &str,
    client: &OpenRouterClient,
    model_name: &str,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn EditProtocol,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    record_turn: bool,
    emit_done: bool,
    staged_state: Option<&StagedEditsState>,
) -> anyhow::Result<ParsedPipelineOutput> {
    let repo_state = crate::git::get_repository_state(Some(context_manager.repo_root()));
    let ctx_state = context_manager.get_state();
    let mut context_files = context_manager.read_context_files();

    if let Some(staged) = staged_state {
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

    if record_turn {
        let _ = compact_history_if_needed(history_manager, client, model_name, cancel_rx.clone()).await;
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

    // Generate semantic repo map excluding files already in context
    let available_files = crate::git::list_repository_files(Some(&repo_root)).unwrap_or_default();
    let context_paths: Vec<String> = context_files.iter().map(|f| f.path.clone()).collect();
    let repo_map = crate::repomap::generate_repo_map(
        &repo_root,
        &available_files,
        &context_paths,
        crate::repomap::DEFAULT_REPOMAP_TOKEN_BUDGET,
    )
    .ok()
    .filter(|m| !m.trim().is_empty());

    // 1. Build prompt layering
    let mut assembly = PromptAssembly::new(
        Some(repo_state),
        ctx_state.revision,
        context_files,
        workflow_name,
        protocol.name(),
        history_tag,
        repo_map,
    );
    assembly.target_language = history_manager.detected_language().map(|s| s.to_string());

    let assembled_messages = assembly.assemble_chat_messages(prompt, protocol);

    // 2. Stream response from model
    let (llm_tx, mut llm_rx) = mpsc::channel::<StreamEvent>(100);

    // Code edits are no longer delivered via native tool calls; no protocol-specific tools.
    let tools: Option<Vec<crate::model::gateway::ToolDefinition>> = None;

    let response_format = protocol.response_format(&editable_paths);
    let is_structured = protocol.name() == "structured";

    let model_str = model_name.to_string();
    let client_call = client.stream_chat_with_tools(
        &model_str,
        assembled_messages.clone(),
        tools.clone(),
        response_format,
        llm_tx,
        cancel_rx.clone(),
    );

    let editable_paths_for_filter = editable_paths.clone();
    let staged_for_filter = staged_state
        .map(|s| s.staged_files.clone())
        .unwrap_or_default();
    let forward_stream_tx = stream_tx.clone();
    let turn_marker = protocol.turn_marker().map(|m| m.to_string());
    let has_emitted_edits = Arc::new(AtomicBool::new(false));
    let has_emitted_edits_forward = has_emitted_edits.clone();

    let forward_task = tokio::spawn(async move {
        let mut assistant_text = String::new();
        let mut cancelled = false;

        if is_structured {
            let mut json_filter = JsonStreamFilter::new(editable_paths_for_filter, repo_root)
                .with_staged_contents(staged_for_filter);
            while let Some(event) = llm_rx.recv().await {
                match event {
                    StreamEvent::TextDelta(ref delta) => {
                        assistant_text.push_str(delta);
                        let filtered_events = json_filter.push_chunk(delta);
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
            for ev in json_filter.finish() {
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
        } else {
            let mut stream_filter = XmlStreamFilter::new(editable_paths_for_filter, repo_root)
                .with_marker(turn_marker)
                .with_staged_contents(staged_for_filter);

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
                    StreamEvent::Done => {
                        // Suppress early Done from the first call so receiver does not terminate early
                    }
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

            // Flush remaining events
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
        }

        (assistant_text, cancelled)
    });

    let client_res = client_call.await;
    let (assistant_text, cancelled) = forward_task.await.unwrap_or_default();

    let completed_tool_calls = client_res?;

    if cancelled {
        return Ok(ParsedPipelineOutput {
            assistant_text: assistant_text.clone(),
            parsed_result: ModelResult::Answer {
                text: assistant_text,
            },
            completed_tool_calls,
            is_cancelled: true,
        });
    }

    // 3. Parse output according to active protocol
    let parsed_result = protocol.parse_output(&assistant_text, &editable_paths);

    // Invariant: once edit events were streamed to the client, the result must never
    // degrade to a plain Answer; surface it as a failed Edit so workflows/clients finalize it.
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

    // If streaming parser did not emit edit events in real time, emit fallback events now
    if !has_emitted_edits.load(Ordering::SeqCst) {
        if let ModelResult::Edit { ref edits, .. } = parsed_result {
            emit_semantic_events_for_edits(edits, &stream_tx).await;
        }
    }

    // Emit final Done event for this execution if requested
    if emit_done {
        let _ = stream_tx.send(StreamEvent::Done).await;
    }

    Ok(ParsedPipelineOutput {
        assistant_text,
        parsed_result,
        completed_tool_calls,
        is_cancelled: false,
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
    use std::collections::{HashMap, HashSet};

    #[test]
    fn test_apply_context_requests_adds_existing_files_to_auto_layer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["a.rs".to_string()];
        let requests = vec![
            ContextRequest {
                path: "a.rs".to_string(),
                access: ContextAccess::Editable,
            },
            ContextRequest {
                path: "missing.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        ];

        let added = apply_context_requests(&mut cm, &requests, &available, None);
        assert_eq!(added, vec!["a.rs".to_string()]);
        assert!(cm.is_editable("a.rs").unwrap());

        // Already satisfied: no further changes.
        assert!(apply_context_requests(&mut cm, &requests, &available, None).is_empty());

        cm.clear_auto();
        assert!(!cm.contains("a.rs").unwrap());
    }

    #[test]
    fn test_apply_context_requests_respects_max_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() {}").unwrap();
        let mut cm = ContextManager::new(dir.path().to_path_buf());
        let available = vec!["a.rs".to_string(), "b.rs".to_string()];
        let requests = vec![
            ContextRequest {
                path: "a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
            ContextRequest {
                path: "b.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        ];

        let added = apply_context_requests(&mut cm, &requests, &available, Some(1));
        assert_eq!(added.len(), 1);
        assert_eq!(added[0], "a.rs");
    }

    #[test]
    fn test_build_patch_retry_prompt_format() {
        let mut succeeded = HashSet::new();
        succeeded.insert("crates/core/src/lib.rs".to_string());

        let mut failed = HashMap::new();
        failed.insert(
            "crates/core/src/main.rs".to_string(),
            EditError::NoMatch {
                path: "crates/core/src/main.rs".to_string(),
            },
        );
        failed.insert(
            "crates/core/src/utils.rs".to_string(),
            EditError::AmbiguousMatch {
                path: "crates/core/src/utils.rs".to_string(),
                count: 3,
            },
        );

        let prompt = build_patch_retry_prompt(&succeeded, &failed, Some("Russian"));

        assert!(prompt.contains("Russian"));
        assert!(prompt.contains("Successfully applied edits for:"));
        assert!(prompt.contains("- crates/core/src/lib.rs"));
        assert!(prompt.contains("(Do NOT regenerate changes for the above files; their updated versions are already present in <context>)."));

        assert!(prompt.contains("Failed files and errors:"));
        assert!(prompt.contains("crates/core/src/main.rs"));
        assert!(prompt.contains("Search block not found (0 matches)"));

        assert!(prompt.contains("crates/core/src/utils.rs"));
        assert!(prompt.contains("Search block matches 3 times"));

        assert!(prompt.contains(
            "Please provide corrected edit blocks ONLY for the failed files listed above."
        ));
    }

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
    fn test_retain_retried_failed_files_drops_unretried_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cm = ContextManager::new(dir.path().to_path_buf());
        let mut failed = HashMap::new();
        failed.insert("a.rs".to_string(), EditError::ReadOnly("a.rs".to_string()));
        failed.insert("b.rs".to_string(), EditError::NoMatch { path: "b.rs".to_string() });

        // If retry only provides edits for b.rs, a.rs must be dropped from failed errors
        let edits = vec![EditOperation::Replace {
            path: "b.rs".to_string(),
            old_text: "old".to_string(),
            new_text: "new".to_string(),
        }];

        retain_retried_failed_files(&mut failed, &cm, &edits);
        assert!(!failed.contains_key("a.rs"));
        assert!(failed.contains_key("b.rs"));

        // If retry provides no edits at all (model retracted all remaining changes), all are cleared
        retain_retried_failed_files(&mut failed, &cm, &[]);
        assert!(failed.is_empty());
    }

    #[test]
    fn test_prepare_staged_for_retry_evicts_modified_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cm = ContextManager::new(dir.path().to_path_buf());
        let mut staged = StagedEditsState::default();
        staged
            .staged_files
            .insert("a.rs".to_string(), "staged a".to_string());
        staged
            .staged_files
            .insert("b.rs".to_string(), "staged b".to_string());

        let mut cumulative = vec![
            EditOperation::Replace {
                path: "a.rs".to_string(),
                old_text: "old a".to_string(),
                new_text: "staged a".to_string(),
            },
            EditOperation::Replace {
                path: "b.rs".to_string(),
                old_text: "old b".to_string(),
                new_text: "staged b".to_string(),
            },
        ];

        let mut succeeded = std::collections::HashSet::new();
        succeeded.insert("a.rs".to_string());
        succeeded.insert("b.rs".to_string());

        let retry_edits = vec![EditOperation::Replace {
            path: "a.rs".to_string(),
            old_text: "old a".to_string(),
            new_text: "fresh a".to_string(),
        }];

        prepare_staged_for_retry(
            &mut staged,
            &mut cumulative,
            &mut succeeded,
            &cm,
            &retry_edits,
        );

        assert!(!staged.staged_files.contains_key("a.rs"));
        assert!(staged.staged_files.contains_key("b.rs"));
        assert_eq!(cumulative.len(), 1);
        assert!(matches!(&cumulative[0], EditOperation::Replace { path, .. } if path == "b.rs"));
        assert!(!succeeded.contains("a.rs"));
        assert!(succeeded.contains("b.rs"));
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
}
