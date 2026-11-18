//! Shared helpers for the RPC handler subsystems.
//!
//! Event emission, transient-context cleanup, turn-outcome inspection,
//! read-only context preloading, and Architect synthesis history recording are
//! identical across the plan, review, and architect handlers. They are defined
//! once here so every subsystem emits events and mutates context through a
//! single code path.

use crate::contracts::Project;
use std::path::Path;
use std::sync::Arc;

use crate::state::AppState;
use crate::transport::OutChannel;
use tauqe_core::providers::StreamEvent;
use tauqe_protocol::{events, Event, ModelEscalatedEvent};
use tokio::sync::mpsc;

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
                        model: model.project(),
                        role: Some(role.project()),
                        alias: None,
                        effort: effort.map(crate::contracts::Project::project),
                        tier,
                    })
                    .ok(),
                );
            }
        }
    })
}

/// Publishes each context snapshot at the boundary selected by core preparation.
pub(crate) async fn prepare_implementation_step(
    state: &Arc<AppState>,
    operation: &tauqe_core::api::OperationGuard,
    root: &Path,
    config: &tauqe_core::config::AppConfig,
    request: &tauqe_core::workflow::implementation::StepTurnRequest,
) -> Result<String, tauqe_core::api::StepPreparationError> {
    state
        .application
        .prepare_implementation_step(operation, root, config, request, |snapshot| {
            crate::handlers::context::publish_context(&state.out, &snapshot);
        })
        .await
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
    use tokio::sync::watch;

    #[tokio::test]
    async fn step_preparation_publishes_clear_and_document_admission_in_order() {
        let root = test_support::scratch_root("preload-events");
        let cached = root
            .join(tauqe_core::outer_docs::OUTER_DOCS_CACHE_DIR)
            .join("vendor/guide.md");
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(cached, "API reference").unwrap();
        let entry = serde_json::from_value(serde_json::json!({
            "id": "vendor", "title": "API", "source": "https://example.test/guide.md"
        }))
        .unwrap();
        let config = tauqe_core::config::AppConfig {
            outer_docs: vec![entry],
            ..Default::default()
        };
        let scope = tauqe_core::workflow::implementation::ExecutionScope::Plan {
            plan_id: "preload".into(),
            step_id: None,
        };
        let queue = tauqe_core::workflow::turn::TurnWorkQueue {
            items: vec![tauqe_core::workflow::turn::TurnWorkItem {
                id: "1".into(),
                title: "Step".into(),
            }],
            active_index: 0,
        };
        let plan = tauqe_core::plan::Plan::from_parts(
            "preload".into(),
            "Preload".into(),
            None,
            0,
            0,
            Vec::new(),
            vec![tauqe_core::plan::PlanItem {
                id: "1".into(),
                title: "Step".into(),
                files: vec!["src/client.rs".into()],
                outer_docs: vec!["vendor".into()],
                ..Default::default()
            }],
        );
        tauqe_core::plan::storage::PlanStorage::save_plan(&root, &plan).unwrap();
        let (out, mut receiver) = OutChannel::new();
        let (history, _) = mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            root.clone(),
            history,
            out,
        );
        let (_, cancel) = watch::channel(false);
        let operation = state.application.try_begin_operation().unwrap();
        let request = tauqe_core::workflow::implementation::StepTurnRequest {
            scope,
            title: "Preload".into(),
            queue,
            prompt: "Execute step".into(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            selection: tauqe_core::models::ModelSelection::Tier {
                model: tauqe_core::models::ModelRef::openrouter("coder/model"),
                effort: None,
            },
            reasoning_effort: None,
            instructions: None,
            marker: None,
            session_id: None,
            cancel_rx: cancel,
        };
        for _ in 0..2 {
            let prompt = prepare_implementation_step(&state, &operation, &root, &config, &request)
                .await
                .unwrap();
            assert_eq!(prompt, "Execute step");
            let cleared: serde_json::Value =
                serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
            assert_eq!(cleared["method"], events::CONTEXT_CHANGED);
            assert_eq!(cleared["params"]["state"]["items"], serde_json::json!([]));
            let admitted: serde_json::Value =
                serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
            assert_eq!(admitted["method"], events::CONTEXT_CHANGED);
            assert_eq!(
                admitted["params"]["state"],
                serde_json::to_value(crate::handlers::context::context_state_dto(
                    &state.context_state().await
                ))
                .unwrap()
            );
            assert!(receiver.try_recv().is_err());
        }
        let _ = std::fs::remove_dir_all(root);
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
                model: tauqe_core::models::ModelRef::openrouter("shared/model"),
                role: tauqe_core::models::ModelRole::Coder,
                effort: Some(tauqe_core::models::ReasoningEffort::High),
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
