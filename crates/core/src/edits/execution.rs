//! One application and verification pass, independent of request scheduling.

use crate::config::project::ProjectConfig;
use std::collections::HashMap;
use std::path::Path;

use super::staging::StagingState;
use super::transaction::{ArtifactBaseline, DiskBackup, TurnTransaction};
use crate::context::ContextManager;
use crate::edits::EditFileOp;
use crate::edits::EditOperation;
use crate::edits::StagedEditsState;
use crate::edits::VerificationVerdict;
use crate::execution::{CancellationToken, EventSink, ExecutionError, ResponseEnvelope};
use crate::git::state::{changed_worktree_paths, snapshot_worktree, WorktreeSnapshot};
use crate::llm::LlmCapability;
use crate::llm::LlmCommand;
use crate::providers::StreamEvent;
use crate::toolchain::{
    run_post_hooks_with_config, run_verification_pipeline_with_config, VerificationStatus,
};
use crate::workflow::policy::ApplyPolicy;
use crate::workflow::verification::extract_error_paths;
use similar::{ChangeTag, TextDiff};

/// Capabilities processed by the atomic edits and verification contour.
pub const EDIT_CAPABILITIES: &[LlmCapability] = &[
    LlmCapability::EditReplace,
    LlmCapability::EditCreate,
    LlmCapability::EditDelete,
    LlmCapability::EditMove,
    LlmCapability::EditMoveDir,
    LlmCapability::EditOverwrite,
];
/// Bound dependencies of one disk-side pass; no request or turn state is available.
pub struct EditEnvironment<'a> {
    pub repo_root: &'a Path,
    pub context_manager: &'a mut ContextManager,
    pub policy: &'a mut dyn ApplyPolicy,
    pub cancel: &'a CancellationToken,
    pub events: &'a EventSink,
}

/// Turn-scoped state of the application and verification passes.
///
/// A turn applies and verifies its change set as many times as it spends healing
/// rounds, so the material those passes share belongs to the turn rather than to
/// one pass. The loop that drives the turn owns the value and hands every pass a
/// mutable reference to it: the virtual change set it converged on, the
/// operations that produced it, the paths the change set touched, and the
/// working-copy status the turn opened with, which is the baseline every
/// generated artifact is measured against.
#[derive(Debug)]
pub struct EditState {
    /// Project configuration the turn froze for its own execution.
    project: ProjectConfig,
    /// Virtual change set the staging attempts converged on.
    staged: StagedEditsState,
    /// Operations that converged, in change-set order.
    edit_batches: Vec<Vec<EditOperation>>,
    /// Paths the change set touched so far, in first-touch order.
    modified_paths: Vec<String>,
    /// Working-copy status used to detect external-command artifacts, refreshed
    /// after policy application so checkpoint commits are not counted as drift.
    initial_worktree: WorktreeSnapshot,
    /// Number of application and verification passes the turn spent.
    attempts: usize,
}

impl EditState {
    /// Creates the state the application passes of one turn share.
    pub fn new(project: ProjectConfig, initial_worktree: WorktreeSnapshot) -> Self {
        Self {
            project,
            staged: StagedEditsState::default(),
            edit_batches: Vec::new(),
            modified_paths: Vec::new(),
            initial_worktree,
            attempts: 0,
        }
    }

    /// Adopts the virtual change set the staging attempts of the turn converged on.
    pub fn adopt_staging(&mut self, staging: &StagingState) {
        self.staged = staging.staged_state().cloned().unwrap_or_default();
        self.edit_batches = staging.edit_batches().to_vec();
    }

    pub fn modified_paths(&self) -> &[String] {
        &self.modified_paths
    }

    /// Records the paths the change set touched, keeping the first-touch order.
    fn record_modified(&mut self, paths: impl IntoIterator<Item = String>) {
        for path in paths {
            if !self.modified_paths.contains(&path) {
                self.modified_paths.push(path);
            }
        }
    }
}

