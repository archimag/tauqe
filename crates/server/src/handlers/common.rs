//! Shared helpers for the RPC handler subsystems.
//!
//! Event emission, transient-context cleanup, turn-outcome inspection,
//! read-only context preloading, and Architect synthesis history recording are
//! identical across the plan, review, and architect handlers. They are defined
//! once here so every subsystem emits events and mutates context through a
//! single code path.

use std::path::Path;
use std::sync::Arc;

use crate::state::AppState;
use crate::transport::OutChannel;
use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::outer_docs::{OuterDocsRegistry, MAX_DOCUMENT_BYTES};
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::research::apply_context_requests_detailed;
use tauqe_protocol::discussion::DiscussionTarget;
use tauqe_protocol::{events, ContextAccess, Event, ModelEscalatedEvent};
use tokio::sync::{mpsc, watch};

/// Emits a server event with an optional serialized parameter payload.
pub(crate) fn emit(out: &OutChannel, method: &str, params: Option<serde_json::Value>) {
    out.send_event(&Event {
        method: method.to_string(),
        params,
    });
}

/// Reason published with every withdrawal of an aborted stream attempt.
const STREAM_RESET_REASON: &str = "connection interrupted mid-stream";

/// Withdraws every fragment an aborted provider attempt already streamed.
///
/// A retrying provider re-issues a request that broke mid-stream, and the retry
/// re-generates the answer from its beginning. The fragments of the aborted
/// attempt are therefore void: they must not survive in the contour's own
/// buffer, and they must not survive in the client, or the retried payload is
/// concatenated onto a prefix that belongs to a different attempt.
///
/// Each relaying contour owns the buffers it accumulated, so it passes their
/// clearing here as `discard`; the single reset event published afterwards is
/// what makes the client drop its own copy. Expressing the withdrawal once keeps
/// the interactive turn, the Architect stream, and the review synthesis from
/// drifting apart.
pub(crate) fn withdraw_streamed_attempt(
    out: &OutChannel,
    operation_id: &str,
    discard: impl FnOnce(),
) {
    discard();
    emit(
        out,
        events::MODEL_STREAM_RESET,
        serde_json::to_value(tauqe_protocol::ModelStreamResetEvent {
            operation_id: operation_id.to_string(),
            reason: STREAM_RESET_REASON.to_string(),
        })
        .ok(),
    );
}

/// Relays the coder promotions a batch raises to the client.
///
/// The batch channel carries the telemetry the implementation machinery raises
/// while it executes a horizon, and the coordinate a client renders is the
/// promotion of a coder rung. The relay drains that channel for the whole run
/// and translates every promotion into the `model/escalated` event the
/// interface already reads, so an intra-role promotion becomes as visible as
/// the Architect contour reports its own. Every other coordinate is absorbed
/// here: step telemetry reaches the client through the turn's own sink, and
/// the channel exists so the executor never parks on a full queue.
///
/// The returned task settles once every sender is gone, so a supervisor drops
/// its own sender and joins it to publish the last promotion before the
/// terminal batch report leaves.
pub(crate) fn spawn_escalation_relay(
    out: OutChannel,
    operation_id: String,
    mut events_rx: mpsc::Receiver<StreamEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            if let StreamEvent::TierEscalated {
                tier,
                model,
                role,
                effort,
            } = event
            {
                emit(
                    &out,
                    events::MODEL_ESCALATED,
                    serde_json::to_value(ModelEscalatedEvent {
                        operation_id: operation_id.clone(),
                        model,
                        role: Some(role),
                        alias: None,
                        effort,
                        tier,
                    })
                    .ok(),
                );
            }
        }
    })
}

/// Whether a completed turn actually applied code edits to disk.
///
/// Plan steps and review findings ask the model to modify code: explanatory text
/// alone, even when accompanied by a terminal success tag, must not satisfy the
/// contract.
pub(crate) fn turn_applied_edits(turn: &crate::handlers::model::ModelTurnResult) -> bool {
    match &turn.result {
        Some(tauqe_protocol::ModelResult::Edit { outcome, .. }) => {
            outcome.applied && outcome.error.is_none()
        }
        _ => false,
    }
}

