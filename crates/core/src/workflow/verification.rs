use std::path::Path;
use tokio::sync::{mpsc, watch};
use tauqe_protocol::{ContextAccess, EditProposal, ModelRef, ModelResult};

use crate::context::ContextManager;
use crate::edits::apply_edit_proposal;
use crate::edits::protocol::{EditProtocol, VerifyOnSuccess, VerifyRequest, VerifyTarget};
use crate::history::HistoryManager;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::common::{apply_context_requests, StepParams};
use crate::workflow::lifecycle::WorkflowOptions;
use crate::workflow::WorkflowExecutionResult;

pub struct VerificationOutcome {
    pub success: bool,
    pub report: String,
}

/// Executes project verification pipeline according to model's `<verify .../>` request.
pub async fn run_workflow_verification(
    repo_root: &Path,
    verify_req: &VerifyRequest,
    stream_tx: &mpsc::Sender<StreamEvent>,
    stream_output: bool,
) -> VerificationOutcome {
    let target_str = match verify_req.target {
        VerifyTarget::Check => "check",
        VerifyTarget::Clippy => "clippy",
        VerifyTarget::Test => "test",
        VerifyTarget::All => "all",
    };

    let _ = stream_tx
        .send(StreamEvent::ToolchainStarted {
            command: target_str.to_string(),
        })
        .await;

    if stream_output {
        let start_msg = format!("\n\n🔍 Running verification (`{}`)...\n", target_str);
        let _ = stream_tx.send(StreamEvent::TextDelta(start_msg)).await;
    }

    let (ok, results) = crate::toolchain::run_verification_pipeline(repo_root, &verify_req.target).await;

    let _ = stream_tx
        .send(StreamEvent::ToolchainFinished {
            command: target_str.to_string(),
            success: ok,
            message: if ok {
                Some(format!("Verification (`{}`) passed", target_str))
            } else {
                Some(format!("Verification (`{}`) failed", target_str))
            },
        })
        .await;

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
        if stream_output || verify_req.on_success == VerifyOnSuccess::Report {
            let _ = stream_tx.send(StreamEvent::TextDelta(success_msg.clone())).await;
        }

        if verify_req.on_success == VerifyOnSuccess::Report {
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

/// Extracts repository file paths mentioned in compilation, linter, or test failure output.
pub fn extract_error_paths(report: &str, available_files: &[String]) -> Vec<String> {
    let available_set: std::collections::HashSet<&str> =
        available_files.iter().map(|s| s.as_str()).collect();
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in report.lines() {
        for word in line.split_whitespace() {
            let trimmed = word.trim_matches(|c: char| {
                matches!(c, '`' | '\'' | '"' | '(' | ')' | '[' | ']' | ',' | ';' | '<' | '>')
            });
            let candidate = trimmed.strip_prefix("./").unwrap_or(trimmed);
            let path_part = candidate.split(':').next().unwrap_or("");
            if !path_part.is_empty()
                && available_set.contains(path_part)
                && seen.insert(path_part.to_string())
            {
                found.push(path_part.to_string());
            }
        }
    }
    found
}

fn auto_register_error_paths(
    context_manager: &mut ContextManager,
    report: &str,
    available_files: &[String],
) -> Vec<String> {
    let mut registered = Vec::new();
    let error_paths = extract_error_paths(report, available_files);
    for path in error_paths {
        if context_manager.effective_access_for(&path) != Some(ContextAccess::Editable)
            && context_manager.add_auto_file(&path, ContextAccess::Editable).is_ok()
        {
            registered.push(path);
        }
    }
    registered
}

/// Constructs a prompt asking the model to fix compiler, clippy, or test failures.
pub fn build_verification_retry_prompt(
    report: &str,
    target_language: Option<&str>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("Code verification failed with the following errors:\n\n```\n");
    prompt.push_str(report.trim());
    prompt.push_str("\n```\n\nPlease fix the errors by proposing corrected edits. If you need additional files to understand or fix the issue, you may request them using `<context_request path=\"...\" access=\"editable|read_only\" />`.");
    if let Some(lang) = target_language {
        prompt.push_str(&format!(
            "\n\nNote: Formulate your explanations in {} (the language of the user's request), while conducting all reasoning strictly in English.",
            lang
        ));
    }
    prompt
}

/// Helper that verifies an Answer's `<verify .../>` request and appends results to conversation text.
pub async fn verify_and_append_to_answer(
    repo_root: &Path,
    assistant_text: &mut String,
    mut text: String,
    stream_tx: &mpsc::Sender<StreamEvent>,
    protocol: &dyn EditProtocol,
) -> String {
    let verify_req = protocol.parse_verify_request(assistant_text);
    if let Some(req) = verify_req {
        let outcome = run_workflow_verification(repo_root, &req, stream_tx, true).await;
        if !outcome.report.trim().is_empty() {
            if text.trim().is_empty() {
                text = outcome.report;
            } else {
                text = format!("{}\n\n{}", text.trim(), outcome.report.trim());
            }
            *assistant_text = text.clone();
        }
    }
    text
}

pub enum VerificationHealingResult {
    Success(Option<VerificationOutcome>),
    Failed(VerificationOutcome),
    Cancelled(Box<WorkflowExecutionResult>),
}

/// Common auto-healing loop across workflows. If verification fails, iteratively asks the model
/// to propose fixes and invokes the provided `on_step_applied` hook on each successful step.
#[allow(clippy::too_many_arguments)]
pub async fn run_verification_healing_loop<F>(
    repo_root: &Path,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    protocol: &dyn crate::edits::EditProtocol,
    workflow_name: &str,
    stream_tx: &mpsc::Sender<StreamEvent>,
    cancel_rx: &watch::Receiver<bool>,
    options: &WorkflowOptions,
    mut verify_req: Option<VerifyRequest>,
    assistant_text: &mut String,
    turn_detected_language: &mut Option<String>,
    total_changed_files: &mut Vec<String>,
    mut on_step_applied: F,
) -> anyhow::Result<VerificationHealingResult>
where
    F: FnMut(&[String], usize),
{
    let Some(initial_req) = verify_req.clone() else {
        return Ok(VerificationHealingResult::Success(None));
    };

    let mut verify_outcome = run_workflow_verification(repo_root, &initial_req, stream_tx, false).await;

    let available_files = crate::git::list_repository_files(Some(repo_root)).unwrap_or_default();
    if !verify_outcome.success {
        let auto_added = auto_register_error_paths(context_manager, &verify_outcome.report, &available_files);
        if !auto_added.is_empty() {
            let ctx_state = context_manager.get_state();
            let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
            let notice = format!(
                "\n\n---\nAdded {} error file(s) to editable context: {}\n\n",
                auto_added.len(),
                auto_added.join(", ")
            );
            let _ = stream_tx.send(StreamEvent::TextDelta(notice.clone())).await;
            assistant_text.push_str(&notice);
        }
    }

    let mut verify_attempt = 0;
    let mut discovery_rounds = 0;
    while !verify_outcome.success && verify_attempt < options.max_retries {
        verify_attempt += 1;

        let _ = stream_tx
            .send(StreamEvent::TurnPhase {
                phase: tauqe_protocol::TurnPhase::Healing,
                round: Some(verify_attempt),
                max_rounds: Some(options.max_retries),
                detail: Some(format!("Healing retry {}/{}", verify_attempt, options.max_retries)),
            })
            .await;

        let retry_banner = format!(
            "\n\n---\n**[Verification Retry {}/{}]** Correcting verification failures...\n\n",
            verify_attempt, options.max_retries
        );
        let _ = stream_tx.send(StreamEvent::TextDelta(retry_banner.clone())).await;

        let retry_prompt = build_verification_retry_prompt(
            &verify_outcome.report,
            turn_detected_language.as_deref(),
        );

        let step_out = crate::workflow::common::execute_edit_pipeline_step(
            &retry_prompt,
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

        if step_out.is_cancelled {
            return Ok(VerificationHealingResult::Cancelled(Box::new(WorkflowExecutionResult {
                result: step_out.parsed_result,
                assistant_text: step_out.assistant_text,
            })));
        }

        if let Some(lang) = protocol.parse_user_language(&step_out.assistant_text) {
            *turn_detected_language = Some(lang);
        }
        let step_text = protocol.clean_assistant_text(&step_out.assistant_text);

        assistant_text.push_str(&retry_banner);
        assistant_text.push_str(step_text.trim_start());

        let context_requests = protocol.parse_context_requests(&step_out.assistant_text);
        let manually_added = if !context_requests.is_empty() {
            apply_context_requests(
                context_manager,
                &context_requests,
                &available_files,
                options.max_auto_files_per_round,
            )
        } else {
            Vec::new()
        };

        if !manually_added.is_empty() {
            let ctx_state = context_manager.get_state();
            let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
            let req_notice = format!(
                "\n\n---\nAdded {} requested file(s) to context: {}\n\n",
                manually_added.len(),
                manually_added.join(", ")
            );
            let _ = stream_tx.send(StreamEvent::TextDelta(req_notice.clone())).await;
            assistant_text.push_str(&req_notice);
        }

        match step_out.parsed_result {
            ModelResult::Edit {
                edits: retry_edits,
                summary: retry_summary,
                ..
            } if !retry_edits.is_empty() => {
                let retry_prop = EditProposal {
                    summary: retry_summary,
                    edits: retry_edits,
                };
                match apply_edit_proposal(repo_root, context_manager, &retry_prop) {
                    Ok(new_files) => {
                        for f in &new_files {
                            if !total_changed_files.contains(f) {
                                total_changed_files.push(f.clone());
                            }
                        }
                        on_step_applied(&new_files, verify_attempt);

                        if let Some(new_req) = protocol.parse_verify_request(&step_out.assistant_text) {
                            verify_req = Some(new_req);
                        }
                        if let Some(ref req) = verify_req {
                            let _ = stream_tx
                                .send(StreamEvent::TurnPhase {
                                    phase: tauqe_protocol::TurnPhase::Verification,
                                    round: Some(verify_attempt),
                                    max_rounds: Some(options.max_retries),
                                    detail: None,
                                })
                                .await;
                            verify_outcome = run_workflow_verification(repo_root, req, stream_tx, false).await;
                            if !verify_outcome.success {
                                let new_error_files = auto_register_error_paths(
                                    context_manager,
                                    &verify_outcome.report,
                                    &available_files,
                                );
                                if !new_error_files.is_empty() {
                                    let ctx_state = context_manager.get_state();
                                    let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
                                }
                            }
                        } else {
                            break;
                        }
                    }
                    Err(e) => {
                        verify_outcome = VerificationOutcome {
                            success: false,
                            report: format!("Failed to apply repair edits: {}", e),
                        };
                    }
                }
            }
            _ => {
                if !manually_added.is_empty() && discovery_rounds < options.max_discovery_rounds {
                    discovery_rounds += 1;
                    verify_attempt = verify_attempt.saturating_sub(1);
                    continue;
                }
                break;
            }
        }
    }

    if verify_outcome.success {
        if verify_outcome.report.contains("✅") {
            let _ = stream_tx
                .send(StreamEvent::TextDelta(format!("\n\n{}", verify_outcome.report.trim())))
                .await;
        }
        Ok(VerificationHealingResult::Success(Some(verify_outcome)))
    } else {
        let _ = stream_tx
            .send(StreamEvent::TextDelta(format!("\n\n{}", verify_outcome.report.trim())))
            .await;
        assistant_text.push_str("\n\n");
        assistant_text.push_str(verify_outcome.report.trim());
        Ok(VerificationHealingResult::Failed(verify_outcome))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_error_paths_from_clippy_report() {
        let report = r#"
error: used `unwrap()` on an `Option` value
  --> crates/protocol/src/context.rs:88:20
   |
88 |         let file = self.files.get(path).unwrap();
   |                    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error
"#;
        let available = vec![
            "crates/protocol/src/context.rs".to_string(),
            "crates/core/src/lib.rs".to_string(),
        ];
        let paths = extract_error_paths(report, &available);
        assert_eq!(paths, vec!["crates/protocol/src/context.rs"]);
    }

    #[test]
    fn test_extract_error_paths_ignores_unknown_files() {
        let report = "--> unknown/file.rs:10:5\n  crates/core/src/lib.rs:20:1";
        let available = vec!["crates/core/src/lib.rs".to_string()];
        let paths = extract_error_paths(report, &available);
        assert_eq!(paths, vec!["crates/core/src/lib.rs"]);
    }
}