/// What one application and verification pass decided.
#[derive(Debug, Clone, PartialEq)]
pub enum EditStepVerdict {
    /// The change set reached the working tree and the toolchain accepted it.
    Passed(VerificationVerdict),
    /// The declared post-hook failed, so the code it generates never converged.
    HookFailed { report: String },
    /// The toolchain rejected the change set.
    VerificationFailed { report: String },
    /// The change set could not be written to the working tree at all.
    ApplyFailed { message: String },
}

/// Executes exactly one application and verification pass.
///
/// The step formats the virtual change set in memory, writes it to the working
/// tree, runs the declared post-hook and folds the artifacts it generated into
/// the change set, runs the deterministic toolchain, and emits the cumulative
/// diff of everything the turn has touched. It never requests a round of its
/// own: the report a failed verdict carries is the material a healing round is
/// built from, and that round belongs to the loop that drives the turn.
pub async fn execute_edit_step(
    ctx: &mut EditEnvironment<'_>,
    state: &mut EditState,
    transaction: &mut TurnTransaction,
) -> Result<EditStepVerdict, ExecutionError> {
    state.attempts += 1;

    let touched_paths = collect_staged_paths(&state.staged);
    if let Err(error) =
        crate::blocking::run_blocking(|| transaction.backup.record_paths(&touched_paths))
    {
        return Ok(EditStepVerdict::ApplyFailed {
            message: format!("{error:#}"),
        });
    }

    let batches: Vec<_> = state.edit_batches.iter().map(Vec::as_slice).collect();
    if let Err(err) = ctx
        .policy
        .apply_temporary_batches(ctx.repo_root, &state.project, ctx.context_manager, &batches)
        .await
    {
        return Ok(EditStepVerdict::ApplyFailed {
            message: err.to_string(),
        });
    }

    // Git application can capture pre-existing dirty files in a checkpoint.
    // Their disappearance from status is not a hook or verification artifact.
    // Compare external commands against the post-application status, while
    // keeping touched paths and transaction backups cumulative across healing.
    state.initial_worktree = crate::blocking::run_blocking(|| snapshot_worktree(ctx.repo_root));

    ctx.events
        .emit(StreamEvent::ContextChanged(ctx.context_manager.get_state()));
    state.record_modified(touched_paths);

    // Emit live cumulative diff against pre-turn baseline for all touched files
    emit_cumulative_diff(ctx, &transaction.backup);

    // 3. Run the declared post-hook and fold the artifacts it generated into the
    // change set, so verification and the final commit observe them.
    if let Some(verdict) = run_post_hook_pass(ctx, state, transaction).await {
        return Ok(verdict);
    }

    // 4. Run the deterministic toolchain verification.
    run_verification_pass(ctx, state, transaction).await
}

/// Runs the declared post-hook, reporting a typed verdict when it failed.
///
async fn run_post_hook_pass(
    ctx: &mut EditEnvironment<'_>,
    state: &mut EditState,
    transaction: &mut TurnTransaction,
) -> Option<EditStepVerdict> {
    state.project.post_hook.as_ref()?;
    let baseline = crate::blocking::run_blocking(|| ArtifactBaseline::capture(ctx.repo_root));

    let hook_res = run_post_hooks_with_config(
        ctx.repo_root,
        &state.project,
        &state.modified_paths,
        Some(&ctx.cancel.receiver()),
    )
    .await;

    record_command_artifacts(ctx, state, transaction, &baseline);
    match hook_res {
        Ok(resp) if !resp.success => {
            let hook_error = resp.message.unwrap_or_else(|| "Post-hook failed".into());
            Some(EditStepVerdict::HookFailed {
                report: format!("Project post-hook failed:\n```\n{}\n```\n", hook_error),
            })
        }
        Ok(_) => None,
        Err(err) => Some(EditStepVerdict::HookFailed {
            report: format!("Project post-hook execution error:\n```\n{}\n```\n", err),
        }),
    }
}

