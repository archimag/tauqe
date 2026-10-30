//! Shared fixtures for the colocated review handler suites.
//!
//! Every review concern keeps its cases beside the module it exercises, so the
//! repository, session, and provider fixtures they share are declared once here
//! and imported by the suites. A suite imports them; it never copies them.

use std::sync::Arc;

use super::handle_review_execute;

use crate::state::AppState;
use crate::transport::OutChannel;
use tauqe_core::providers::{ChatMessage, LlmProvider, ResponseFormat, StreamEvent};
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{IssueStatus, PlanItemStatus, Request, ReviewItem, ReviewSession};
use tokio::sync::{mpsc, watch};

mod batch;
mod batch_failures;
mod execute;
mod patterns;
mod preview;
mod start;
mod step;
mod storage_ops;
mod subplan;

/// Repository scratch directory owned by a test and removed on drop.
struct TestDir(std::path::PathBuf);

impl TestDir {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tauqe-test-review-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&path);
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_state(dir: &TestDir) -> Arc<AppState> {
    let (out, _rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    )
}

fn test_item(id: u32, status: IssueStatus, file: &str) -> tauqe_protocol::ReviewIssue {
    tauqe_protocol::ReviewIssue {
        id,
        title: format!("Finding {}", id),
        uuid: None,
        group: None,
        model: None,
        issue_type: tauqe_protocol::IssueType::Quality,
        status,
        description: "Details".to_string(),
        recommendations: Vec::new(),
        files: vec![file.to_string()],
        readonly_files: Vec::new(),
        note: None,
    }
}

fn make_tasks(review_id: &str, ids: &[u32]) -> Vec<(String, u32)> {
    ids.iter().map(|id| (review_id.to_string(), *id)).collect()
}

fn test_session(id: &str, items: Vec<tauqe_protocol::ReviewIssue>) -> ReviewSession {
    ReviewSession {
        id: id.to_string(),
        title: format!("Session {}", id),
        created_at: 100,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec!["foo.rs".to_string()],
        items: items.into_iter().map(ReviewItem::from).collect(),
        raw_markdown: String::new(),
        groups: Vec::new(),
        post_analysis: None,
        launch_config: None,
    }
}

fn test_config() -> tauqe_core::config::AppConfig {
    let mut config = tauqe_core::config::AppConfig::default();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
    config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
    config.models.auto_level_up = false;
    config
}
/// Creates a scratch repository holding the given files, committed once so the
/// repository listing an audit window reads reports every one of them as
/// tracked.
fn init_repo_with_files(dir: &TestDir, files: &[(&str, &str)]) {
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    let mut staged = Vec::new();
    for (path, content) in files {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, content).unwrap();
        staged.push((*path).to_string());
    }
    tauqe_core::git::cmd::stage_files(dir.path(), &staged).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add fixtures", false).unwrap();
}

/// Creates a scratch repository holding one committed target file.
fn init_repo_with_file(dir: &TestDir, path: &str) {
    init_repo_with_files(dir, &[(path, "pub fn target() {}\n")]);
}

/// Creates a scratch repository holding `count` committed source files under
/// `src/`, every one of them carrying the same fixture content and a path of the
/// same length, so a single price describes the whole set.
fn init_repo_with_uniform_files(dir: &TestDir, count: usize) -> Vec<String> {
    let paths: Vec<String> = (0..count)
        .map(|index| format!("src/f{index:03}.rs"))
        .collect();
    let fixtures: Vec<(&str, &str)> = paths
        .iter()
        .map(|path| (path.as_str(), "pub fn target() {}\n"))
        .collect();
    init_repo_with_files(dir, &fixtures);
    paths
}

async fn test_state_with_provider(
    dir: &TestDir,
    config: tauqe_core::config::AppConfig,
    provider: Arc<dyn LlmProvider>,
) -> Arc<AppState> {
    let (out, _rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);
    *state.provider_override.lock().await = Some(provider);
    state
}

fn architecture_edit(marker: &str, replacement: &str) -> String {
    format!(
        "<tauqe_edits_{m} summary=\"Apply fix\">\n<edit_{m} path=\"src/target.rs\">\n<search_{m}>\npub fn target() {{}}\n</search_{m}>\n<replace_{m}>\n{body}\n</replace_{m}>\n</edit_{m}>\n<success_{m} summary=\"done\" />\n</tauqe_edits_{m}>",
        m = marker,
        body = replacement
    )
}