/// Whether a completed turn left changes in the working tree that the failed
/// attempt never reverted.
///
/// A failed edit turn rolls its Git transaction back to the step checkpoint, and
/// the FSM empties `changed_files` once that rollback succeeded. A change set the
/// FSM refused to apply while still naming modified files is therefore the single
/// signal that the working tree carries the bytes of an aborted attempt: an empty
/// change set never touched disk, and a turn without an edit result owns no
/// change set at all. A supervisor ends the step with a recovery instruction on
/// this signal instead of retrying on a dirty tree.
pub fn turn_left_unreverted_changes(turn: &crate::handlers::model::ModelTurnResult) -> bool {
    match &turn.result {
        Some(tauqe_protocol::ModelResult::Edit { outcome, .. }) => {
            !outcome.applied && !outcome.changed_files.is_empty()
        }
        _ => false,
    }
}

/// Releases the active discussion focus when it anchors the deleted entity.
///
/// A focus anchor may never address a non-existent entity, so deleting a plan
/// or a review session releases a focus bound to the removed target together
/// with it. The release goes through the shared [`AppState`] facade, which
/// broadcasts `DISCUSSION_FOCUS_CHANGED`, so every client drops the dead
/// anchor instead of rendering it. A focus anchored on any other entity is
/// left untouched. The single shared implementation keeps plan and review
/// deletions from drifting apart.
///
/// The comparison and the release are one critical section inside
/// [`AppState`], because `discussion/setFocus` shares no mutual exclusion with
/// the deleting handler: a focus installed concurrently must survive a
/// release that never addressed it.
pub(crate) async fn release_focus_anchored_on(state: &Arc<AppState>, anchor: &DiscussionTarget) {
    if state.release_discussion_focus_anchored_on(anchor).await {
        tracing::info!(
            anchor = ?anchor,
            "Cleared the discussion focus anchored on the deleted entity"
        );
    }
}

/// Releases the task's transient context so a retried attempt or the next step
/// starts from a clean, deterministic basis, then notifies clients of the new
/// snapshot.
///
/// The transient context is whatever the harness gathered so the model could
/// decide: the files a research round discovered and the external documents the
/// registry materialized. Both layers are released together, because they are
/// provided for the same decision and a step decides it fully.
///
/// The clear goes through the shared [`AppState`] facade, so removing files
/// from the transient layers also rotates the provider session identifier when
/// those layers really held files: the stable prompt prefix they warmed no
/// longer exists, and the next round must not be routed to a node holding a
/// stale copy of it.
pub(crate) async fn cleanup_transient_context(state: &Arc<AppState>) {
    if let Ok(new_ctx_state) = state.clear_transient_context().await {
        emit(
            &state.out,
            events::CONTEXT_CHANGED,
            Some(serde_json::json!({ "state": new_ctx_state })),
        );
    }
}

/// Normalizes a declared plan file path into the repository-relative form used
/// for context preloading, trimming whitespace and stripping leading `./` and
/// `/` markers so cosmetic spelling differences collapse to one key.
pub(crate) fn normalize_preload_path(path: &str) -> &str {
    path.trim().trim_start_matches("./").trim_start_matches('/')
}

/// Lists the tracked repository files on the blocking pool.
///
/// The listing spawns a `git` process, so it is resolved before any exclusive
/// context guard is taken: a guarded mutation closure must never hold every
/// context reader behind a process spawn.
pub(crate) async fn load_repository_files(repo_root: &Path) -> Vec<String> {
    let root = repo_root.to_path_buf();
    crate::handlers::offload(move || tauqe_core::git::list_repository_files(Some(&root)))
        .await
        .ok()
        .and_then(|files| files.ok())
        .unwrap_or_default()
}

