//! Best-effort bounded journal compaction owned by the application.

use crate::api::{Application, HistoryError, OperationGuard};
use crate::config::AppConfig;
use crate::models::ProviderKind;
use crate::providers::LlmProvider;
use tokio::sync::watch;

impl Application {
    /// The caller's operation keeps the history copy and adoption exclusive
    /// with destructive journal mutations, without locking over provider I/O.
    pub async fn compact_history(
        &self,
        operation: &OperationGuard,
        config: &AppConfig,
        cancel: watch::Receiver<bool>,
    ) -> Result<(), HistoryError> {
        if !self.owns_operation(operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        self.compact_history_using(config, cancel, |kind| {
            crate::providers::create_provider(kind, config)
        })
        .await
    }

    async fn compact_history_using<F>(
        &self,
        config: &AppConfig,
        cancel: watch::Receiver<bool>,
        resolve: F,
    ) -> Result<(), HistoryError>
    where
        F: FnOnce(ProviderKind) -> anyhow::Result<Box<dyn LlmProvider>>,
    {
        let budget = config.history.budget_tokens;
        let mut history = self.history_for_turn().await?;
        if !history.needs_compaction(budget) {
            return Ok(());
        }
        let model = config.history_model();
        let provider = match resolve(model.provider) {
            Ok(provider) => provider,
            Err(error) => {
                tracing::warn!(error = %error, "session journal stayed over budget: the Scribe provider is unavailable");
                return Ok(());
            }
        };
        let before = history.estimated_tokens();
        if let Err(error) = crate::control::history::compact_history_if_needed_with_budget(
            &mut history,
            provider.as_ref(),
            &model,
            cancel,
            budget,
            config.history.tail_turns,
        )
        .await
        {
            tracing::warn!(error = %error, "session journal compaction failed");
            return Ok(());
        }
        let after = history.estimated_tokens();
        if after != before {
            tracing::info!(
                before_tokens = before,
                after_tokens = after,
                "compacted the session journal before the turn"
            );
            self.adopt_turn_history(history).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryMutation;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use tokio::sync::mpsc;

    async fn populated(
        root: std::path::PathBuf,
    ) -> (
        Application,
        AppConfig,
        mpsc::UnboundedReceiver<HistoryMutation>,
    ) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root, sender);
        for index in 1..=4 {
            app.record_history_exchange(
                format!("Task {index}"),
                Vec::new(),
                format!("Done {index}"),
                None,
            )
            .await
            .unwrap();
        }
        let mut config = AppConfig::default();
        config.history.budget_tokens = app.history_token_estimate().await / 2;
        config.history.tail_turns = 1;
        (app, config, receiver)
    }

    #[tokio::test]
    async fn compaction_adopts_the_watermark_and_forwards_the_structural_event() {
        let root = tempfile::tempdir().unwrap();
        let (app, config, mut events) = populated(root.path().to_path_buf()).await;
        let _operation = app.try_begin_operation().unwrap();
        let before = app.history_page(Some(100), None).await.unwrap();
        let maximum_id = before.items.iter().map(|item| item.id).max().unwrap();
        let provider = MockLlmProvider::scripted([MockResponse::text(
            r#"{"summary":"Earlier tasks completed."}"#,
        )]);
        let (_cancel, receiver) = watch::channel(false);
        app.compact_history_using(&config, receiver, |_| Ok(Box::new(provider)))
            .await
            .unwrap();
        let compacted = app.history_page(Some(100), None).await.unwrap();
        assert_eq!(compacted.items.len(), 3);
        assert!(compacted.estimated_tokens < before.estimated_tokens);
        let mut structural = 0;
        while let Ok(event) = events.try_recv() {
            if matches!(event, HistoryMutation::Compacted { .. }) {
                structural += 1;
            }
        }
        assert_eq!(structural, 1);
        app.record_history_turn("Next task".into(), Vec::new())
            .await
            .unwrap();
        let after = app.history_page(Some(100), None).await.unwrap();
        assert!(after.items.last().unwrap().id > maximum_id);
    }

    #[tokio::test]
    async fn provider_resolution_or_stream_failure_keeps_the_journal_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let (app, config, _events) = populated(root.path().to_path_buf()).await;
        let _operation = app.try_begin_operation().unwrap();
        let before = app.history_page(Some(100), None).await.unwrap();
        let (_cancel, receiver) = watch::channel(false);
        app.compact_history_using(&config, receiver.clone(), |_| {
            Err(anyhow::anyhow!("provider unavailable"))
        })
        .await
        .unwrap();
        let provider = MockLlmProvider::scripted([MockResponse::error("stream failed")]);
        app.compact_history_using(&config, receiver, |_| Ok(Box::new(provider)))
            .await
            .unwrap();
        let after = app.history_page(Some(100), None).await.unwrap();
        assert_eq!(after.items, before.items);
        assert_eq!(after.estimated_tokens, before.estimated_tokens);
    }

    #[tokio::test]
    async fn an_in_budget_journal_never_resolves_a_provider() {
        let root = tempfile::tempdir().unwrap();
        let (app, mut config, _events) = populated(root.path().to_path_buf()).await;
        config.history.budget_tokens = u64::MAX;
        let (_cancel, receiver) = watch::channel(false);
        app.compact_history_using(&config, receiver, |_| {
            panic!("an in-budget journal needs no provider")
        })
        .await
        .unwrap();
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 8);
    }

    #[tokio::test]
    async fn compaction_refuses_an_operation_from_another_session() {
        let root = tempfile::tempdir().unwrap();
        let (app, config, _events) = populated(root.path().to_path_buf()).await;
        let other = Application::default();
        let operation = other.try_begin_operation().unwrap();
        let (_cancel, receiver) = watch::channel(false);
        assert!(matches!(
            app.compact_history(&operation, &config, receiver).await,
            Err(HistoryError::OperationNotOwned)
        ));
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 8);
    }
}
