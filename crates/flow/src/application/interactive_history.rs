//! Start the interactive journal turn from the context it actually observes.

use crate::api::{Application, HistoryError, OperationGuard};
use tauqe_core::history::HistorySnapshot;

#[derive(Default)]
pub struct InteractiveHistoryStart {
    pub turn_id: Option<u64>,
    pub history: Option<HistorySnapshot>,
}

impl Application {
    pub(crate) async fn prepare_interactive_history(
        &self,
        operation: &OperationGuard,
        prompt: String,
    ) -> Result<InteractiveHistoryStart, HistoryError> {
        if !self.owns_operation(operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        // Release the context lock before file reads and before taking the
        // history mutex. Read files and arrival metadata from the same copy.
        let context = self.context_for_turn().await;
        let (files, watermark) = tauqe_core::blocking::run_blocking_async(move || {
            let watermark = context
                .get_state()
                .items
                .iter()
                .map(|item| item.insertion_seq)
                .max()
                .filter(|watermark| *watermark > 0);
            let files = context
                .read_context_files()
                .into_iter()
                .map(|file| file.path)
                .collect();
            (files, watermark)
        })
        .await
        .map_err(|error| {
            tauqe_core::blocking::log_join_error(&error, "capture interactive turn context");
            HistoryError::Offload(error.to_string())
        })?;
        let (turn_id, history) = self.start_history_turn(prompt, files, watermark).await?;
        Ok(InteractiveHistoryStart { turn_id, history })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::config::AppConfig;
    use tauqe_core::context::ContextAccess;
    use tauqe_core::history::entry::HistoryEntry;

    fn application(root: &std::path::Path) -> Application {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(AppConfig::default(), root.to_path_buf(), sender)
    }

    #[tokio::test]
    async fn start_records_readable_paths_and_the_full_context_arrival_watermark() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("kept.rs"), "source").unwrap();
        std::fs::write(root.path().join("missing.rs"), "source").unwrap();
        let app = application(root.path());
        app.record_history_turn("Previous request".into(), Vec::new())
            .await
            .unwrap();
        app.test_mutate_context(|context| {
            context
                .add_file("kept.rs", ContextAccess::ReadOnly)
                .unwrap();
            context
                .add_auto_file("missing.rs", ContextAccess::ReadOnly)
                .unwrap();
        })
        .await
        .unwrap();
        let watermark = app
            .context_state()
            .await
            .items
            .iter()
            .map(|item| item.insertion_seq)
            .max();
        std::fs::remove_file(root.path().join("missing.rs")).unwrap();
        let identity = app.session_uuid().await;
        let operation = app.try_begin_operation().unwrap();
        let started = app
            .prepare_interactive_history(&operation, "Current request".into())
            .await
            .unwrap();
        assert!(started.turn_id.is_some());
        let before = started.history.unwrap();
        assert_eq!(before.entries().len(), 1);
        assert!(
            matches!(&before.entries()[0], HistoryEntry::Turn { prompt, .. } if prompt == "Previous request")
        );
        let after = app.history_for_turn().await.unwrap().snapshot().unwrap();
        match after.entries().last().unwrap() {
            HistoryEntry::Turn {
                id,
                prompt,
                context,
                context_watermark,
                ..
            } => {
                assert_eq!(*id, started.turn_id);
                assert_eq!(prompt, "Current request");
                assert_eq!(context, &vec!["kept.rs".to_string()]);
                assert_eq!(*context_watermark, watermark);
            }
            _ => panic!("the new journal entry must be the current turn"),
        }
        assert_eq!(identity, app.session_uuid().await);
    }

    #[tokio::test]
    async fn empty_context_records_no_arrival_watermark() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let started = app
            .prepare_interactive_history(&operation, "Empty context".into())
            .await
            .unwrap();
        assert!(started.history.unwrap().is_empty());
        let snapshot = app.history_for_turn().await.unwrap().snapshot().unwrap();
        assert!(
            matches!(&snapshot.entries()[0], HistoryEntry::Turn { context, context_watermark: None, .. } if context.is_empty())
        );
    }

    #[tokio::test]
    async fn unwritable_journal_keeps_the_existing_empty_snapshot_without_a_turn_id() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        std::fs::create_dir_all(root.path().join(".tauqe/history.jsonl")).unwrap();
        let operation = app.try_begin_operation().unwrap();
        let started = app
            .prepare_interactive_history(&operation, "Request".into())
            .await
            .unwrap();
        assert_eq!(started.turn_id, None);
        assert!(started.history.unwrap().is_empty());
        assert!(app.try_begin_operation().is_err());
    }

    #[tokio::test]
    async fn foreign_operation_is_rejected_before_the_journal_changes() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        app.record_history_turn("Existing".into(), Vec::new())
            .await
            .unwrap();
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        let _own = app.try_begin_operation().unwrap();
        assert!(matches!(
            app.prepare_interactive_history(&foreign, "Must not be recorded".into())
                .await,
            Err(HistoryError::OperationNotOwned)
        ));
        let snapshot = app.history_for_turn().await.unwrap().snapshot().unwrap();
        assert_eq!(snapshot.entries().len(), 1);
        assert!(
            matches!(&snapshot.entries()[0], HistoryEntry::Turn { prompt, .. } if prompt == "Existing")
        );
    }
}
