//! Safe execution of the project commands a proposal invokes.
//!
//! The model has no shell. It addresses one command from the closed allowlist a
//! project declares in its `[project]` section, and the harness decides whether
//! that command may run. This module owns that decision and the answer round: an
//! invocation is validated against the declaration, executed with literal
//! arguments in its own process group, and reported back through the turn
//! journal, so the next proposal round reasons about the real output instead of
//! the assumption that the command ran.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use tauqe_protocol::{EditOutcome, LlmCommand, ModelResult, ProjectConfig, TurnPhase};
use tokio::sync::watch;

use crate::escape::{escape_xml_attr, escape_xml_text};
use crate::providers::StreamEvent;
use crate::toolchain::ToolchainCheckResult;

use super::guard::{finalize_blocked_turn, finish_turn};
use super::proposal::{log_fsm_phase, merge_text, ProposalStep};
use super::{propose, FsmTurnOutcome, PhaseContext};

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
/// runs: an undeclared name, a broken reference chain, or an undeclared parameter
/// is answered with a diagnostic block instead of reaching a process. The reports
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

    // The turn froze its effective project configuration on the first proposal
    // round, so an invocation validates its declaration against that snapshot
    // instead of re-reading `tauqe.toml` and re-running a task runner dump.
    let project = ctx.project().await;
    let max_feedback_chars = ctx.options.max_feedback_chars;
    let mut blocks = Vec::new();
    for (name, args) in invocations {
        let answer = execute_invocation(
            ctx.repo_root,
            &project,
            ctx.cancel_rx,
            &name,
            &args,
            max_feedback_chars,
        )
        .await;
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
///
/// `max_feedback_chars` bounds each reported command stream, so how much output
/// the model reads is the project's decision rather than a compile-time ceiling.
async fn execute_invocation(
    repo_root: &Path,
    project: &ProjectConfig,
    cancel_rx: &watch::Receiver<bool>,
    name: &str,
    args: &Value,
    max_feedback_chars: usize,
) -> InvocationAnswer {
    let arguments = match named_arguments(args) {
        Ok(arguments) => arguments,
        Err(reason) => {
            return InvocationAnswer {
                block: render_rejected(name, &reason),
                cancelled: false,
            };
        }
    };

    match crate::toolchain::run_project_command_with_config(
        repo_root,
        project,
        name,
        &arguments,
        Some(cancel_rx),
    )
    .await
    {
        Ok(result) if result.is_cancelled => InvocationAnswer {
            block: String::new(),
            cancelled: true,
        },
        Ok(result) => InvocationAnswer {
            block: render_result(name, &result, max_feedback_chars),
            cancelled: false,
        },
        Err(reason) => InvocationAnswer {
            block: render_rejected(name, &reason),
            cancelled: false,
        },
    }
}

/// Reads the named parameter values of one invocation.
///
/// The invocation reduces to a map of names to single literal values, so the
/// declared parameter list of the command is the only thing that decides what
/// runs: a name the declaration does not carry is refused downstream, and no
/// value is ever split into several arguments. An absent argument object
/// supplies nothing, a scalar is rendered as text, and a structured value is
/// refused instead of being flattened into a command line.
fn named_arguments(args: &Value) -> Result<BTreeMap<String, String>, String> {
    match args {
        Value::Null => Ok(BTreeMap::new()),
        Value::Object(map) => {
            let mut arguments = BTreeMap::new();
            for (name, value) in map {
                let value = match value {
                    Value::String(text) => text.clone(),
                    Value::Number(number) => number.to_string(),
                    Value::Bool(flag) => flag.to_string(),
                    _ => {
                        return Err(format!(
                            "the parameter `{name}` carries a structured value; supply one \
                             literal value"
                        ));
                    }
                };
                arguments.insert(name.clone(), value);
            }
            Ok(arguments)
        }
        _ => Err(
            "the invocation carries no named argument object; supply parameters as named \
             attributes"
                .to_string(),
        ),
    }
}

/// Renders the report of an executed command as one journal block.
fn render_result(name: &str, result: &ToolchainCheckResult, max_feedback_chars: usize) -> String {
    let mut block = format!(
        "<project_command_feedback name=\"{}\" success=\"{}\">",
        escape_xml_attr(name),
        result.success
    );
    block.push_str(&format!(
        "\nexit status: {}",
        if result.success { "success" } else { "failure" }
    ));
    if let Some(stdout) = render_stream(&result.stdout, max_feedback_chars) {
        block.push_str(&format!("\nstdout:\n{}", stdout));
    }
    if let Some(stderr) = render_stream(&result.stderr, max_feedback_chars) {
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

/// Renders one command stream, truncating it at `max_feedback_chars`.
///
/// The journal travels inside the model context, so a command that prints a
/// whole build transcript is reported as its head with an explicit truncation
/// note instead of crowding out the task statement. The head is kept on a
/// Unicode scalar boundary, so a multi-byte character is never split and the
/// truncation note names exactly how much was dropped.
fn render_stream(text: &str, max_feedback_chars: usize) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let total = trimmed.chars().count();
    let head: String = trimmed.chars().take(max_feedback_chars).collect();
    let escaped = escape_xml_text(&head);
    if total > max_feedback_chars {
        Some(format!(
            "{}\n[{} more characters omitted]",
            escaped,
            total - max_feedback_chars
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
        abort: None,
        has_terminal_success: false,
        plan_saved: false,
        plan_errors: Vec::new(),
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
    // Only stale paths are dropped; the findings the investigation already
    // gathered stay in the auto layer for the developer's next turn.
    crate::blocking::run_blocking(|| {
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

    /// Feedback bound applied by tests that do not exercise truncation.
    fn default_feedback_chars() -> usize {
        crate::config::DEFAULT_MAX_FEEDBACK_CHARS
    }

    /// Creates a temporary repository with a `tauqe.toml` declaration.
    fn repo(config: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tauqe.toml"), config).unwrap();
        dir
    }

    /// Resolves the project configuration the declaration of `dir` describes.
    ///
    /// These tests exercise command execution outside a bounded turn, so they
    /// resolve the configuration the way a turn-free caller does.
    async fn project_of(dir: &Path) -> tauqe_protocol::ProjectConfig {
        crate::toolchain::effective_project_config(dir, None).await
    }

    #[tokio::test]
    async fn executes_a_declared_argument_free_command() {
        let dir = repo("[project.commands.greet]\ncommand = \"printf\"\nargs = [\"hello\"]\n");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let project = project_of(dir.path()).await;

        let answer = execute_invocation(
            dir.path(),
            &project,
            &cancel_rx,
            "greet",
            &Value::Null,
            default_feedback_chars(),
        )
        .await;

        assert!(!answer.cancelled);
        assert!(answer.block.contains("name=\"greet\""));
        assert!(answer.block.contains("success=\"true\""));
        assert!(answer.block.contains("hello"));
    }

    #[tokio::test]
    async fn rejects_an_undeclared_command() {
        let dir = repo("[project.commands.greet]\ncommand = \"printf\"\nargs = [\"hello\"]\n");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let project = project_of(dir.path()).await;

        let answer = execute_invocation(
            dir.path(),
            &project,
            &cancel_rx,
            "rm-rf",
            &Value::Null,
            default_feedback_chars(),
        )
        .await;

        assert!(!answer.cancelled);
        assert!(answer.block.contains("success=\"false\""));
        assert!(answer.block.contains("rejected:"));
        assert!(answer.block.contains("declares no command"));
    }

    #[tokio::test]
    async fn rejects_an_undeclared_parameter() {
        let dir = repo("[project.commands.greet]\ncommand = \"printf\"\nargs = [\"hello\"]\n");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let project = project_of(dir.path()).await;

        let args = serde_json::json!({ "path": "../escape" });
        let answer = execute_invocation(
            dir.path(),
            &project,
            &cancel_rx,
            "greet",
            &args,
            default_feedback_chars(),
        )
        .await;

        assert!(!answer.cancelled);
        assert!(answer.block.contains("success=\"false\""));
        assert!(answer.block.contains("rejected:"));
        assert!(answer.block.contains("declares no parameter"));
    }

    #[tokio::test]
    async fn binds_declared_parameters_and_fills_defaults() {
        let dir = repo(
            "[project.commands.greet]\ncommand = \"printf\"\nargs = [\"[%s] [%s]\"]\n\
             parameters = [{ name = \"who\" }, { name = \"count\", default = \"1\" }]\n",
        );
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let project = project_of(dir.path()).await;

        let args = serde_json::json!({ "who": "tauqe" });
        let answer = execute_invocation(
            dir.path(),
            &project,
            &cancel_rx,
            "greet",
            &args,
            default_feedback_chars(),
        )
        .await;

        assert!(!answer.cancelled);
        assert!(answer.block.contains("success=\"true\""));
        assert!(answer.block.contains("[tauqe] [1]"));
    }

    #[tokio::test]
    async fn rejects_a_missing_required_parameter_and_a_structured_value() {
        let dir = repo(
            "[project.commands.greet]\ncommand = \"printf\"\nargs = [\"[%s]\"]\n\
             parameters = [{ name = \"who\" }]\n",
        );
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let project = project_of(dir.path()).await;

        let missing = execute_invocation(
            dir.path(),
            &project,
            &cancel_rx,
            "greet",
            &Value::Null,
            default_feedback_chars(),
        )
        .await;
        assert!(!missing.cancelled);
        assert!(missing.block.contains("requires the parameter `who`"));

        let args = serde_json::json!({ "who": ["a", "b"] });
        let structured = execute_invocation(
            dir.path(),
            &project,
            &cancel_rx,
            "greet",
            &args,
            default_feedback_chars(),
        )
        .await;
        assert!(!structured.cancelled);
        assert!(structured.block.contains("structured value"));
    }

    #[tokio::test]
    async fn rejects_a_broken_reference_chain() {
        let dir = repo("[project.commands.broken]\ncommand = \"@missing\"\n");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let project = project_of(dir.path()).await;

        let answer = execute_invocation(
            dir.path(),
            &project,
            &cancel_rx,
            "broken",
            &Value::Null,
            default_feedback_chars(),
        )
        .await;

        assert!(!answer.cancelled);
        assert!(answer.block.contains("success=\"false\""));
        assert!(answer.block.contains("rejected:"));
    }

    #[test]
    fn render_stream_truncates_on_a_scalar_boundary() {
        let bound = default_feedback_chars();
        let short = render_stream("  output  ", bound).expect("non-empty output renders");
        assert_eq!(short, "output");
        assert_eq!(render_stream("   ", bound), None);

        let long: String = "я".repeat(bound + 5);
        let rendered = render_stream(&long, bound).expect("long output renders");
        assert!(rendered.contains("[5 more characters omitted]"));
        assert!(rendered.starts_with('я'));

        // The bound is configuration-driven, so a project widens or narrows how
        // much command output the model reads.
        let narrow = render_stream("abcdefgh", 3).expect("short bound still renders");
        assert!(narrow.starts_with("abc"));
        assert!(narrow.contains("[5 more characters omitted]"));
    }

    #[test]
    fn named_arguments_reads_one_literal_value_per_parameter() {
        assert!(named_arguments(&Value::Null)
            .expect("an absent object supplies nothing")
            .is_empty());
        assert!(named_arguments(&serde_json::json!({}))
            .expect("an empty object supplies nothing")
            .is_empty());

        // Scalars are rendered as text, so a structured decoder that carries a
        // number reaches the declaration as one literal value.
        let arguments =
            named_arguments(&serde_json::json!({ "n": 10, "follow": true, "q": "a b" }))
                .expect("scalar values are readable");
        assert_eq!(
            arguments,
            BTreeMap::from([
                ("follow".to_string(), "true".to_string()),
                ("n".to_string(), "10".to_string()),
                ("q".to_string(), "a b".to_string()),
            ])
        );

        // A structured value would have to be split or flattened, so it is
        // refused instead of being forwarded to a process.
        assert!(named_arguments(&serde_json::json!({ "n": [1, 2] })).is_err());
        assert!(named_arguments(&serde_json::json!({ "n": { "a": 1 } })).is_err());
        assert!(named_arguments(&serde_json::json!(["1"])).is_err());
        assert!(named_arguments(&serde_json::json!("text")).is_err());
    }
}
