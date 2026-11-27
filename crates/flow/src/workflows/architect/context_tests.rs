#![cfg(test)]

use super::*;
use crate::api::Application;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauqe_core::config::AppConfig;
use tauqe_core::context::{ContextAccess, ContextLayer, ContextState};
use tauqe_core::models::{ModelRef, ProviderKind};
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::providers::{LlmProvider, LlmRequest, ProviderResult};
use tokio::sync::Notify;

const DEADLINE: Duration = Duration::from_secs(5);

struct Observer {
    snapshots: Mutex<Vec<ContextState>>,
    changed: Notify,
}

impl Observer {
    fn new() -> Self {
        Self {
            snapshots: Mutex::new(Vec::new()),
            changed: Notify::new(),
        }
    }

    async fn wait_for(&self, predicate: impl Fn(&ContextState) -> bool) -> ContextState {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(state) = self.snapshots.lock().unwrap().iter().find(|s| predicate(s)) {
                    return state.clone();
                }
                notified.await;
            }
        })
        .await
        .expect("context event")
    }

    fn states(&self) -> Vec<ContextState> {
        self.snapshots.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ApplicationObserver for Observer {
    fn publish(&self, _: &str, event: ApplicationEvent) {
        if let ApplicationEvent::Context(state) = event {
            self.snapshots.lock().unwrap().push(state);
            self.changed.notify_waiters();
        }
    }
}

struct PausedProvider {
    calls: AtomicUsize,
    requests: MockLlmProvider,
    second: Notify,
    release: Notify,
}

impl PausedProvider {
    fn new(fail: bool) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            requests: MockLlmProvider::scripted([
                MockResponse::text(serde_json::json!({
                    "status": "needs_context", "explanation": "Read source",
                    "reason": null, "abort": {"is_aborted": false, "reason": "", "recommendation": ""},
                    "context_requests": ["source.rs"], "repo_searches": [],
                    "repo_maps": [], "items": []
                }).to_string()),
                if fail {
                    MockResponse::error("provider failed")
                } else {
                    MockResponse::text(serde_json::json!({
                        "status": "blocked", "explanation": "Stopped",
                        "reason": "Stopped", "abort": {"is_aborted": false, "reason": "", "recommendation": ""},
                        "context_requests": [], "repo_searches": [], "repo_maps": [], "items": []
                    }).to_string())
                },
            ]),
            second: Notify::new(),
            release: Notify::new(),
        }
    }

    async fn wait_for_second(&self) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let notified = self.second.notified();
                if self.calls.load(Ordering::SeqCst) >= 2 {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("second architect request");
    }
}

#[async_trait::async_trait]
impl LlmProvider for PausedProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenRouter
    }

    async fn stream(
        &self,
        request: LlmRequest<'_>,
        tx: mpsc::Sender<StreamEvent>,
        cancel: tauqe_core::stop::HardCancellation,
    ) -> ProviderResult {
        if self.calls.fetch_add(1, Ordering::SeqCst) != 0 {
            self.second.notify_waiters();
            self.release.notified().await;
        }
        self.requests.stream(request, tx, cancel).await
    }
}

fn application(root: &std::path::Path) -> Application {
    let mut config = AppConfig::default();
    config.context.include_repo_map = false;
    let (history, _) = mpsc::unbounded_channel();
    Application::for_repository(config, root.into(), history)
}

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.rs"), "pub fn source() {}\n").unwrap();
    tauqe_core::git::cmd::stage_files(root.path(), &["source.rs".into()]).unwrap();
    root
}

fn has_source(state: &ContextState) -> bool {
    state.items.iter().any(|item| {
        item.path == "source.rs"
            && item.layer == ContextLayer::Auto
            && item.access == ContextAccess::ReadOnly
    })
}

async fn start(
    app: Application,
    operation: Arc<OperationGuard>,
    observer: Arc<Observer>,
) -> tokio::task::JoinHandle<(
    anyhow::Result<StructuredRefineOutcome>,
    ContourRelaySummary,
    Option<ArchitectResearchState>,
)> {
    tokio::spawn(async move {
        let environment = PlanExecutionEnvironment::new(app.configuration().await, &operation);
        let context = app.context_for_turn().await;
        run_contour_with_state(
            &app.execution_session(),
            app.context_session(),
            &operation,
            &environment,
            PlanHorizonContour {
                operation_id: "architect-test".into(),
                task: "Inspect source".into(),
                context,
                scope: ArchitectScope::Plan("test".into()),
                tier: ArchitectTier::from(ModelRef::openrouter("mock/model")),
                intro_header: String::new(),
            },
            observer,
            None,
        )
        .await
    })
}