/// Runs the declared toolchain verification and reports its typed verdict.
///
async fn run_verification_pass(
    ctx: &mut EditEnvironment<'_>,
    state: &mut EditState,
    transaction: &mut TurnTransaction,
) -> Result<EditStepVerdict, ExecutionError> {
    let baseline = crate::blocking::run_blocking(|| ArtifactBaseline::capture(ctx.repo_root));
    ctx.events.emit(StreamEvent::ToolchainStarted {
        command: "verify".into(),
    });

    let (status, results) = run_verification_pipeline_with_config(
        ctx.repo_root,
        &state.project,
        &state.modified_paths,
        Some(&ctx.cancel.receiver()),
    )
    .await;

    record_command_artifacts(ctx, state, transaction, &baseline);
    if status == VerificationStatus::Cancelled || ctx.cancel.is_cancelled() {
        ctx.events.emit(StreamEvent::ToolchainFinished {
            command: "verify".into(),
            success: false,
            message: Some("Verification cancelled".into()),
        });
        return Err(ExecutionError::Cancelled);
    }

    if status.allows_commit() {
        ctx.events.emit(StreamEvent::ToolchainFinished {
            command: "verify".into(),
            success: true,
            message: Some("Verification passed".into()),
        });
        tracing::info!(
            repo_root = %ctx.repo_root.display(),
            attempt = state.attempts,
            files = state.modified_paths.len(),
            "project verification passed"
        );
        return Ok(EditStepVerdict::Passed(status.into()));
    }

    ctx.events.emit(StreamEvent::ToolchainFinished {
        command: "verify".into(),
        success: false,
        message: Some("Verification failed".into()),
    });
    tracing::warn!(
        repo_root = %ctx.repo_root.display(),
        attempt = state.attempts,
        files = state.modified_paths.len(),
        "project verification failed"
    );

    Ok(EditStepVerdict::VerificationFailed {
        report: format_verification_failure_report(&results),
    })
}

fn record_command_artifacts(
    ctx: &EditEnvironment<'_>,
    state: &mut EditState,
    transaction: &mut TurnTransaction,
    baseline: &ArtifactBaseline,
) {
    let (current, paths) = crate::blocking::run_blocking(|| {
        let current = snapshot_worktree(ctx.repo_root);
        let paths = changed_worktree_paths(&state.initial_worktree, &current);
        transaction.backup.record_artifacts(baseline, &paths);
        (current, paths)
    });
    state.initial_worktree = current;
    if !paths.is_empty() {
        state.record_modified(paths);
        emit_cumulative_diff(ctx, &transaction.backup);
    }
}

/// Returns the commit summary a round declared in its terminal success.
///
/// The success message is the model's own account of the change set, so it
/// becomes the summary of the final commit verbatim. A round that stated no
/// message yields `None`, and the caller keeps its own fallback label rather
/// than inventing a summary the model never wrote.
pub fn declared_commit_message(envelope: &ResponseEnvelope) -> Option<String> {
    envelope
        .commands()
        .iter()
        .find_map(|command| match command {
            LlmCommand::CommitMessage(command) => Some(command.commit_message.trim())
                .filter(|text| !text.is_empty())
                .map(str::to_string),
            _ => None,
        })
}

fn collect_staged_paths(staged: &StagedEditsState) -> Vec<String> {
    let mut paths = Vec::new();
    for p in staged.staged_files.keys() {
        paths.push(p.clone());
    }
    for p in &staged.deleted_paths {
        if !paths.contains(p) {
            paths.push(p.clone());
        }
    }
    paths
}

fn emit_cumulative_diff(ctx: &EditEnvironment<'_>, backup: &DiskBackup) {
    let root = ctx.repo_root.to_path_buf();
    let files = backup.known_files();
    let events =
        crate::blocking::run_blocking(move || compute_cumulative_diff_events(&root, &files));
    for ev in events {
        ctx.events.emit(ev);
    }
}

