//! Wire coverage for grouped reviews.
#![cfg(test)]
use crate::state::{AppState, OperationGuard};
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use tauqe_core::providers::StreamEvent;
use tauqe_core::review::{ReviewGroup, ReviewSession};
use tauqe_core::workflow::batch::BatchReport;
use tauqe_protocol::events;

pub(crate) async fn run_group_audits(
    state: &Arc<AppState>,
    root: &Path,
    session: &ReviewSession,
    operation: Arc<OperationGuard>,
) -> Result<BatchReport, String> {
    let observer = Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    });
    state
        .application
        .test_run_group_audits(
            root,
            session,
            operation,
            observer,
            crate::reap::task_exit_deadline(),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::scratch_root;
    use crate::handlers::review::tests::{
        await_published, stall_outbound_transport, RecordingSink,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tauqe_core::providers::LlmProvider;
    use tauqe_core::review::ReviewStorage;
    use tokio::sync::mpsc;

    use serde_json::Value;
    use tauqe_core::config::AppConfig;
    use tauqe_core::models::{ModelRef, ProviderKind};
    use tauqe_core::providers::mock::MockResponse;
    use tauqe_core::providers::MockLlmProvider;
    use tauqe_core::workflow::batch::BatchStop;

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

    impl GroupFixture {
        /// Takes the outbound queue receiver, leaving the receiver of a queue
        /// that never receives, so a pacing regression can drive its own writer.
        fn take_events(&mut self) -> tokio::sync::mpsc::UnboundedReceiver<String> {
            let (_, placeholder) = tokio::sync::mpsc::unbounded_channel();
            std::mem::replace(&mut self.events, placeholder)
        }
    }

    /// Builds a scratch repository holding one file per audit group, plus the
    /// daemon state bound to it.
    fn fixture(label: &str) -> GroupFixture {
        let root = scratch_root(&format!("review-groups-{label}"));
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

        async fn stream(
            &self,
            _request: tauqe_core::providers::LlmRequest<'_>,
            tx: mpsc::Sender<StreamEvent>,
            _cancel: tauqe_core::stop::HardCancellation,
        ) -> tauqe_core::providers::ProviderResult {
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
        fixture.state.application.test_set_provider(provider).await;

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
        fixture.state.application.test_set_provider(provider).await;

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
        fixture.state.application.test_set_provider(provider).await;

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

    /// The reviewer reasons before it writes, so a grouped audit forwards the
    /// reasoning deltas of the group it audits exactly as a plain launch does:
    /// the client renders live thinking instead of a silent wait
    /// (#c-review-reasoning-stream).
    #[tokio::test]
    async fn group_audit_forwards_reasoning_deltas() {
        let mut fixture = fixture("reasoning");
        let session = session(vec![group("grp-alpha", "Alpha", &["alpha/"])]);
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let response = MockResponse::text(findings_payload("Alpha cloning"))
            .with_reasoning("Inspecting the audited slice");
        let provider = Arc::new(MockLlmProvider::scripted([response]));
        fixture.state.application.test_set_provider(provider).await;

        let operation = Arc::new(fixture.state.try_begin_operation().unwrap());
        let report = run_group_audits(&fixture.state, &fixture.root, &session, operation)
            .await
            .expect("the group batch runs");
        assert!(report.is_success());

        let published = drain_events(&mut fixture.events);
        let reasoning = events_of(&published, events::REVIEW_REASONING_DELTA);
        assert_eq!(reasoning.len(), 1, "the group forwards its reasoning");
        let reasoning_position = published
            .iter()
            .position(|(method, _)| method == events::REVIEW_REASONING_DELTA)
            .unwrap();
        let finished_position = published
            .iter()
            .position(|(method, _)| method == events::REVIEW_GROUP_FINISHED)
            .expect("the group publishes its completion");
        assert!(reasoning_position < finished_position);
        assert_eq!(
            reasoning[0]["delta"],
            serde_json::json!("Inspecting the audited slice")
        );
        let operation_id = reasoning[0]["operation_id"].as_str().unwrap_or_default();
        assert!(
            operation_id.starts_with("op-"),
            "the group delta names the operation it belongs to: {operation_id}"
        );
    }
    /// A group relays reasoning at token granularity, so it obeys the transport's
    /// pacing contract: a client that keeps its pipe open but stopped reading must
    /// pause the group audit instead of growing the outbound queue, and the drain
    /// of that queue must release the paused group.
    #[tokio::test]
    async fn group_audit_backpressures_a_stalled_client_until_the_queue_drains() {
        let mut fixture = fixture("pacing");
        let session = session(vec![group("grp-alpha", "Alpha", &["alpha/"])]);
        ReviewStorage::save_session(&fixture.root, &session).unwrap();

        let response = MockResponse::text(findings_payload("Alpha cloning"))
            .with_reasoning("Inspecting the audited slice");
        let provider = Arc::new(MockLlmProvider::scripted([response]));
        fixture.state.application.test_set_provider(provider).await;

        // The client keeps its pipe open but stopped reading its stream.
        stall_outbound_transport(&fixture.state);
        let outbound = fixture.take_events();

        let state = Arc::clone(&fixture.state);
        let root = fixture.root.clone();
        let operation = Arc::new(
            state
                .try_begin_operation()
                .expect("the operation slot is free"),
        );
        let batch =
            tokio::spawn(async move { run_group_audits(&state, &root, &session, operation).await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !batch.is_finished(),
            "a stalled client must pause the group audit"
        );
        let stalled = ReviewStorage::load_review(&fixture.root, "rev-groups")
            .unwrap()
            .expect("the session is stored");
        assert!(
            stalled.items.is_empty(),
            "a stalled group appends no findings"
        );

        // Draining the queue below the low-water mark releases the paused relay.
        let sink = RecordingSink::default();
        let writing = tokio::spawn(fixture.state.out.writer(outbound).run_with(sink.clone()));
        let published = await_published(&sink, events::REVIEW_REASONING_DELTA).await;
        assert!(
            published.contains("Inspecting the audited slice"),
            "the drain must release the paused group relay"
        );

        let report = batch
            .await
            .expect("the batch task joins")
            .expect("the group batch runs");
        assert!(report.is_success());
        let stored = ReviewStorage::load_review(&fixture.root, "rev-groups")
            .unwrap()
            .expect("the session is stored");
        assert_eq!(stored.items.len(), 1);
        assert_eq!(stored.items[0].group.as_deref(), Some("grp-alpha"));

        writing.abort();
        let _ = writing.await;
    }
}
