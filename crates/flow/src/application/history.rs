//! Shared history ownership and serialized journal commands.

use crate::api::Application;
use std::path::PathBuf;
use std::sync::Arc;
use tauqe_core::history::{HistoryFileChange, HistoryItem};
use tauqe_core::history::{HistoryManager, HistoryMutation, HistorySnapshot};

use tokio::sync::{mpsc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("history mutation requires an active operation for this session")]
    OperationNotOwned,

    #[error("history is not initialized for a repository")]
    Unavailable,
    #[error("{0}")]
    Storage(String),
    #[error("blocking task failed: {0}")]
    Offload(String),
}

pub struct HistoryPage {
    pub items: Vec<HistoryItem>,
    pub has_more: bool,
    pub total_count: usize,
    pub estimated_tokens: u64,
}

#[derive(Default)]
pub(crate) struct HistorySession {
    state: Arc<Mutex<Option<HistoryManager>>>,
    sender: Option<mpsc::UnboundedSender<HistoryMutation>>,
}

fn manager(
    root: PathBuf,
    sender: Option<mpsc::UnboundedSender<HistoryMutation>>,
) -> HistoryManager {
    let mut history = HistoryManager::new(root);
    if let Some(sender) = sender {
        history.set_listener(move |mutation| {
            let _ = sender.send(mutation);
        });
    }
    history
}

impl HistorySession {
    pub(crate) fn new(root: PathBuf, sender: mpsc::UnboundedSender<HistoryMutation>) -> Self {
        let listener = sender.clone();
        let history = tauqe_core::blocking::run_blocking(move || manager(root, Some(listener)));
        Self {
            state: Arc::new(Mutex::new(Some(history))),
            sender: Some(sender),
        }
    }

    async fn mutate<T, F>(&self, mutate: F) -> Result<T, HistoryError>
    where
        F: FnOnce(&mut HistoryManager) -> Result<T, HistoryError> + Send + 'static,
        T: Send + 'static,
    {
        let mut guard = Arc::clone(&self.state).lock_owned().await;
        tauqe_core::blocking::run_blocking_async(move || {
            let history = guard.as_mut().ok_or(HistoryError::Unavailable)?;
            mutate(history)
        })
        .await
        .map_err(|error| {
            tauqe_core::blocking::log_join_error(&error, "access session history");
            HistoryError::Offload(error.to_string())
        })?
    }

    async fn rebind(&self, root: PathBuf) -> Result<(), HistoryError> {
        let sender = self.sender.clone();
        let history = tauqe_core::blocking::run_blocking_async(move || manager(root, sender))
            .await
            .map_err(|error| {
                tauqe_core::blocking::log_join_error(&error, "rebind session history");
                HistoryError::Offload(error.to_string())
            })?;
        *self.state.lock().await = Some(history);
        Ok(())
    }

    async fn clone_for_turn(&self) -> Result<HistoryManager, HistoryError> {
        self.state
            .lock()
            .await
            .clone()
            .ok_or(HistoryError::Unavailable)
    }
    async fn adopt(&self, history: HistoryManager) {
        *self.state.lock().await = Some(history);
    }
    #[cfg(test)]
    async fn tokens(&self) -> u64 {
        self.state
            .lock()
            .await
            .as_ref()
            .map_or(0, HistoryManager::estimated_tokens)
    }
}

fn storage_error(error: anyhow::Error) -> HistoryError {
    HistoryError::Storage(error.to_string())
}

impl Application {
    pub async fn history_page(
        &self,
        limit: Option<usize>,
        before_id: Option<u64>,
    ) -> Result<HistoryPage, HistoryError> {
        self.history_session()
            .mutate(move |history| {
                let estimated_tokens = history.estimated_tokens();
                let (items, has_more, total_count) = history
                    .history_slice(limit.unwrap_or(10), before_id)
                    .map_err(storage_error)?;
                Ok(HistoryPage {
                    items,
                    has_more,
                    total_count,
                    estimated_tokens,
                })
            })
            .await
    }

    #[cfg(test)]
    pub(crate) async fn history_token_estimate(&self) -> u64 {
        self.history_session().tokens().await
    }

    /// Private execution copy for compaction; no shared lock crosses provider I/O.
    pub(crate) async fn history_for_turn(&self) -> Result<HistoryManager, HistoryError> {
        self.history_session().clone_for_turn().await
    }
    pub(crate) async fn adopt_turn_history(&self, history: HistoryManager) {
        self.history_session().adopt(history).await;
    }
    pub(crate) async fn rebind_history(&self, root: PathBuf) -> Result<(), HistoryError> {
        self.history_session().rebind(root).await
    }