/// Shows only changes accepted by atomic patch staging.
pub(super) fn emit_staged_patch_diff(
    repo_root: &Path,
    sink: &EventSink,
    before: Option<&StagedEditsState>,
    result: &crate::edits::StageEditsResult,
) {
    let events = crate::blocking::run_blocking(|| {
        let originals = result
            .successful_paths
            .iter()
            .map(|path| {
                let original = if before.is_some_and(|state| state.deleted_paths.contains(path)) {
                    None
                } else {
                    before
                        .and_then(|state| state.staged_files.get(path))
                        .cloned()
                        .or_else(|| {
                            crate::context::verify_path_within_repo(repo_root, path).ok()?;
                            std::fs::read(repo_root.join(path)).ok()
                        })
                };
                (path.clone(), original)
            })
            .collect();
        compute_diff_events(&originals, |path| {
            result.staged_state.staged_files.get(path).cloned()
        })
    });
    for event in events {
        sink.emit(event);
    }
}

fn compute_cumulative_diff_events(
    root: &Path,
    backup_files: &HashMap<String, Option<Vec<u8>>>,
) -> Vec<StreamEvent> {
    compute_diff_events(backup_files, |path| std::fs::read(root.join(path)).ok())
}

fn compute_diff_events(
    backup_files: &HashMap<String, Option<Vec<u8>>>,
    current_bytes: impl Fn(&str) -> Option<Vec<u8>>,
) -> Vec<StreamEvent> {
    let mut paths: Vec<&String> = backup_files.keys().collect();
    paths.sort();

    if paths.is_empty() {
        return Vec::new();
    }

    let mut events = Vec::new();
    events.push(StreamEvent::EditStarted);

    for path in paths {
        let original = backup_files.get(path).and_then(|opt| opt.as_deref());
        let current = current_bytes(path);

        if original == current.as_deref() {
            continue;
        }

        let op_type = match (original, current.as_deref()) {
            (None, Some(_)) => EditFileOp::Create,
            (Some(_), None) => EditFileOp::Delete,
            _ => EditFileOp::Replace,
        };

        events.push(StreamEvent::EditFileStarted {
            path: path.clone(),
            op_type,
        });

        let old_text = original.and_then(|b| std::str::from_utf8(b).ok());
        let new_text = current.as_deref().and_then(|b| std::str::from_utf8(b).ok());

        let hunks = match (old_text, new_text) {
            (Some(old_str), Some(new_str)) => compute_file_hunks(path, old_str, new_str, op_type),
            (None, Some(new_str)) if op_type == EditFileOp::Create => {
                compute_file_hunks(path, "", new_str, op_type)
            }
            (Some(old_str), None) if op_type == EditFileOp::Delete => {
                compute_file_hunks(path, old_str, "", op_type)
            }
            _ => Vec::new(),
        };

        let hunks_count = hunks.len();
        events.extend(hunks);

        events.push(StreamEvent::EditFileDone {
            path: path.clone(),
            status: crate::edits::EditFileStatus::Ok,
            error: None,
            hunks_count,
        });
    }

    events
}