fn subplan_item(
    id: &str,
    title: &str,
    status: PlanItemStatus,
    files: Vec<&str>,
) -> tauqe_protocol::PlanItem {
    tauqe_protocol::PlanItem {
        id: id.to_string(),
        title: title.to_string(),
        status,
        files: files.into_iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

/// Builds a subplan container (non-leaf) node from its children.
fn subplan_container(
    id: &str,
    title: &str,
    children: Vec<tauqe_protocol::PlanItem>,
) -> tauqe_protocol::PlanItem {
    tauqe_protocol::PlanItem {
        id: id.to_string(),
        title: title.to_string(),
        children,
        ..Default::default()
    }
}

/// Builds a leaf sub-step that declares the given dependencies.
fn dependent_subplan_item(id: &str, title: &str, deps: &[&str]) -> tauqe_protocol::PlanItem {
    tauqe_protocol::PlanItem {
        id: id.to_string(),
        title: title.to_string(),
        status: PlanItemStatus::Todo,
        files: vec!["src/target.rs".to_string()],
        depends_on: deps.iter().map(|d| d.to_string()).collect(),
        ..Default::default()
    }
}

/// Saves a session whose architectural group carries the given unified
/// subplan and collects one finding.
fn save_grouped_session(
    dir: &TestDir,
    review_id: &str,
    group_id: &str,
    subplan: Vec<tauqe_protocol::PlanItem>,
) {
    let mut item = test_item(1, IssueStatus::Todo, "src/target.rs");
    item.group = Some(group_id.to_string());
    let mut session = test_session(review_id, vec![item]);
    session.groups = vec![tauqe_protocol::ReviewGroup {
        id: group_id.to_string(),
        title: format!("Group {group_id}"),
        kind: tauqe_protocol::review::ReviewGroupKind::Architectural,
        subplan,
        ..Default::default()
    }];
    ReviewStorage::save_session(dir.path(), &session).unwrap();
}

/// A session collecting one finding under an architectural group.
fn architectural_session(review_id: &str, group_id: &str) -> ReviewSession {
    let mut item = test_item(1, IssueStatus::Todo, "src/target.rs");
    item.group = Some(group_id.to_string());
    let mut session = test_session(review_id, vec![item]);
    session.groups = vec![tauqe_protocol::ReviewGroup {
        id: group_id.to_string(),
        title: format!("Group {group_id}"),
        kind: tauqe_protocol::review::ReviewGroupKind::Architectural,
        ..Default::default()
    }];
    session
}

/// Sends a `review/execute` request targeting a single sub-step of a group.
async fn execute_group_substep(
    state: &Arc<AppState>,
    review_id: &str,
    group_id: &str,
    step_id: &str,
) -> tauqe_protocol::Response {
    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_EXECUTE.to_string(),
        params: Some(serde_json::json!({
            "review_id": review_id,
            "group_id": group_id,
            "step_id": step_id,
        })),
    };
    handle_review_execute(req, state).await
}

/// Provider whose first attempt streams a prefix of its answer and then breaks
/// at the transport layer, while every later attempt replays a complete payload.
///
/// It reproduces the mid-stream interruption the retry decorator heals, so a
/// test can assert that the aborted attempt is withdrawn instead of being
/// spliced onto the retried one.
struct InterruptedAttemptProvider {
    /// Payload streamed by the interrupted attempt.
    partial: String,
    /// Payload replayed by every later attempt.
    complete: String,
    /// Number of attempts started so far.
    attempts: Arc<std::sync::atomic::AtomicUsize>,
}

impl InterruptedAttemptProvider {
    fn new(partial: &str, complete: &str) -> Self {
        Self {
            partial: partial.to_string(),
            complete: complete.to_string(),
            attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Wraps the provider in the retry decorator a real transport uses.
    fn retrying(self) -> Arc<dyn LlmProvider> {
        Arc::new(tauqe_core::providers::RetryingProvider::new(
            Box::new(self),
            tauqe_core::providers::RetryConfig {
                max_attempts: 3,
                base_delay_ms: 1,
                max_delay_ms: 1,
            },
        ))
    }
}

#[async_trait::async_trait]
impl LlmProvider for InterruptedAttemptProvider {
    fn kind(&self) -> tauqe_protocol::ProviderKind {
        tauqe_protocol::ProviderKind::OpenRouter
    }

    async fn stream_chat(
        &self,
        _model: &str,
        _messages: Vec<ChatMessage>,
        _response_format: Option<ResponseFormat>,
        tx: mpsc::Sender<StreamEvent>,
        _cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let attempt = self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if attempt == 0 {
            let _ = tx.send(StreamEvent::TextDelta(self.partial.clone())).await;
            return Err(anyhow::anyhow!(
                "OpenRouter stream read error: connection reset by peer"
            ));
        }
        let _ = tx.send(StreamEvent::TextDelta(self.complete.clone())).await;
        let _ = tx.send(StreamEvent::Done).await;
        Ok(())
    }
}
