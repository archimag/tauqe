//! Safe execution of the project commands a proposal invokes.
//!
//! The model has no shell. It addresses one command from the closed allowlist a
//! project declares in its `[project]` section, and the harness decides whether
//! that command may run. This module owns that decision and the answer round: an
//! invocation is validated against the declaration, executed with literal
//! arguments in its own process group, and reported back through the turn
//! journal, so the next proposal round reasons about the real output instead of
//! the assumption that the command ran.

use std::path::Path;

use serde_json::Value;
use tauqe_protocol::{EditOutcome, LlmCommand, ModelResult, TurnPhase};
use tokio::sync::watch;

use crate::escape::{escape_xml_attr, escape_xml_text};
use crate::providers::StreamEvent;
use crate::toolchain::ToolchainCheckResult;

use super::guard::{finalize_blocked_turn, finish_turn};
use super::proposal::{log_fsm_phase, merge_text, ProposalStep};
use super::{propose, FsmTurnOutcome, PhaseContext};

/// Upper bound on the characters of one command stream reported to the model.
///
/// The journal travels inside the model context, so a command that prints a
/// whole build transcript is reported as its head with an explicit truncation
/// note instead of crowding out the task statement.
const MAX_FEEDBACK_CHARS: usize = 4000;

/// Outcome of the project-command phase of one proposal round.
pub(super) enum CustomToolPhase {
    /// The round carried no invocation.
    Absent(Box<ProposalStep>),
    /// One invocation round ran and its answer round is carried.
    Executed(Box<ProposalStep>),
    /// The turn must terminate immediately with the carried outcome.
    Terminate(Box<FsmTurnOutcome>),
}

/// Answer to one invocation, as it enters the turn journal.
struct InvocationAnswer {
    /// Feedback block recorded for the invocation, empty when it was cancelled.
    block: String,
    /// Whether the invocation was interrupted before it finished.
    cancelled: bool,
}

/// Executes and answers the project-command invocations of one proposal round.
///
/// Every invocation is validated against the declared allowlist before anything
/// runs: an undeclared name, a broken reference chain, or free-form arguments are
/// answered with a diagnostic block instead of reaching a process. The reports
/// join the turn journal and one answer round is requested, so the model
/// continues from the real output.
pub(super) async fn run_custom_tool_phase(
    ctx: &mut PhaseContext<'_>,
    step: ProposalStep,
) -> anyhow::Result<CustomToolPhase> {
    let invocations: Vec<(String, Value)> = step
        .custom_tools()
        .iter()
        .filter_map(|command| match command {
            LlmCommand::CustomTool { name, args } => Some((name.clone(), args.clone())),
            _ => None,
        })
        .collect();
    if invocations.is_empty() {
        return Ok(CustomToolPhase::Absent(Box::new(step)));
    }

    // Project commands and discovery share one round budget: both widen what the
    // model knows and both cost a provider round trip, so a turn that keeps
    // asking for either without proposing a change is stopped deterministically.
    if ctx.rounds_used >= ctx.options.max_discovery_rounds {
        return Ok(CustomToolPhase::Terminate(Box::new(
            finish_budget_exhausted(ctx).await,
        )));
    }
    ctx.rounds_used += 1;
    let round = ctx.rounds_used;

    log_fsm_phase(
        ctx.workflow_name,
        ctx.model,
        ctx.intent.is_isolated_step(),
        TurnPhase::Discovery,
        Some(round),
        Some(ctx.options.max_discovery_rounds),
    );
    let _ = ctx
        .stream_tx
        .send(StreamEvent::TurnPhase {
            phase: TurnPhase::Discovery,
            round: Some(round),
            max_rounds: Some(ctx.options.max_discovery_rounds),
            detail: Some("Project command execution".to_string()),
        })
        .await;

    let mut blocks = Vec::new();
    for (name, args) in invocations {
        let answer = execute_invocation(ctx.repo_root, ctx.cancel_rx, &name, &args).await;
        if answer.cancelled {
            let _ = ctx.stream_tx.send(StreamEvent::Done).await;
            return Ok(CustomToolPhase::Terminate(Box::new(cancelled_turn(ctx))));
        }
        blocks.push(answer.block);
    }

    let banner = format!(
        "\n\n---\n**[Project Command Round {}]** Executed {} project command(s).\n\n",
        round,
        blocks.len()
    );
    let _ = ctx
        .stream_tx
        .send(StreamEvent::TextDelta(banner.clone()))
        .await;
    for block in &blocks {
        ctx.journal.push_block(block.as_str());
    }

    // The invocation output is answered through the journal, so the answer round
    // extends the previous request instead of reshaping the cached prefix.
    let round_prompt = ctx.journal.compose_user_prompt(ctx.prompt, None);
    let next = propose(
        ctx.provider,
        ctx.model,
        ctx.protocol,
        ctx.history_manager,
        &*ctx.context_manager,
        ctx.workflow_name,
        ctx.stream_tx,
        ctx.cancel_rx,
        ctx.options,
        &mut *ctx.input_cache,
        ctx.state,
        ctx.intent,
        &round_prompt,
        None,
    )
    .await?;

    if next.is_cancelled {
        let _ = ctx.stream_tx.send(StreamEvent::Done).await;
        return Ok(CustomToolPhase::Terminate(Box::new(cancelled_turn(ctx))));
    }

    ctx.state.absorb(&next);
    ctx.accumulated_text = merge_text(
        std::mem::take(&mut ctx.accumulated_text),
        &banner,
        next.text(),
    );

    if let Some(reason) = ctx.state.blocked.clone() {
        let accumulated = std::mem::take(&mut ctx.accumulated_text);
        let outcome = finalize_blocked_turn(
            ctx.policy,
            ctx.context_manager,
            ctx.stream_tx,
            ctx.repo_root,
            reason,
            accumulated,
            &ctx.state.plans,
            ctx.plan_mode,
        )
        .await;
        return Ok(CustomToolPhase::Terminate(Box::new(outcome)));
    }

    Ok(CustomToolPhase::Executed(Box::new(next)))
}