fn compute_file_hunks(
    path: &str,
    old_str: &str,
    new_str: &str,
    op_type: EditFileOp,
) -> Vec<StreamEvent> {
    if op_type == EditFileOp::Create {
        let new_lines = new_str.lines().count();
        return vec![StreamEvent::EditHunk {
            path: path.to_string(),
            hunk_index: 0,
            old_start: 0,
            old_lines: 0,
            new_start: if new_lines > 0 { 1 } else { 0 },
            new_lines,
            context_hint: None,
            old_text: String::new(),
            new_text: new_str.to_string(),
        }];
    }

    if op_type == EditFileOp::Delete {
        let old_lines = old_str.lines().count();
        return vec![StreamEvent::EditHunk {
            path: path.to_string(),
            hunk_index: 0,
            old_start: if old_lines > 0 { 1 } else { 0 },
            old_lines,
            new_start: 0,
            new_lines: 0,
            context_hint: None,
            old_text: old_str.to_string(),
            new_text: String::new(),
        }];
    }

    let diff = TextDiff::from_lines(old_str, new_str);
    let mut hunks = Vec::new();

    for (hunk_index, group) in diff.grouped_ops(3).into_iter().enumerate() {
        let Some(first_op) = group.first() else {
            continue;
        };
        let Some(last_op) = group.last() else {
            continue;
        };

        let old_start_idx = first_op.old_range().start;
        let old_end_idx = last_op.old_range().end;
        let new_start_idx = first_op.new_range().start;
        let new_end_idx = last_op.new_range().end;

        let old_lines_count = old_end_idx.saturating_sub(old_start_idx);
        let new_lines_count = new_end_idx.saturating_sub(new_start_idx);

        let old_start = if old_lines_count > 0 {
            old_start_idx + 1
        } else {
            0
        };
        let new_start = if new_lines_count > 0 {
            new_start_idx + 1
        } else {
            0
        };

        let mut old_chunk = String::new();
        let mut new_chunk = String::new();

        for op in &group {
            for change in diff.iter_changes(op) {
                match change.tag() {
                    ChangeTag::Equal => {
                        old_chunk.push_str(change.value());
                        new_chunk.push_str(change.value());
                    }
                    ChangeTag::Delete => {
                        old_chunk.push_str(change.value());
                    }
                    ChangeTag::Insert => {
                        new_chunk.push_str(change.value());
                    }
                }
            }
        }

        let context_hint = old_str
            .lines()
            .take(old_start_idx)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .find_map(|line| {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    Some(trimmed.chars().take(120).collect())
                } else {
                    None
                }
            });

        hunks.push(StreamEvent::EditHunk {
            path: path.to_string(),
            hunk_index,
            old_start,
            old_lines: old_lines_count,
            new_start,
            new_lines: new_lines_count,
            context_hint,
            old_text: old_chunk,
            new_text: new_chunk,
        });
    }

    hunks
}

const MAX_HEALING_OUTPUT_BYTES: usize = 4096;
const MAX_HEALING_DIAGNOSTIC_BYTES: usize = 768;

/// Limits only the model-facing report, leaving the captured command output intact.
fn compact_verification_output(result: &crate::toolchain::ToolchainCheckResult) -> String {
    if result.diagnostics.is_empty() {
        let output = result.combined_output.trim();
        if output.len() <= MAX_HEALING_OUTPUT_BYTES {
            return output.to_string();
        }

        // Keep startup failures and the final test or linker summary.
        const MARKER: &str = "\n[... command output omitted ...]\n";
        let side = (MAX_HEALING_OUTPUT_BYTES - MARKER.len()) / 2;
        let head = clip_utf8(output, side);
        let mut tail_start = output.len() - side;
        while !output.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        let tail = &output[tail_start..];
        return format!("{head}{MARKER}{tail}");
    }

    let rendered = crate::toolchain::format_diagnostics(&result.diagnostics);
    let total = rendered.lines().count();
    let mut report = String::new();
    const NOTICE_BYTES: usize = 64;
    for (index, line) in rendered.lines().enumerate() {
        let clipped = clip_utf8(line, MAX_HEALING_DIAGNOSTIC_BYTES);
        let suffix = if clipped.len() < line.len() {
            "…"
        } else {
            ""
        };
        if report.len() + clipped.len() + suffix.len() + 1 > MAX_HEALING_OUTPUT_BYTES - NOTICE_BYTES
        {
            report.push_str(&format!("[{} more diagnostic(s) omitted]", total - index));
            return report;
        }
        report.push_str(clipped);
        report.push_str(suffix);
        report.push('\n');
    }
    report.pop();
    report
}