#[tokio::test]
async fn research_is_visible_before_the_next_reply_and_is_revoked_on_error() {
    let root = fixture();
    let app = application(root.path());
    let provider = Arc::new(PausedProvider::new(true));
    app.test_set_provider(provider.clone()).await;
    let operation = Arc::new(app.try_begin_operation().unwrap());
    let observer = Arc::new(Observer::new());
    let running = start(app.clone(), operation.clone(), observer.clone()).await;
    provider.wait_for_second().await;
    assert!(
        has_source(&app.context_state().await),
        "second request started without research context; snapshots: {:?}",
        observer
            .states()
            .iter()
            .map(|state| &state.items)
            .collect::<Vec<_>>()
    );
    assert!(
        observer.states().iter().any(has_source),
        "research event missing: {:?}",
        observer.states()
    );
    let visible = observer.wait_for(has_source).await;
    assert_eq!(app.context_state().await, visible);
    assert!(!app
        .context_for_turn()
        .await
        .get_state()
        .items
        .iter()
        .any(|item| item.path == "source.rs"));

    provider.release.notify_one();
    let (result, summary, state) = tokio::time::timeout(DEADLINE, running)
        .await
        .unwrap()
        .unwrap();
    let requests = provider.requests.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]
        .messages
        .iter()
        .any(|message| message.content.contains("pub fn source")));
    assert!(requests[1]
        .messages
        .iter()
        .all(|message| message.role != "assistant"));
    assert!(result.is_err());
    assert_eq!(summary.error.as_deref(), Some("provider failed"));
    let state = state.expect("research survives a provider error");
    assert_eq!(
        state
            .execution
            .executor
            .evidence_snapshot()
            .unwrap()
            .files()[0]
            .content,
        "pub fn source() {}\n"
    );
    assert!(has_source(&state.context.get_state()));
    let restored = app.context_state().await;
    assert!(has_source(&restored));
    assert!(restored.revision >= visible.revision);
    assert_eq!(observer.states().last(), Some(&restored));
    drop(operation);
}

#[tokio::test]
async fn aborted_worker_restores_shared_context_and_rejects_late_callbacks() {
    let root = fixture();
    let app = application(root.path());
    let provider = Arc::new(PausedProvider::new(false));
    app.test_set_provider(provider.clone()).await;
    let operation = Arc::new(app.try_begin_operation().unwrap());
    let observer = Arc::new(Observer::new());
    let running = start(app.clone(), operation.clone(), observer.clone()).await;
    provider.wait_for_second().await;
    assert!(
        observer.states().iter().any(has_source),
        "research event missing: {:?}",
        observer.states()
    );
    let visible = observer.wait_for(has_source).await;
    app.execution_session().operations.hard_cancel();
    tokio::time::timeout(DEADLINE, async {
        while !app.execution_session().operations.abort(operation.id()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (_, summary, state) = tokio::time::timeout(DEADLINE, running)
        .await
        .unwrap()
        .unwrap();
    assert!(summary.cancelled);
    assert!(state.is_none());
    let restored = app.context_state().await;
    assert!(!has_source(&restored));
    assert!(restored.revision > visible.revision);
    assert_eq!(observer.states().last(), Some(&restored));
    drop(operation);
}

#[tokio::test]
async fn dropping_waiter_does_not_abandon_publication_recovery() {
    let root = fixture();
    let app = application(root.path());
    let provider = Arc::new(PausedProvider::new(true));
    app.test_set_provider(provider.clone()).await;
    let operation = Arc::new(app.try_begin_operation().unwrap());
    let observer = Arc::new(Observer::new());
    let running = start(app.clone(), operation.clone(), observer.clone()).await;
    provider.wait_for_second().await;
    assert!(
        observer.states().iter().any(has_source),
        "research event missing: {:?}",
        observer.states()
    );
    let visible = observer.wait_for(has_source).await;
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    provider.release.notify_one();
    tokio::time::timeout(DEADLINE, async {
        while app.usage_snapshot().await.operations == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accounting supervisor finishes after the waiter exits");
    let restored = app.context_state().await;
    assert!(has_source(&restored));
    assert_eq!(restored, visible);
    assert_eq!(observer.states().last(), Some(&restored));
    assert_eq!(app.active_operation_id(), Some(operation.id()));
    drop(operation);
}