/// Validates and runs one invocation, rendering the block recorded for it.
async fn execute_invocation(
    repo_root: &Path,
    cancel_rx: &watch::Receiver<bool>,
    name: &str,
    args: &Value,
) -> InvocationAnswer {
    if let Some(reason) = free_form_arguments(args) {
        return InvocationAnswer {
            block: render_rejected(name, &reason),
            cancelled: false,
        };
    }

    match crate::toolchain::run_project_command(repo_root, name, Some(cancel_rx)).await {
        Ok(result) if result.is_cancelled => InvocationAnswer {
            block: String::new(),
            cancelled: true,
        },
        Ok(result) => InvocationAnswer {
            block: render_result(name, &result),
            cancelled: false,
        },
        Err(reason) => InvocationAnswer {
            block: render_rejected(name, &reason),
            cancelled: false,
        },
    }
}

/// Rejects arguments a declared project command cannot accept.
///
/// A declaration is a closed command line: it names its own literal arguments, so
/// an invocation adds none. Anything else — a scalar, an array, or a non-empty
/// object — is refused instead of being forwarded to a process.
fn free_form_arguments(args: &Value) -> Option<String> {
    match args {
        Value::Null => None,
        Value::Object(map) if map.is_empty() => None,
        _ => Some(
            "the command declaration accepts no free-form arguments; leave `args` empty"
                .to_string(),
        ),
    }
}

/// Renders the report of an executed command as one journal block.
fn render_result(name: &str, result: &ToolchainCheckResult) -> String {
    let mut block = format!(
        "<project_command_feedback name=\"{}\" success=\"{}\">",
        escape_xml_attr(name),
        result.success
    );
    block.push_str(&format!(
        "\nexit status: {}",
        if result.success { "success" } else { "failure" }
    ));
    if let Some(stdout) = render_stream(&result.stdout) {
        block.push_str(&format!("\nstdout:\n{}", stdout));
    }
    if let Some(stderr) = render_stream(&result.stderr) {
        block.push_str(&format!("\nstderr:\n{}", stderr));
    }
    block.push_str("\n</project_command_feedback>");
    block
}

/// Renders the diagnostic block of an invocation that never reached a process.
fn render_rejected(name: &str, reason: &str) -> String {
    format!(
        "<project_command_feedback name=\"{}\" success=\"false\">\nrejected: {}\n</project_command_feedback>",
        escape_xml_attr(name),
        escape_xml_text(reason)
    )
}

/// Renders one command stream, truncating it at the feedback bound.
///
/// The head is kept on a Unicode scalar boundary, so a multi-byte character is
/// never split and the truncation note names exactly how much was dropped.
fn render_stream(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let total = trimmed.chars().count();
    let head: String = trimmed.chars().take(MAX_FEEDBACK_CHARS).collect();
    let escaped = escape_xml_text(&head);
    if total > MAX_FEEDBACK_CHARS {
        Some(format!(
            "{}\n[{} more characters omitted]",
            escaped,
            total - MAX_FEEDBACK_CHARS
        ))
    } else {
        Some(escaped)
    }
}