fn clip_utf8(text: &str, max_bytes: usize) -> &str {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn format_verification_failure_report(
    results: &[crate::toolchain::ToolchainCheckResult],
) -> String {
    let mut report = String::new();
    for r in results.iter().filter(|r| !r.success) {
        let body = compact_verification_output(r);
        report.push_str(&format!(
            "Command `{}` failed:\n```\n{}\n```\n\n",
            r.command, body
        ));
    }
    if report.is_empty() {
        "Verification command returned non-zero exit status".into()
    } else {
        report
    }
}

/// Returns repository-backed diagnostic candidates without changing context access.
pub async fn diagnostic_file_candidates(repo_root: &Path, report: &str) -> Vec<String> {
    let available_files = crate::workflow::research::resolve_repository_files(repo_root)
        .await
        .unwrap_or_default();
    extract_error_paths(report, &available_files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextAccess;
    use crate::context::ContextManager;
    use crate::edits::EditReplace;
    use crate::llm::{CommitMessageCommand, LlmCommand, TextCommand};

    fn failed_command(output: String) -> crate::toolchain::ToolchainCheckResult {
        crate::toolchain::ToolchainCheckResult {
            success: false,
            is_cancelled: false,
            command: "just verify".into(),
            stdout: String::new(),
            stderr: String::new(),
            combined_output: output,
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn healing_output_retains_the_start_and_failure_tail_of_unparsed_output() {
        let result = failed_command(format!(
            "starting tests\n{}\nFAIL tests::roundtrip: values differ",
            "шум\n".repeat(MAX_HEALING_OUTPUT_BYTES)
        ));
        let report = compact_verification_output(&result);
        assert!(report.len() <= MAX_HEALING_OUTPUT_BYTES);
        assert!(report.starts_with("starting tests"));
        assert!(report.ends_with("FAIL tests::roundtrip: values differ"));
        assert!(report.contains("command output omitted"));
    }

    #[test]
    fn healing_output_prioritizes_located_errors_and_bounds_long_diagnostics() {
        let mut result = failed_command("raw terminal output".into());
        result.diagnostics = (1..=20)
            .rev()
            .map(|line| tauqe_tools::toolchain::Diagnostic {
                file: Some("src/lib.rs".into()),
                code: Some("E0308".into()),
                line: Some(line),
                message: "несовпадение типов ".repeat(MAX_HEALING_DIAGNOSTIC_BYTES),
                ..Default::default()
            })
            .collect();
        let report = compact_verification_output(&result);
        assert!(report.len() <= MAX_HEALING_OUTPUT_BYTES);
        assert!(report.starts_with("src/lib.rs:1: error[E0308]:"));
        assert!(report.contains("more diagnostic(s) omitted"));
        assert!(!report.contains("raw terminal output"));
    }

    struct Fixture {
        dir: tempfile::TempDir,
        context: ContextManager,
        cancel: CancellationToken,
        events: EventSink,
        policy: crate::workflow::policy::DirectApplyPolicy,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Self {
            let dir = tempfile::tempdir().expect("scratch directory");
            let root = dir.path().to_path_buf();
            crate::git::init_repository(Some(&root), true).expect("repository");
            for (path, content) in files {
                let full = root.join(path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).expect("dir");
                }
                std::fs::write(full, content).expect("file");
            }
            let mut context = ContextManager::new(root);
            for (path, _) in files {
                context
                    .add_file(path, ContextAccess::Editable)
                    .expect("context file");
            }
            Self {
                dir,
                context,
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::policy::DirectApplyPolicy::new(),
            }
        }

        fn context(&mut self) -> EditEnvironment<'_> {
            EditEnvironment {
                repo_root: self.dir.path(),
                context_manager: &mut self.context,
                policy: &mut self.policy,
                cancel: &self.cancel,
                events: &self.events,
            }
        }
    }

    #[test]
    fn declared_commit_message_reads_the_terminal_summary() {
        let envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::CommitMessage(CommitMessageCommand {
                commit_message: "  feat(hello): return a value  ".to_string(),
            })]);
        assert_eq!(
            declared_commit_message(&envelope).as_deref(),
            Some("feat(hello): return a value")
        );
    }

    #[test]
    fn declared_commit_message_ignores_a_blank_summary() {
        let envelope = ResponseEnvelope::from_commands(&[
            LlmCommand::CommitMessage(CommitMessageCommand {
                commit_message: "   ".to_string(),
            }),
            LlmCommand::Text(TextCommand {
                content: "done".to_string(),
            }),
        ]);
        assert_eq!(declared_commit_message(&envelope), None);
    }

    /// One application pass over an empty change set reaches verification and
    /// records the verdict the project declares: a repository that declares no
    /// gate accepts the pass without claiming that anything was verified.
    #[tokio::test]
    async fn an_empty_change_set_passes_a_project_without_verification() {
        let mut fixture = Fixture::new(&[("a.rs", "fn a() {}\n")]);
        let root = fixture.dir.path().to_path_buf();
        let initial_worktree = crate::blocking::run_blocking(|| snapshot_worktree(&root));
        let mut state = EditState::new(ProjectConfig::default(), initial_worktree);
        let mut transaction = TurnTransaction::new(&root);
        let mut ctx = fixture.context();

        let verdict = execute_edit_step(&mut ctx, &mut state, &mut transaction)
            .await
            .expect("the pass runs");
        assert_eq!(
            verdict,
            EditStepVerdict::Passed(VerificationVerdict::NotConfigured)
        );
        assert!(state.modified_paths.is_empty());
    }

    /// A staged replacement reaches the working tree in one pass.
    #[tokio::test]
    async fn a_staged_replacement_reaches_the_working_tree() {
        let mut fixture = Fixture::new(&[("a.rs", "fn a() {}\n")]);
        let root = fixture.dir.path().to_path_buf();
        let initial_worktree = crate::blocking::run_blocking(|| snapshot_worktree(&root));
        let mut state = EditState::new(ProjectConfig::default(), initial_worktree);
        state
            .staged
            .staged_files
            .insert("a.rs".to_string(), b"fn a() -> i32 { 1 }\n".to_vec());
        state
            .edit_batches
            .push(vec![EditOperation::Replace(EditReplace {
                path: "a.rs".to_string(),
                old_text: "fn a() {}\n".to_string(),
                new_text: "fn a() -> i32 { 1 }\n".to_string(),
            })]);
        let mut transaction = TurnTransaction::new(&root);
        let mut ctx = fixture.context();

        let verdict = execute_edit_step(&mut ctx, &mut state, &mut transaction)
            .await
            .expect("the pass runs");
        assert!(matches!(verdict, EditStepVerdict::Passed(_)));
        assert_eq!(state.modified_paths, vec!["a.rs".to_string()]);
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).expect("read the target"),
            "fn a() -> i32 { 1 }\n"
        );
    }

    #[tokio::test]
    async fn a_backup_conflict_refuses_application_before_any_write() {
        let mut fixture = Fixture::new(&[("a.rs", "before\n")]);
        let root = fixture.dir.path().to_path_buf();
        std::fs::create_dir(root.join("conflict")).unwrap();
        let mut state = EditState::new(ProjectConfig::default(), snapshot_worktree(&root));
        state
            .staged
            .staged_files
            .insert("conflict".into(), b"new".to_vec());
        state
            .staged
            .staged_files
            .insert("a.rs".into(), b"after\n".to_vec());
        state
            .edit_batches
            .push(vec![EditOperation::Replace(EditReplace {
                path: "a.rs".into(),
                old_text: "before\n".into(),
                new_text: "after\n".into(),
            })]);
        let mut transaction = TurnTransaction::new(&root);
        let verdict = execute_edit_step(&mut fixture.context(), &mut state, &mut transaction)
            .await
            .unwrap();
        assert!(
            matches!(verdict, EditStepVerdict::ApplyFailed { message } if message.contains("`conflict`"))
        );
        assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"before\n");
        assert!(root.join("conflict").is_dir());
    }
}
