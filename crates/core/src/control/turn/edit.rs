//! Atomic disk-side edits and toolchain verification step.
//!
//! One application and verification pass is [`execute_edit_step`]: it formats the
//! virtual change set, writes it to the working tree, folds the artifacts the post-hook
//! and the toolchain generated into that change set, and reports a typed verdict. The
//! step requests no model round of its own: the healing prompts and the rounds that
//! carry them belong to [`crate::control::turn::run_turn_loop`], which is the single
//! loop of the turn.

use crate::config::project::ProjectConfig;
use crate::context::ContextAccess;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::staging::StagingState;
use crate::control::context::LoopContext;
use crate::control::outcome::{ResponseEnvelope, SystemError};
use crate::edits::EditFileOp;
use crate::edits::EditOperation;
use crate::edits::EditProposal;
use crate::edits::StagedEditsState;
use crate::edits::VerificationVerdict;
use crate::git::state::{changed_worktree_paths, snapshot_worktree, WorktreeSnapshot};
use crate::llm::LlmCapability;
use crate::llm::LlmCommand;
use crate::providers::StreamEvent;
use crate::toolchain::{
    format_staged_content, run_post_hooks_with_config, run_verification_pipeline_with_config,
    VerificationStatus,
};
use crate::workflow::verification::{build_verification_retry_prompt, extract_error_paths};
use similar::{ChangeTag, TextDiff};

/// Capabilities processed by the atomic edits and verification contour.
pub(crate) const EDIT_CAPABILITIES: &[LlmCapability] = &[
    LlmCapability::EditReplace,
    LlmCapability::EditCreate,
    LlmCapability::EditDelete,
    LlmCapability::EditMove,
    LlmCapability::EditMoveDir,
    LlmCapability::EditOverwrite,
];
/// Local in-memory backup of disk files touched during edit application.
///
/// It is the last-resort rollback: [`TurnTransaction::rollback`] restores the
/// working tree through the transaction policy and replays this backup only when
/// the policy restored nothing, so a restored tree is never overwritten with a
/// stale copy.
struct DiskBackup {
    root: PathBuf,
    files: HashMap<String, Option<Vec<u8>>>,
}

impl DiskBackup {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            files: HashMap::new(),
        }
    }

    fn record_paths(&mut self, paths: &[String]) {
        for path in paths {
            if !self.files.contains_key(path) {
                let full = self.root.join(path);
                let original = std::fs::read(&full).ok();
                self.files.insert(path.clone(), original);
            }
        }
    }

    fn record_paths_from_git_baseline(&mut self, paths: &[String]) {
        for path in paths {
            if !self.files.contains_key(path) {
                let original =
                    crate::git::cmd::run_git(Some(&self.root), &["show", &format!("HEAD:{path}")])
                        .ok()
                        .map(|s| s.into_bytes());
                self.files.insert(path.clone(), original);
            }
        }
    }

    /// Replays the recorded pre-turn bytes over the working tree.
    ///
    /// This is the fallback of [`TurnTransaction::rollback`]: it runs only when
    /// the policy restored no tree, so a developer's uncommitted work is never
    /// replaced by the bytes a checkpoint already restored.
    fn rollback(&self) {
        for (path, original) in &self.files {
            let full = self.root.join(path);
            match original {
                Some(bytes) => {
                    if let Some(parent) = full.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&full, bytes);
                }
                None => {
                    let _ = std::fs::remove_file(&full);
                }
            }
        }
    }
}

/// Owns the rollback obligation of one edit loop.
///
/// The loop reaches the working tree before it knows the change set survives:
/// every attempt is applied as a temporary Git step commit so verification runs
/// against a reproducible revision. The obligation to undo those commits
/// therefore belongs to the loop as a whole, never to one of its exits. Keeping
/// it in one value is what makes the guarantee total — a `?` on a healing round,
/// a toolchain run the developer cancelled, and a verification budget that ran
/// out all leave through the same door and unwind the same transaction.
///
/// The transaction is settled by [`TurnTransaction::settle`] once the policy has
/// attempted finalization. A finalization that failed settles too, because it
/// already consumed the checkpoint: the verified bytes stay in the working tree
/// and the artifact reports the failure instead of the turn withdrawing its work.
pub(crate) struct TurnTransaction {
    /// Last-resort backup of the pre-turn bytes of every touched path.
    backup: DiskBackup,
    /// Whether the policy has already settled the transaction.
    settled: bool,
}