    pub(crate) async fn start_history_turn(
        &self,
        prompt: String,
        files: Vec<String>,
        watermark: Option<u64>,
    ) -> Result<(Option<u64>, Option<HistorySnapshot>), HistoryError> {
        self.history_session()
            .mutate(move |history| {
                let snapshot = history.snapshot().ok();
                let id = history
                    .record_turn_with_watermark(prompt, files, watermark)
                    .ok()
                    .and_then(|entry| entry.id());
                Ok((id, snapshot))
            })
            .await
    }
    pub async fn record_history_turn(
        &self,
        prompt: String,
        files: Vec<String>,
    ) -> Result<(), HistoryError> {
        self.history_session()
            .mutate(move |history| {
                history
                    .record_turn(prompt, files)
                    .map(|_| ())
                    .map_err(storage_error)
            })
            .await
    }
    pub(crate) async fn record_history_response(
        &self,
        text: String,
        commit: Option<String>,
        summary: Option<String>,
        files: Vec<HistoryFileChange>,
    ) -> Result<(), HistoryError> {
        self.history_session()
            .mutate(move |history| {
                history
                    .record_response(text, commit, summary, files)
                    .map(|_| ())
                    .map_err(storage_error)
            })
            .await
    }
    pub(crate) async fn record_history_review(
        &self,
        model: String,
        prompt: Option<String>,
        summary: Option<String>,
        files: Vec<String>,
        findings: usize,
    ) -> Result<(), HistoryError> {
        self.history_session()
            .mutate(move |history| {
                history
                    .record_review(model, prompt, summary, files, findings)
                    .map(|_| ())
                    .map_err(storage_error)
            })
            .await
    }
    pub(crate) async fn record_history_exchange(
        &self,
        prompt: String,
        paths: Vec<String>,
        response: String,
        summary: Option<String>,
    ) -> Result<(), HistoryError> {
        self.history_session()
            .mutate(move |history| {
                let _ = history.record_turn(prompt, paths);
                let _ = history.record_response(response, None, summary, Vec::new());
                Ok(())
            })
            .await
    }
    pub(crate) async fn rollback_history_entry(
        &self,
        id: u64,
        reason: String,
    ) -> Result<(), HistoryError> {
        self.history_session()
            .mutate(move |history| history.rollback_entry(id, reason).map_err(storage_error))
            .await
    }
    pub(crate) async fn clear_history(
        &self,
        operation: &crate::api::OperationGuard,
    ) -> Result<(), HistoryError> {
        if !self.owns_operation(operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        self.history_session()
            .mutate(move |history| history.clear().map_err(storage_error))
            .await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn test_mutate_history<T, F>(&self, mutate: F) -> Result<T, HistoryError>
    where
        F: FnOnce(&mut HistoryManager) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.history_session()
            .mutate(move |history| Ok(mutate(history)))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::config::AppConfig;

    fn application(root: PathBuf) -> (Application, mpsc::UnboundedReceiver<HistoryMutation>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Application::for_repository(AppConfig::default(), root, sender),
            receiver,
        )
    }

    #[tokio::test]
    async fn history_pages_keep_defaults_and_remain_readable_during_an_operation() {
        let root = tempfile::tempdir().unwrap();
        let (app, _events) = application(root.path().to_path_buf());
        for index in 0..12 {
            app.record_history_turn(format!("Task {index}"), Vec::new())
                .await
                .unwrap();
        }
        let _operation = app.try_begin_operation().unwrap();
        let page = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            app.history_page(None, None),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(page.items.len(), 10);
        assert_eq!(page.total_count, 12);
        assert!(page.has_more);
        assert!(page.estimated_tokens > 0);
        let previous = app
            .history_page(Some(10), Some(page.items[0].id))
            .await
            .unwrap();
        assert_eq!(previous.items.len(), 2);
        assert!(!previous.has_more);
    }

    #[tokio::test]
    async fn concurrent_records_survive_and_emit_one_event_each() {
        let root = tempfile::tempdir().unwrap();
        let (app, mut events) = application(root.path().to_path_buf());
        let mut tasks = Vec::new();
        for index in 0..8 {
            let app = app.clone();
            tasks.push(tokio::spawn(async move {
                app.record_history_turn(format!("Task {index}"), Vec::new())
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let page = app.history_page(Some(20), None).await.unwrap();
        assert_eq!(page.total_count, 8);
        let ids: std::collections::HashSet<_> = page.items.iter().map(|item| item.id).collect();
        assert_eq!(ids.len(), 8);
        let mut appended = 0;
        while let Ok(event) = events.try_recv() {
            if matches!(event, HistoryMutation::Appended { .. }) {
                appended += 1;
            }
        }
        assert_eq!(appended, 8);
    }

    #[tokio::test]
    async fn rebinding_keeps_listener_and_isolates_repository_journals() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let (app, mut events) = application(first.path().to_path_buf());
        app.record_history_turn("First".into(), Vec::new())
            .await
            .unwrap();
        let operation = app.begin_configuration_change().unwrap();
        operation
            .rebind_history(second.path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 0);
        app.record_history_turn("Second".into(), Vec::new())
            .await
            .unwrap();
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 1);
        operation
            .rebind_history(first.path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 1);
        assert!(matches!(
            events.try_recv(),
            Ok(HistoryMutation::Appended { .. })
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(HistoryMutation::Appended { .. })
        ));
    }

    #[tokio::test]
    async fn adopting_turn_history_preserves_ids_and_clear_requires_the_owning_operation() {
        let root = tempfile::tempdir().unwrap();
        let (app, _events) = application(root.path().to_path_buf());
        app.record_history_turn("First".into(), Vec::new())
            .await
            .unwrap();
        let mut turn = app.history_for_turn().await.unwrap();
        let turn = tauqe_core::blocking::run_blocking(move || {
            turn.record_turn("Second", Vec::new()).unwrap();
            turn
        });
        app.adopt_turn_history(turn).await;
        app.record_history_turn("Third".into(), Vec::new())
            .await
            .unwrap();
        assert_eq!(
            app.history_page(None, None)
                .await
                .unwrap()
                .items
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        assert!(matches!(
            app.clear_history(&foreign).await,
            Err(HistoryError::OperationNotOwned)
        ));
        let operation = app.try_begin_operation().unwrap();
        app.clear_history(&operation).await.unwrap();
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 0);
        assert_eq!(app.history_token_estimate().await, 0);
    }
}