/// Loads declared context files into the transient auto layer before a turn.
///
/// Target files are registered as editable and reference files as read-only.
/// Declared files are never capped by an artificial quota: the caller that
/// declares them already decided they are required, so both the files budget and
/// the refs budget stay unbounded. One implementation serves the step turn of the
/// implementation host and the read-only preload of an Architect synthesis, so the
/// two surfaces cannot drift apart.
pub(crate) async fn preload_declared_files(
    state: &Arc<AppState>,
    repo_root: &Path,
    files: &[String],
    readonly_files: &[String],
) {
    let mut requests: Vec<ContextRequest> = Vec::with_capacity(files.len() + readonly_files.len());
    requests.extend(
        files
            .iter()
            .map(String::as_str)
            .map(normalize_preload_path)
            .filter(|p| !p.is_empty())
            .map(|p| ContextRequest {
                path: p.to_string(),
                access: ContextAccess::Editable,
            }),
    );
    requests.extend(
        readonly_files
            .iter()
            .map(String::as_str)
            .map(normalize_preload_path)
            .filter(|p| !p.is_empty())
            .map(|p| ContextRequest {
                path: p.to_string(),
                access: ContextAccess::ReadOnly,
            }),
    );
    if requests.is_empty() {
        return;
    }

    let available_files = load_repository_files(repo_root).await;
    let preloaded = crate::handlers::with_context_mutation(state, move |ctx| {
        // Declared preload files are never capped by an artificial quota, so the
        // token budget stays unbounded here.
        let outcome = apply_context_requests_detailed(ctx, &requests, &available_files, usize::MAX);
        let new_state = ctx.get_state();
        (outcome, new_state)
    })
    .await;

    if let Ok((outcome, new_ctx_state)) = preloaded {
        if !outcome.added.is_empty() {
            emit(
                &state.out,
                events::CONTEXT_CHANGED,
                Some(serde_json::json!({ "state": new_ctx_state })),
            );
        }
    }
}

/// Loads declared target and reference files into the read-only auto context
/// layer before an Architect synthesis, so the real code of every existing step
/// is visible on the first generation round.
pub(crate) async fn preload_readonly_files(
    state: &Arc<AppState>,
    repo_root: &Path,
    files: &[String],
) {
    preload_declared_files(state, repo_root, &[], files).await;
}

/// Typed refusal of one declared external document.
///
/// A trust decision belongs to the developer: no round of the run can widen a
/// declaration the repository cannot reach, so the refusal travels outward with
/// the exact cause and the `[[workspace]]` block that would authorize the source.
pub(crate) struct OuterDocsRefusal {
    /// Declared entry the refusal addresses.
    pub id: String,
    /// Exact cause reported by the developer's trust declaration.
    pub reason: String,
    /// Ready-to-paste `[[workspace]]` declaration that would authorize the source.
    pub remedy: Option<String>,
}

impl OuterDocsRefusal {
    /// Diagnostic the step reports to the developer instead of starting its turn.
    pub(crate) fn to_message(&self) -> String {
        let mut message = format!(
            "The step was not started: the declared external document `{id}` is refused by the developer's trust declaration. {reason}",
            id = self.id,
            reason = self.reason
        );
        match &self.remedy {
            Some(remedy) => {
                message.push_str("\n\nAuthorize the source by adding this declaration to `$XDG_CONFIG_HOME/tauqe/refs-trust.toml`:\n\n");
                message.push_str(remedy.trim_end());
            }
            None => message.push_str(
                "\n\nThe declared source cannot be fetched at all, so correct the `[[outer_docs]]` entry of `tauqe.toml` instead of widening the trust declaration.",
            ),
        }
        message
    }
}