/// Builds the outcome of a turn interrupted while a project command ran.
fn cancelled_turn(ctx: &mut PhaseContext<'_>) -> FsmTurnOutcome {
    FsmTurnOutcome {
        assistant_text: std::mem::take(&mut ctx.accumulated_text),
        result: ModelResult::Answer {
            text: String::new(),
        },
        is_cancelled: true,
        blocked: None,
        has_terminal_success: false,
        plan_saved: false,
        skip_reason: None,
    }
}

/// Finalizes the turn when the shared investigation budget is exhausted and the
/// model keeps asking for more knowledge instead of proposing a change.
async fn finish_budget_exhausted(ctx: &mut PhaseContext<'_>) -> FsmTurnOutcome {
    tracing::warn!(
        workflow = ctx.workflow_name,
        model = %ctx.model,
        isolated_step = ctx.intent.is_isolated_step(),
        rounds = ctx.options.max_discovery_rounds,
        "shared investigation round budget exhausted"
    );
    crate::blocking::run_blocking(|| {
        ctx.context_manager.clear_auto();
        ctx.context_manager.prune_missing_files();
    });
    let _ = ctx.stream_tx.send(StreamEvent::Done).await;
    let result = ModelResult::Edit {
        summary: "Investigation budget exhausted".to_string(),
        edits: Vec::new(),
        proposal: None,
        outcome: EditOutcome {
            applied: false,
            error: Some(format!(
                "The model exhausted the shared investigation budget ({} rounds) without proposing a change.",
                ctx.options.max_discovery_rounds
            )),
            changed_files: Vec::new(),
            commit_hash: None,
        },
    };
    finish_turn(
        std::mem::take(&mut ctx.accumulated_text),
        result,
        ctx.repo_root,
        &ctx.state.plans,
        ctx.plan_mode,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a temporary repository with a `tauqe.toml` declaration.
    fn repo(config: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tauqe.toml"), config).unwrap();
        dir
    }

    #[tokio::test]
    async fn executes_a_declared_argument_free_command() {
        let dir = repo("[project.commands.greet]\ncommand = \"printf\"\nargs = [\"hello\"]\n");
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let answer = execute_invocation(dir.path(), &cancel_rx, "greet", &Value::Null).await;

        assert!(!answer.cancelled);
        assert!(answer.block.contains("name=\"greet\""));
        assert!(answer.block.contains("success=\"true\""));
        assert!(answer.block.contains("hello"));
    }

    #[tokio::test]
    async fn rejects_an_undeclared_command() {
        let dir = repo("[project.commands.greet]\ncommand = \"printf\"\nargs = [\"hello\"]\n");
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let answer = execute_invocation(dir.path(), &cancel_rx, "rm-rf", &Value::Null).await;

        assert!(!answer.cancelled);
        assert!(answer.block.contains("success=\"false\""));
        assert!(answer.block.contains("rejected:"));
        assert!(answer.block.contains("declares no command"));
    }

    #[tokio::test]
    async fn rejects_free_form_arguments() {
        let dir = repo("[project.commands.greet]\ncommand = \"printf\"\nargs = [\"hello\"]\n");
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let args = serde_json::json!({ "path": "../escape" });
        let answer = execute_invocation(dir.path(), &cancel_rx, "greet", &args).await;

        assert!(!answer.cancelled);
        assert!(answer.block.contains("rejected:"));
        assert!(answer.block.contains("leave `args` empty"));
    }

    #[tokio::test]
    async fn rejects_a_broken_reference_chain() {
        let dir = repo("[project.commands.broken]\ncommand = \"@missing\"\n");
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let answer = execute_invocation(dir.path(), &cancel_rx, "broken", &Value::Null).await;

        assert!(!answer.cancelled);
        assert!(answer.block.contains("success=\"false\""));
        assert!(answer.block.contains("rejected:"));
    }

    #[test]
    fn render_stream_truncates_on_a_scalar_boundary() {
        let short = render_stream("  output  ").expect("non-empty output renders");
        assert_eq!(short, "output");
        assert_eq!(render_stream("   "), None);

        let long: String = "я".repeat(MAX_FEEDBACK_CHARS + 5);
        let rendered = render_stream(&long).expect("long output renders");
        assert!(rendered.contains("[5 more characters omitted]"));
        assert!(rendered.starts_with('я'));
    }

    #[test]
    fn free_form_arguments_accepts_only_an_absent_argument_object() {
        assert!(free_form_arguments(&Value::Null).is_none());
        assert!(free_form_arguments(&serde_json::json!({})).is_none());
        assert!(free_form_arguments(&serde_json::json!({"a": 1})).is_some());
        assert!(free_form_arguments(&serde_json::json!([])).is_some());
        assert!(free_form_arguments(&serde_json::json!("text")).is_some());
    }
}