impl TurnTransaction {
    /// Opens the transaction of one turn against the working tree it addresses.
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            backup: DiskBackup::new(root),
            settled: false,
        }
    }

    /// Marks the transaction as settled, so the closing rollback keeps the
    /// applied change set.
    pub(crate) fn settle(&mut self) {
        self.settled = true;
    }

    /// Restores the working tree unless the policy settled the transaction.
    ///
    /// The transaction policy is the authority on the working tree, so it
    /// restores the tree first and the local disk backup answers only when the
    /// policy restored nothing: a policy that holds no transaction, or a
    /// rollback that failed, leaves the pre-turn bytes to the backup. Writing
    /// the backup on top of a restored tree would replace the developer's own
    /// uncommitted work with the bytes the turn found, which is exactly what the
    /// checkpoint exists to protect.
    pub(crate) async fn rollback(&mut self, ctx: &mut LoopContext<'_>) {
        if self.settled {
            return;
        }
        let repo_root = ctx.repo_root;
        let restored = match ctx.policy.rollback(repo_root).await {
            Ok(restored) => restored,
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "failed to roll back the turn transaction; restoring the pre-turn bytes instead"
                );
                false
            }
        };
        if !restored {
            self.backup.rollback();
        }
        // The sweep only drops context paths that no longer exist on disk, so it
        // runs off the async executor thread. The auto layer is deliberately kept:
        // only a successful commit may shrink the context.
        crate::blocking::run_blocking(|| ctx.context_manager.prune_missing_files());
    }
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
    cumulative_edits: Vec<EditOperation>,
    /// Paths the change set touched so far, in first-touch order.
    pub(crate) modified_paths: Vec<String>,
    /// Working-copy status captured before the first application.
    initial_worktree: WorktreeSnapshot,
    /// Number of application and verification passes the turn spent.
    attempts: usize,
}

impl EditState {
    /// Creates the state the application passes of one turn share.
    pub(crate) fn new(project: ProjectConfig, initial_worktree: WorktreeSnapshot) -> Self {
        Self {
            project,
            staged: StagedEditsState::default(),
            cumulative_edits: Vec::new(),
            modified_paths: Vec::new(),
            initial_worktree,
            attempts: 0,
        }
    }