/// Materializes and mounts the external documents a step declares.
///
/// A declared document is materialized into the isolated documentation cache and
/// then mounted into the read-only OuterDocs context layer under the single
/// `[context].token_budget` ceiling, exactly as a research round admits one: both
/// kinds of context material are priced by the same rule, so a step never widens
/// the context past the ceiling the project declared.
///
/// A trust refusal is returned to the caller, because no round of the run can
/// widen a declaration the repository cannot reach. A delivery failure stays a
/// diagnostic: the step still runs, and the model routes around the missing
/// document instead of waiting for material that will never arrive.
pub(crate) async fn preload_declared_outer_docs(
    state: &Arc<AppState>,
    repo_root: &Path,
    registry: &OuterDocsRegistry,
    declared: &[String],
    token_budget: usize,
    cancel_rx: &watch::Receiver<bool>,
) -> Result<Vec<String>, OuterDocsRefusal> {
    let mut unavailable: Vec<String> = Vec::new();
    let mut mounted: Vec<String> = Vec::new();
    let mut requested: Vec<&str> = Vec::new();

    for id in declared {
        let id = id.trim();
        if id.is_empty() || requested.contains(&id) {
            continue;
        }
        requested.push(id);
        match registry
            .materialize_declared(repo_root, id, MAX_DOCUMENT_BYTES, Some(cancel_rx))
            .await
        {
            Ok(cache_path) => {
                if !mounted.contains(&cache_path) {
                    mounted.push(cache_path);
                }
            }
            Err(error) if error.trust().is_some() => {
                return Err(OuterDocsRefusal {
                    id: id.to_string(),
                    reason: error.reason(),
                    remedy: error.remedy(),
                });
            }
            Err(error) => unavailable.push(format!("- `{id}`: {}", error.reason())),
        }
    }

    if mounted.is_empty() {
        return Ok(unavailable);
    }

    // A declared document is mounted exactly like a requested repository file:
    // the same aggregate ceiling prices its content together with the wrapper its
    // own `<file>` element renders.
    let requests: Vec<ContextRequest> = mounted
        .iter()
        .map(|path| ContextRequest {
            path: path.clone(),
            access: ContextAccess::ReadOnly,
        })
        .collect();
    let applied = crate::handlers::with_context_mutation(state, move |ctx| {
        let outcome = apply_context_requests_detailed(ctx, &requests, &[], token_budget);
        let new_state = ctx.get_state();
        (outcome, new_state)
    })
    .await;

    match applied {
        Ok((outcome, new_state)) => {
            if !outcome.added.is_empty() {
                emit(
                    &state.out,
                    events::CONTEXT_CHANGED,
                    Some(serde_json::json!({ "state": new_state })),
                );
            }
            if let Some(limit) = outcome.limit_exceeded {
                unavailable.push(format!(
                    "- the declared external documents {} were not mounted: the context would exceed the configured token budget (current {} tokens, requested {}, budget {})",
                    limit.requested_files.join(", "),
                    limit.current_tokens,
                    limit.requested_tokens,
                    limit.token_budget
                ));
            }
            for rejection in &outcome.outer_docs_rejected {
                unavailable.push(format!("- `{}`: {}", rejection.path, rejection.reason));
            }
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "the declared external documents could not be mounted"
            );
            unavailable.push(format!(
                "- the declared external documents could not be mounted: {err}"
            ));
        }
    }

    Ok(unavailable)
}

/// Appends the diagnostics of the declared external documents to a step prompt.
///
/// Only the documents the harness could not mount reach the prompt: a mounted
/// document is already in the read-only OuterDocs layer, so announcing it would
/// restate the context block. The prompt tells the model to route around the
/// missing material instead of spending a round waiting for it.
pub(crate) fn append_outer_docs_diagnostics(prompt: &mut String, diagnostics: &[String]) {
    if diagnostics.is_empty() {
        return;
    }
    prompt.push_str("\n## Declared External Documentation Unavailable\n");
    prompt.push_str(
        "The plan declares the following external documents for this step, but the harness could not mount them:\n",
    );
    for diagnostic in diagnostics {
        prompt.push_str(diagnostic);
        prompt.push('\n');
    }
    prompt.push_str(
        "Work with the repository material you already hold and route around the missing document instead of waiting for it. The same document may be reachable again in a later turn.\n",
    );
}

/// Appends the developer directives section to an execution prompt when present.
///
/// Every prompt contour that can be started with developer directives — a plan
/// step, a review finding, a review sub-step — appends them the same way, so a
/// directive section never drifts between the workflows that accept one.
pub(crate) fn append_developer_directives(prompt: &mut String, instructions: Option<&str>) {
    if let Some(dirs) = instructions.map(str::trim).filter(|d| !d.is_empty()) {
        prompt.push_str("\n## Developer Directives:\n");
        prompt.push_str(dirs);
        prompt.push('\n');
    }
}

/// Records an Architect synthesis interaction atomically once it reaches a
/// terminal outcome.
///
/// The user turn and its response are written together under a single history
/// mutation, only after the synthesis has produced a durable result. Recording
/// the turn before the research loop would leave an orphaned user entry in the
/// transcript whenever the round is cancelled, fails, or is rejected before a
/// response exists. Persistence is `fsync`-bounded, so it runs on the blocking
/// pool through `crate::handlers::with_history_mutation` instead of stalling an
/// async worker thread.
pub(crate) async fn record_synthesis_history(
    state: &Arc<AppState>,
    turn_prompt: String,
    context_paths: Vec<String>,
    response_message: String,
    response_summary: Option<String>,
) {
    let _ = crate::handlers::with_history_mutation(state, move |history| {
        let _ = history.record_turn(turn_prompt, context_paths);
        let _ = history.record_response(response_message, None, response_summary, Vec::new());
    })
    .await;
}