    /// Adopts the virtual change set the staging attempts of the turn converged on.
    pub(crate) fn adopt_staging(&mut self, staging: &StagingState) {
        self.staged = staging.staged_state.clone().unwrap_or_default();
        self.cumulative_edits = staging.cumulative_edits.clone();
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
pub(crate) async fn execute_edit_step(
    ctx: &mut LoopContext<'_>,
    state: &mut EditState,
    transaction: &mut TurnTransaction,
) -> Result<EditStepVerdict, SystemError> {
    state.attempts += 1;

    // 1. Format the virtual change set in memory when a formatter is declared.
    format_staged_files(ctx.repo_root, &state.project, &mut state.staged, ctx.cancel).await;

    // 2. Back up the pre-turn bytes of every touched path, then write the change
    // set to the working tree so verification runs against real bytes.
    let touched_paths = collect_staged_paths(&state.staged);
    transaction.backup.record_paths(&touched_paths);

    let proposal = EditProposal {
        summary: format!("edit round {}", state.attempts),
        edits: state.cumulative_edits.clone(),
    };

    if let Err(err) = ctx
        .policy
        .apply_temporary(
            ctx.repo_root,
            &state.project,
            ctx.context_manager,
            &proposal,
        )
        .await
    {
        return Ok(EditStepVerdict::ApplyFailed {
            message: err.to_string(),
        });
    }

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
    ctx: &mut LoopContext<'_>,
    state: &mut EditState,
    transaction: &mut TurnTransaction,
) -> Option<EditStepVerdict> {
    state.project.post_hook.as_ref()?;

    let hook_res = run_post_hooks_with_config(
        ctx.repo_root,
        &state.project,
        &state.modified_paths,
        Some(&ctx.cancel.receiver()),
    )
    .await;

    match hook_res {
        Ok(resp) if !resp.success => {
            let hook_error = resp.message.unwrap_or_else(|| "Post-hook failed".into());
            Some(EditStepVerdict::HookFailed {
                report: format!("Project post-hook failed:\n```\n{}\n```\n", hook_error),
            })
        }
        Ok(_) => {
            // The artifacts of the hook are the drift between the status the turn
            // opened with and the status that follows it, so a generated file the
            // hook wrote without naming enters the change set and its backup.
            let current_worktree =
                crate::blocking::run_blocking(|| snapshot_worktree(ctx.repo_root));
            let hook_artifacts = changed_worktree_paths(&state.initial_worktree, &current_worktree);
            transaction
                .backup
                .record_paths_from_git_baseline(&hook_artifacts);
            state.record_modified(hook_artifacts);
            emit_cumulative_diff(ctx, &transaction.backup);
            None
        }
        Err(err) => Some(EditStepVerdict::HookFailed {
            report: format!("Project post-hook execution error:\n```\n{}\n```\n", err),
        }),
    }
}

/// Runs the declared toolchain verification and reports its typed verdict.
///
async fn run_verification_pass(
    ctx: &mut LoopContext<'_>,
    state: &mut EditState,
    transaction: &mut TurnTransaction,
) -> Result<EditStepVerdict, SystemError> {
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

    if status == VerificationStatus::Cancelled || ctx.cancel.is_cancelled() {
        ctx.events.emit(StreamEvent::ToolchainFinished {
            command: "verify".into(),
            success: false,
            message: Some("Verification cancelled".into()),
        });
        return Err(SystemError::Cancelled);
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
        // Re-observe worktree for any drift produced during verification (e.g. Cargo.lock)
        let current_worktree = crate::blocking::run_blocking(|| snapshot_worktree(ctx.repo_root));
        let verify_drift = changed_worktree_paths(&state.initial_worktree, &current_worktree);
        transaction
            .backup
            .record_paths_from_git_baseline(&verify_drift);
        state.record_modified(verify_drift);

        emit_cumulative_diff(ctx, &transaction.backup);
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

/// Opens a healing round for a failed pass and renders the prompt that carries it.
///
/// The error files the report names enter the editable context, the attempt counter
/// advances, and the rendered prompt is the feedback of the round the caller requests,
/// so the model addresses the files it must fix instead of the rendering the turn
/// opened with. The round itself is requested by the loop that owns the turn.
pub(super) async fn prepare_healing(
    ctx: &mut LoopContext<'_>,
    report: &str,
    healing_attempt: &mut usize,
) -> String {
    auto_register_error_files(ctx, report).await;
    *healing_attempt += 1;
    build_verification_retry_prompt(report)
}

/// Returns the commit summary a round declared in its terminal success.
///
/// The success message is the model's own account of the change set, so it
/// becomes the summary of the final commit verbatim. A round that stated no
/// message yields `None`, and the caller keeps its own fallback label rather
/// than inventing a summary the model never wrote.
pub(super) fn declared_commit_message(envelope: &ResponseEnvelope) -> Option<String> {
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

async fn format_staged_files(
    repo_root: &Path,
    project: &ProjectConfig,
    staged: &mut StagedEditsState,
    cancel: &crate::control::context::CancellationToken,
) {
    if project.format.is_none() {
        return;
    }
    for bytes in staged.staged_files.values_mut() {
        if let Ok(content_str) = std::str::from_utf8(bytes) {
            if let Some(outcome) =
                format_staged_content(repo_root, content_str, Some(&cancel.receiver())).await
            {
                if outcome.formatted {
                    *bytes = outcome.content.into_bytes();
                }
            }
        }
    }
}

fn emit_cumulative_diff(ctx: &LoopContext<'_>, backup: &DiskBackup) {
    let root = ctx.repo_root.to_path_buf();
    let files = backup.files.clone();
    let events =
        crate::blocking::run_blocking(move || compute_cumulative_diff_events(&root, &files));
    for ev in events {
        ctx.events.emit(ev);
    }
}

fn compute_cumulative_diff_events(
    root: &Path,
    backup_files: &HashMap<String, Option<Vec<u8>>>,
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
        let full_path = root.join(path);
        let current = std::fs::read(&full_path).ok();

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

fn format_verification_failure_report(
    results: &[crate::toolchain::ToolchainCheckResult],
) -> String {
    let mut report = String::new();
    for r in results.iter().filter(|r| !r.success) {
        let body = if r.diagnostics.is_empty() {
            r.combined_output.trim().to_string()
        } else {
            crate::toolchain::format_diagnostics(&r.diagnostics)
        };
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

async fn auto_register_error_files(ctx: &mut LoopContext<'_>, report: &str) {
    let available_files = crate::workflow::research::resolve_repository_files(ctx.repo_root)
        .await
        .unwrap_or_default();
    let error_paths = extract_error_paths(report, &available_files);
    let added = crate::blocking::run_blocking(|| {
        let mut added = Vec::new();
        for path in error_paths {
            if ctx.context_manager.effective_access_for(&path) != Some(ContextAccess::Editable)
                && ctx
                    .context_manager
                    .add_auto_file(&path, ContextAccess::Editable)
                    .is_ok()
            {
                added.push(path);
            }
        }
        added
    });
    if !added.is_empty() {
        ctx.events
            .emit(StreamEvent::ContextChanged(ctx.context_manager.get_state()));
        // The error files the report named are the hot files of the healing
        // round: the churn mark stales the rendered prefix, so the reseed the
        // next round performs renders them with the anchors of the bytes the
        // model must fix instead of leaving them outside the frozen prefix.
        ctx.note_context_churn(&added, &[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink};
    use crate::edits::EditReplace;
    use crate::llm::CapabilityTree;
    use crate::llm::LlmCapability;
    use crate::llm::{CommitMessageCommand, LlmCommand, TextCommand};
    use crate::models::ModelRef;
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::research::journal::TurnResearchJournal;

    struct Fixture {
        dir: tempfile::TempDir,
        context: ContextManager,
        journal: TurnResearchJournal,
        provider: MockLlmProvider,
        model: ModelRef,
        protocol: JsonPromptProtocol,
        cache: crate::control::cache::TurnInputCache,
        cancel: CancellationToken,
        events: EventSink,
        policy: crate::workflow::policy::DirectApplyPolicy,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)], responses: Vec<MockResponse>) -> Self {
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
                journal: TurnResearchJournal::default(),
                provider: MockLlmProvider::scripted(responses),
                model: ModelRef::openrouter("mock/model"),
                protocol: JsonPromptProtocol::edits(),
                cache: crate::control::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::policy::DirectApplyPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            let caps = CapabilityTree::set(
                EDIT_CAPABILITIES
                    .iter()
                    .copied()
                    .chain([LlmCapability::TextMessage])
                    .map(CapabilityTree::leaf),
            );
            let prompt_builder = crate::control::prompt::PromptBuilder::new(
                &self.provider,
                &self.model,
                &self.protocol,
                &mut self.cache,
            );
            LoopContext::new(
                &self.cancel,
                &self.events,
                self.dir.path(),
                &mut self.context,
                &mut self.journal,
                &mut self.policy,
                caps,
                prompt_builder,
            )
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
        let mut fixture = Fixture::new(&[("a.rs", "fn a() {}\n")], Vec::new());
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
        let mut fixture = Fixture::new(&[("a.rs", "fn a() {}\n")], Vec::new());
        let root = fixture.dir.path().to_path_buf();
        let initial_worktree = crate::blocking::run_blocking(|| snapshot_worktree(&root));
        let mut state = EditState::new(ProjectConfig::default(), initial_worktree);
        state
            .staged
            .staged_files
            .insert("a.rs".to_string(), b"fn a() -> i32 { 1 }\n".to_vec());
        state
            .cumulative_edits
            .push(EditOperation::Replace(EditReplace {
                path: "a.rs".to_string(),
                old_text: "fn a() {}\n".to_string(),
                new_text: "fn a() -> i32 { 1 }\n".to_string(),
            }));
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
}