/// Test-only helpers shared by the handler regression suites.
///
/// They live beside the production helpers so a read-path regression test does
/// not re-implement scratch-repository setup or tree snapshots in every
/// subsystem.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Creates a unique scratch repository root for a server regression test.
    ///
    /// The name is derived as `tauqe-<tag>-<pid>-<counter>`. The process id
    /// isolates the parallel test processes `cargo nextest` runs, each of which
    /// restarts its process-global counters; the atomic counter isolates the
    /// concurrently running tests of one process under the threaded model of
    /// `cargo test`. A name derived from the counter alone or from the clock
    /// alone is a defect: under nextest it collides between processes, and two
    /// colliding tests then share one directory and race on the files inside
    /// it. The directory is created before the path is returned, so a caller
    /// can write into it immediately.
    pub(crate) fn scratch_root(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let sequence = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("tauqe-{tag}-{}-{sequence}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create the scratch repository root");
        root
    }

    /// Snapshots every file under `root` as `(relative path, bytes)`.
    ///
    /// Comparing two snapshots proves that a request left the persisted state
    /// byte-identical — no file created, rewritten, or removed — without
    /// hard-coding the storage layout of the subsystem under test.
    pub(crate) fn snapshot_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut snapshot = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                let relative = path.strip_prefix(root).map_or_else(
                    |_| path.to_string_lossy().into_owned(),
                    |relative| relative.to_string_lossy().replace('\\', "/"),
                );
                snapshot.push((relative, std::fs::read(&path).unwrap_or_default()));
            }
        }
        snapshot.sort();
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::model::ModelTurnResult;

    /// Builds a completed turn carrying the given model result.
    fn completed_turn(result: Option<tauqe_protocol::ModelResult>) -> ModelTurnResult {
        ModelTurnResult {
            operation_id: "op-test".to_string(),
            result,
            is_cancelled: false,
            error: None,
            has_terminal_success: false,
            blocked: None,
            abort: None,
        }
    }

    /// Builds an edit result with the given verdict and reported files.
    fn edit_result(applied: bool, changed_files: Vec<String>) -> tauqe_protocol::ModelResult {
        tauqe_protocol::ModelResult::Edit {
            summary: "edit".to_string(),
            edits: Vec::new(),
            proposal: None,
            outcome: tauqe_protocol::EditOutcome {
                applied,
                error: if applied {
                    None
                } else {
                    Some("edit rejected".to_string())
                },
                changed_files,
                commit_hash: None,
                verification: None,
            },
        }
    }

    #[test]
    fn test_turn_left_unreverted_changes_reports_only_a_change_that_reached_disk() {
        // An applied change set went through the turn's own transaction.
        let applied = completed_turn(Some(edit_result(true, vec!["src/lib.rs".to_string()])));
        assert!(!turn_left_unreverted_changes(&applied));

        // A rejected change set that rolled back cleanly reports no files: the
        // FSM empties `changed_files` once the transaction is restored.
        let clean = completed_turn(Some(edit_result(false, Vec::new())));
        assert!(!turn_left_unreverted_changes(&clean));

        // A rejected change set that still names modified files means the
        // rollback never reached the step checkpoint.
        let dirty = completed_turn(Some(edit_result(false, vec!["src/lib.rs".to_string()])));
        assert!(turn_left_unreverted_changes(&dirty));

        // A conversational answer touches no bytes.
        let answer = completed_turn(Some(tauqe_protocol::ModelResult::Answer {
            text: "Nothing to change.".to_string(),
        }));
        assert!(!turn_left_unreverted_changes(&answer));

        // A turn that produced no result at all owns no change set.
        let no_result = completed_turn(None);
        assert!(!turn_left_unreverted_changes(&no_result));
    }

    #[test]
    fn test_append_outer_docs_diagnostics_renders_only_the_reported_failures() {
        let mut prompt = "Execute the step.".to_string();
        append_outer_docs_diagnostics(&mut prompt, &[]);
        assert_eq!(
            prompt, "Execute the step.",
            "a step whose documents all mounted keeps its prompt unchanged"
        );

        append_outer_docs_diagnostics(
            &mut prompt,
            &["- `vendor/api.md`: the document answered with the HTTP status 503".to_string()],
        );

        assert!(prompt.contains("## Declared External Documentation Unavailable"));
        assert!(prompt.contains("`vendor/api.md`"));
        assert!(prompt.contains("route around the missing document"));
    }

    #[test]
    fn test_a_trust_refusal_reports_the_declaration_that_authorizes_the_source() {
        let refusal = OuterDocsRefusal {
            id: "vendor/api.md".to_string(),
            reason: "the workspace has no entry in `refs-trust.toml`".to_string(),
            remedy: Some("[[workspace]]\nroot = \"/work/project\"".to_string()),
        };

        let message = refusal.to_message();

        assert!(message.contains("vendor/api.md"), "{message}");
        assert!(message.contains("no entry"), "{message}");
        assert!(message.contains("refs-trust.toml"), "{message}");
        assert!(message.contains("[[workspace]]"), "{message}");
    }

    #[test]
    fn test_a_refusal_without_a_declaration_names_the_entry_to_correct() {
        let refusal = OuterDocsRefusal {
            id: "vendor/api.md".to_string(),
            reason: "the source `file:///etc/passwd` is not an address the registry can fetch"
                .to_string(),
            remedy: None,
        };

        let message = refusal.to_message();

        assert!(message.contains("tauqe.toml"), "{message}");
        assert!(!message.contains("[[workspace]]"), "{message}");
    }

    /// Two scratch roots derived in one process never coincide, and each name
    /// carries the process id that isolates the parallel `cargo nextest`
    /// processes from one another.
    #[test]
    fn test_scratch_root_derives_a_process_unique_name_per_call() {
        let first = test_support::scratch_root("guard");
        let second = test_support::scratch_root("guard");

        assert!(first.is_dir(), "the scratch root must be created");
        assert_ne!(first, second, "two calls must never share a scratch root");

        let pid = std::process::id().to_string();
        let name = first
            .file_name()
            .and_then(|name| name.to_str())
            .expect("the scratch root name is valid UTF-8");
        assert!(
            name.split('-').any(|part| part == pid.as_str()),
            "the scratch root name must carry the process id: {name}"
        );

        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    /// The relay is the single translation seam between the batch channel and
    /// the wire: a coder promotion becomes `model/escalated` under the
    /// operation of the batch, every other coordinate is absorbed, and the task
    /// settles once the channel closes.
    #[tokio::test]
    async fn test_escalation_relay_publishes_a_promotion_and_ends_on_close() {
        let (out, mut rx) = OutChannel::new();
        let (stream_tx, stream_rx) = mpsc::channel(8);
        let relay = spawn_escalation_relay(out, "op-batch".to_string(), stream_rx);

        stream_tx
            .send(StreamEvent::TextDelta("absorbed".to_string()))
            .await
            .expect("the relay is listening");
        stream_tx
            .send(StreamEvent::TierEscalated {
                tier: 2,
                model: tauqe_protocol::ModelRef::openrouter("shared/model"),
                role: tauqe_protocol::ModelRole::Coder,
                effort: Some(tauqe_protocol::ReasoningEffort::High),
            })
            .await
            .expect("the relay is listening");
        drop(stream_tx);

        tokio::time::timeout(std::time::Duration::from_secs(1), relay)
            .await
            .expect("the relay settles once the channel closes")
            .expect("the relay task joins");

        let mut published = Vec::new();
        while let Ok(line) = rx.try_recv() {
            published.push(line);
        }
        assert_eq!(
            published.len(),
            1,
            "only the promotion reaches the client: {published:?}"
        );
        let event: serde_json::Value =
            serde_json::from_str(&published[0]).expect("the event is valid JSON");
        assert_eq!(event["method"], events::MODEL_ESCALATED);
        assert_eq!(event["params"]["operation_id"], "op-batch");
        assert_eq!(event["params"]["tier"], 2);
        assert_eq!(event["params"]["role"], "coder");
        assert_eq!(event["params"]["effort"], "high");
        assert_eq!(event["params"]["model"]["name"], "shared/model");
    }
}
