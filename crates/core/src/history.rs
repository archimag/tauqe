pub mod compaction;
pub mod entry;
pub mod storage;

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauqe_protocol::{HistoryEntryStatus, HistoryFileChange, UiHistoryItem, UiHistoryKind};

pub use compaction::{
    build_compacted_history, estimate_entries_tokens, partition_head_tail,
    DEFAULT_HISTORY_BUDGET_TOKENS, DEFAULT_TAIL_TURNS_COUNT,
};
pub use entry::HistoryEntry;
pub use storage::SessionStorage;

/// A typed notification of a mutation to the session history.
///
/// Appending a record and compacting the session are structurally different
/// operations: an append extends the dialogue, whereas compaction replaces the
/// summarized prefix with a single summary entry and preserves a tail.
/// Reporting both through one append-shaped notification forces a compaction
/// to masquerade as a trailing message, so subscribers receive the mutation
/// kind explicitly instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryMutation {
    /// A new interaction record was appended to the end of the session.
    Appended { item: UiHistoryItem },
    /// The summarized prefix was replaced by `summary`, preserving `tail_ids`.
    Compacted {
        summary: UiHistoryItem,
        tail_ids: Vec<u64>,
    },
    /// A record was rolled back: its turn failed or was cancelled before the
    /// result reached the model context.
    RolledBack { item: UiHistoryItem },
}

#[derive(Clone)]
pub struct HistoryEntryListener(pub Arc<dyn Fn(HistoryMutation) + Send + Sync>);

impl std::fmt::Debug for HistoryEntryListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HistoryEntryListener(..)")
    }
}
/// Interaction records frozen at the start of a bounded turn.
///
/// The freeze keeps the records themselves rather than the rendered
/// `<history>` block. The semantic layout renders that block from them through
/// [`HistorySnapshot::render_block`], which is the same definition the live
/// manager renders through, so the frozen block and the live block are byte
/// for byte identical. The chronological layout reads the recorded context
/// watermarks from the identical structure to place the context files between
/// the turns that introduced them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistorySnapshot {
    entries: Vec<HistoryEntry>,
}

impl HistorySnapshot {
    /// Freezes `entries` into a turn-scoped snapshot.
    pub fn new(entries: Vec<HistoryEntry>) -> Self {
        Self { entries }
    }

    /// Frozen records, in recording order.
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Reports whether the snapshot carries no record at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Highest context insertion watermark its turn records observed.
    ///
    /// A record written before the watermark existed carries none, so a
    /// session whose turns never captured one stays distinguishable from a
    /// session whose turns observed an empty context.
    pub fn watermark(&self) -> Option<u64> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                HistoryEntry::Turn {
                    context_watermark, ..
                } => *context_watermark,
                _ => None,
            })
            .max()
    }

    /// Renders the `<history>` block, or `None` when the snapshot is empty.
    pub fn render_block(&self) -> Option<String> {
        render_history_block(&self.entries)
    }
}

/// Renders interaction records as the `<history>` prompt block.
///
/// The live manager and every turn-scoped snapshot render through this single
/// definition, so a frozen snapshot reproduces byte for byte the block the
/// live history would have produced at the same moment.
///
/// A record that cannot be serialized is skipped with a diagnostic instead of
/// failing the whole block: the records around it still describe the session.
pub fn render_history_block(entries: &[HistoryEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let mut output = String::from("<history>\n");
    for entry in entries {
        match entry.to_jsonl_line() {
            Ok(line) => {
                output.push_str(&line);
                output.push('\n');
            }
            Err(error) => {
                tracing::warn!("Failed to serialize a history record for the prompt: {error}");
            }
        }
    }
    output.push_str("</history>");
    Some(output)
}
/// Facade for persistent interaction history management.
#[derive(Debug, Clone)]
pub struct HistoryManager {
    storage: SessionStorage,
    listener: Option<HistoryEntryListener>,
    current_max_id: u64,
}

impl HistoryManager {
    pub fn new(repo_root: PathBuf) -> Self {
        let storage = SessionStorage::new(repo_root);
        let current_max_id = storage
            .read_entries()
            .map(|entries| {
                entries
                    .iter()
                    .enumerate()
                    .map(|(idx, e)| e.id().unwrap_or((idx + 1) as u64))
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        Self {
            storage,
            listener: None,
            current_max_id,
        }
    }

    pub fn set_listener<F>(&mut self, listener: F)
    where
        F: Fn(HistoryMutation) + Send + Sync + 'static,
    {
        self.listener = Some(HistoryEntryListener(Arc::new(listener)));
    }

    pub fn entry_to_ui_item(id: u64, entry: &HistoryEntry) -> UiHistoryItem {
        let final_id = entry.id().unwrap_or(id);
        match entry {
            HistoryEntry::Summary { text, .. } => UiHistoryItem {
                id: final_id,
                kind: UiHistoryKind::System,
                text: text.clone(),
                summary: None,
                commit_hash: None,
                files: Vec::new(),
                status: HistoryEntryStatus::Committed,
            },
            HistoryEntry::Turn {
                prompt, context, ..
            } => UiHistoryItem {
                id: final_id,
                kind: UiHistoryKind::User,
                text: prompt.clone(),
                summary: None,
                commit_hash: None,
                files: context
                    .iter()
                    .cloned()
                    .map(HistoryFileChange::plain)
                    .collect(),
                status: HistoryEntryStatus::Committed,
            },
            HistoryEntry::Response {
                message,
                commit,
                summary,
                files,
                ..
            } => UiHistoryItem {
                id: final_id,
                kind: UiHistoryKind::Assistant,
                text: message.clone(),
                summary: summary.clone(),
                commit_hash: commit.clone(),
                // The persisted record already carries the typed operation, so
                // the producer's knowledge reaches every client unchanged and is
                // never re-inferred from rendered text (Conventions §1.5).
                files: files.clone(),
                status: HistoryEntryStatus::Committed,
            },
            HistoryEntry::Review {
                model,
                user_prompt,
                files,
                findings_count,
                summary,
                ..
            } => {
                let mut text = format!(
                    "Code review completed with {} ({} findings across {} files)",
                    model,
                    findings_count,
                    files.len()
                );
                if let Some(prompt) = user_prompt {
                    text.push_str(&format!("\nFocus: {}", prompt));
                }
                if let Some(summary) = summary {
                    if !summary.trim().is_empty() {
                        text.push_str("\n\n");
                        text.push_str(summary.trim());
                    }
                }
                UiHistoryItem {
                    id: final_id,
                    kind: UiHistoryKind::System,
                    text,
                    summary: Some(format!("Review: {} findings", findings_count)),
                    commit_hash: None,
                    files: files
                        .iter()
                        .cloned()
                        .map(HistoryFileChange::plain)
                        .collect(),
                    status: HistoryEntryStatus::Committed,
                }
            }
        }
    }

    /// Publishes an appended record under an explicit lifecycle status.
    ///
    /// A prompt enters the developer's chronicle the moment the turn starts, so
    /// it is published as [`HistoryEntryStatus::Active`] and only a recorded
    /// result is published as committed. Both notifications share one shape, so
    /// the client never has to infer the status from surrounding events.
    fn notify_entry(&self, entry: &HistoryEntry, status: HistoryEntryStatus) {
        if let Some(listener) = &self.listener {
            let id = entry.id().unwrap_or(self.current_max_id);
            let mut item = Self::entry_to_ui_item(id, entry);
            item.status = status;
            (listener.0)(HistoryMutation::Appended { item });
        }
    }

    pub fn repo_root(&self) -> &Path {
        self.storage.repo_root()
    }

    pub fn record_turn(
        &mut self,
        prompt: impl Into<String>,
        context_files: Vec<String>,
    ) -> Result<HistoryEntry> {
        self.current_max_id += 1;
        let entry = HistoryEntry::Turn {
            id: Some(self.current_max_id),
            prompt: prompt.into(),
            context: context_files,
            context_watermark: None,
        };
        self.storage.append_entry(&entry)?;
        self.notify_entry(&entry, HistoryEntryStatus::Active);
        Ok(entry)
    }

    pub fn record_response(
        &mut self,
        message: impl Into<String>,
        commit: Option<String>,
        summary: Option<String>,
        files: Vec<HistoryFileChange>,
    ) -> Result<HistoryEntry> {
        self.current_max_id += 1;
        let entry = HistoryEntry::Response {
            id: Some(self.current_max_id),
            message: message.into(),
            commit,
            summary,
            files,
        };
        self.storage.append_entry(&entry)?;
        self.notify_entry(&entry, HistoryEntryStatus::Committed);
        Ok(entry)
    }

    pub fn record_review(
        &mut self,
        model: impl Into<String>,
        user_prompt: Option<String>,
        summary: Option<String>,
        files: Vec<String>,
        findings_count: usize,
    ) -> Result<HistoryEntry> {
        self.current_max_id += 1;
        let entry = HistoryEntry::Review {
            id: Some(self.current_max_id),
            model: model.into(),
            user_prompt,
            files,
            findings_count,
            summary,
        };
        self.storage.append_entry(&entry)?;
        self.notify_entry(&entry, HistoryEntryStatus::Committed);
        Ok(entry)
    }

    pub fn get_entries(&self) -> Result<Vec<HistoryEntry>> {
        self.storage.read_entries()
    }

    pub fn get_ui_slice(
        &self,
        limit: usize,
        before_id: Option<u64>,
    ) -> Result<(Vec<UiHistoryItem>, bool, usize)> {
        let entries = self.get_entries()?;
        let total_count = entries.len();
        let all_items: Vec<UiHistoryItem> = entries
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let fallback_id = (idx + 1) as u64;
                Self::entry_to_ui_item(entry.id().unwrap_or(fallback_id), entry)
            })
            .collect();

        let candidate_slice = match before_id {
            Some(bid) => {
                let end_idx = all_items
                    .iter()
                    .position(|it| it.id >= bid)
                    .unwrap_or(all_items.len());
                &all_items[..end_idx]
            }
            None => &all_items[..],
        };

        let start_idx = candidate_slice.len().saturating_sub(limit);
        let has_more = start_idx > 0;
        let items = candidate_slice[start_idx..].to_vec();

        Ok((items, has_more, total_count))
    }

    pub fn estimated_tokens(&self) -> Result<u64> {
        let entries = self.get_entries()?;
        Ok(estimate_entries_tokens(&entries))
    }

    pub fn needs_compaction(&self, budget_tokens: u64) -> Result<bool> {
        Ok(self.estimated_tokens()? > budget_tokens)
    }

    /// Formats the session history directly inside `<history>...</history>` for LLM prompt injection.
    /// Formats the session history directly inside `<history>...</history>` for LLM prompt injection.
    ///
    /// The block comes from the same renderer a turn-scoped
    /// [`HistorySnapshot`] renders through, so a frozen snapshot reproduces the
    /// block the live history would produce at that moment byte for byte.
    /// Returns an empty string if there are no history entries.
    pub fn format_history_tag(&self) -> Result<String> {
        let entries = self.get_entries()?;
        Ok(render_history_block(&entries).unwrap_or_default())
    }

    /// Freezes the current records into a turn-scoped [`HistorySnapshot`].
    ///
    /// The snapshot keeps the records themselves, so a later round renders the
    /// semantic `<history>` block and reads the recorded context watermarks
    /// from the identical structure.
    pub fn snapshot(&self) -> Result<HistorySnapshot> {
        Ok(HistorySnapshot::new(self.get_entries()?))
    }
    /// Partitions current history into `(head, tail)` for compaction.
    pub fn prepare_compaction(
        &self,
        keep_tail_turns: usize,
    ) -> Result<Option<(Vec<HistoryEntry>, Vec<HistoryEntry>)>> {
        let entries = self.get_entries()?;
        Ok(partition_head_tail(&entries, keep_tail_turns))
    }

    /// Applies compaction by rewriting the session with `summary_text` followed by `tail` entries.
    pub fn apply_compaction(
        &mut self,
        summary_text: impl Into<String>,
        tail: Vec<HistoryEntry>,
    ) -> Result<()> {
        let summary_id = tail
            .first()
            .and_then(|e| e.id())
            .map(|id| id.saturating_sub(1));
        let tail_ids: Vec<u64> = tail.iter().filter_map(|entry| entry.id()).collect();
        let compacted = build_compacted_history(summary_id, summary_text.into(), tail);
        self.storage.rewrite_entries(&compacted)?;
        if let Some(listener) = &self.listener {
            if let Some(summary_entry) = compacted.first() {
                let id = summary_entry.id().unwrap_or(1);
                let summary = Self::entry_to_ui_item(id, summary_entry);
                (listener.0)(HistoryMutation::Compacted { summary, tail_ids });
            }
        }
        Ok(())
    }

    /// Removes an entry by ID, rewriting storage and updating `current_max_id`.
    ///
    /// A rolled-back record leaves the model context but stays visible in the
    /// developer's chronicle: the listener receives the discarded record with
    /// [`HistoryEntryStatus::RolledBack`] together with the interruption reason.
    /// The client therefore updates exactly the entry the failure refers to
    /// instead of guessing which prompt was rejected.
    pub fn rollback_entry(&mut self, entry_id: u64, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let mut entries = self.storage.read_entries()?;
        if let Some(pos) = entries.iter().rposition(|e| e.id() == Some(entry_id)) {
            let discarded = entries.remove(pos);
            self.storage.rewrite_entries(&entries)?;
            self.current_max_id = entries
                .iter()
                .enumerate()
                .map(|(idx, e)| e.id().unwrap_or((idx + 1) as u64))
                .max()
                .unwrap_or(0);
            if let Some(listener) = &self.listener {
                let mut item = Self::entry_to_ui_item(entry_id, &discarded);
                item.status = HistoryEntryStatus::RolledBack;
                item.summary = Some(reason);
                (listener.0)(HistoryMutation::RolledBack { item });
            }
        }
        Ok(())
    }

    pub fn clear(&mut self) -> Result<()> {
        self.current_max_id = 0;
        self.storage.clear()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tauqe_protocol::edit::EditFileOp;
    use tempfile::tempdir;

    #[test]
    fn test_history_manager_flow() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut hm = HistoryManager::new(root);

        assert_eq!(hm.format_history_tag().unwrap(), "");

        hm.record_turn("Check git status", vec!["src/git.rs".to_string()])
            .unwrap();
        hm.record_response(
            "Checked git status",
            Some("f1a2b3c".to_string()),
            Some("Fix status check".to_string()),
            vec![HistoryFileChange::plain("src/git.rs")],
        )
        .unwrap();
        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 2);

        let tag = hm.format_history_tag().unwrap();
        assert!(tag.starts_with("<history>\n"));
        assert!(tag.ends_with("\n</history>"));
        assert!(tag.contains(
            r#"{"type":"turn","id":1,"prompt":"Check git status","context":["src/git.rs"]}"#
        ));

        // Clear
        hm.clear().unwrap();
        assert_eq!(hm.format_history_tag().unwrap(), "");
    }

    #[test]
    fn test_history_compaction_flow() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut hm = HistoryManager::new(root);

        for i in 1..=4 {
            hm.record_turn(format!("Task {}", i), vec![]).unwrap();
            hm.record_response(format!("Done {}", i), None, None, vec![])
                .unwrap();
        }

        let (head, tail) = hm.prepare_compaction(2).unwrap().unwrap();
        assert_eq!(head.len(), 4);
        assert_eq!(tail.len(), 4);

        hm.apply_compaction("Tasks 1 and 2 completed successfully.", tail)
            .unwrap();

        let compacted_entries = hm.get_entries().unwrap();
        assert_eq!(compacted_entries.len(), 5); // 1 summary + 4 tail entries
        match &compacted_entries[0] {
            HistoryEntry::Summary { text, .. } => {
                assert_eq!(text, "Tasks 1 and 2 completed successfully.");
            }
            _ => panic!("Expected summary entry first"),
        }
    }

    #[test]
    fn test_history_manager_rollback_entry() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut hm = HistoryManager::new(root);

        let entry1 = hm.record_turn("Task 1", vec![]).unwrap();
        let entry2 = hm.record_turn("Task 2", vec![]).unwrap();
        assert_eq!(hm.get_entries().unwrap().len(), 2);

        hm.rollback_entry(entry2.id().unwrap(), "Turn cancelled by the developer")
            .unwrap();
        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id(), entry1.id());

        hm.rollback_entry(999, "Missing entry").unwrap();
        assert_eq!(hm.get_entries().unwrap().len(), 1);
    }

    #[test]
    fn test_rollback_notifies_typed_record_with_reason() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut hm = HistoryManager::new(root);

        let received: Arc<std::sync::Mutex<Vec<HistoryMutation>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        hm.set_listener(move |mutation| {
            let mut guard = sink.lock().unwrap();
            guard.push(mutation);
        });

        let entry = hm.record_turn("Refactor the parser", vec![]).unwrap();
        let id = entry.id().unwrap();
        hm.rollback_entry(id, "Turn cancelled by the developer")
            .unwrap();

        let mutations = received.lock().unwrap();
        assert_eq!(
            mutations.len(),
            2,
            "the append and the rollback are reported"
        );

        // The prompt reaches the chronicle the moment the turn starts, so the
        // developer sees the running task before any result exists.
        match &mutations[0] {
            HistoryMutation::Appended { item } => {
                assert_eq!(item.id, id);
                assert_eq!(item.status, HistoryEntryStatus::Active);
                assert_eq!(item.text, "Refactor the parser");
            }
            other => panic!("expected an append, got {:?}", other),
        }

        // The rollback keeps the discarded prompt and carries the reason, so the
        // client can explain why the turn never reached the model context.
        match &mutations[1] {
            HistoryMutation::RolledBack { item } => {
                assert_eq!(item.id, id);
                assert_eq!(item.status, HistoryEntryStatus::RolledBack);
                assert_eq!(item.text, "Refactor the parser");
                assert_eq!(
                    item.summary.as_deref(),
                    Some("Turn cancelled by the developer")
                );
            }
            other => panic!("expected a rollback, got {:?}", other),
        }
    }

    #[test]
    fn test_entry_to_ui_item() {
        let entry1 = HistoryEntry::Summary {
            id: Some(1),
            text: "Summary text".to_string(),
        };
        let item1 = HistoryManager::entry_to_ui_item(1, &entry1);
        assert_eq!(item1.id, 1);
        assert_eq!(item1.kind, UiHistoryKind::System);
        assert_eq!(item1.text, "Summary text");

        let entry2 = HistoryEntry::Turn {
            id: Some(2),
            prompt: "Write tests".to_string(),
            context: vec!["test.rs".to_string()],
            context_watermark: None,
        };
        let item2 = HistoryManager::entry_to_ui_item(2, &entry2);
        assert_eq!(item2.id, 2);
        assert_eq!(item2.kind, UiHistoryKind::User);
        assert_eq!(item2.text, "Write tests");
        assert_eq!(item2.files, vec![HistoryFileChange::plain("test.rs")]);

        let entry3 = HistoryEntry::Response {
            id: Some(3),
            message: "Done".to_string(),
            commit: Some("a1b2c3d".to_string()),
            summary: Some("Add tests".to_string()),
            files: vec![HistoryFileChange::plain("test.rs")],
        };
        let item3 = HistoryManager::entry_to_ui_item(3, &entry3);
        assert_eq!(item3.id, 3);
        assert_eq!(item3.kind, UiHistoryKind::Assistant);
        assert_eq!(item3.text, "Done");
        assert_eq!(item3.commit_hash, Some("a1b2c3d".to_string()));
        assert_eq!(item3.summary, Some("Add tests".to_string()));
        assert_eq!(item3.files, vec![HistoryFileChange::plain("test.rs")]);

        // A persisted record already carries the typed operation, so the UI
        // item passes every record through unchanged.
        let entry5_files = vec![
            HistoryFileChange::operated(EditFileOp::Replace, "src/lib.rs"),
            HistoryFileChange::operated(EditFileOp::Create, "src/util.rs"),
            HistoryFileChange::moved("src/a.rs", "src/b.rs"),
            HistoryFileChange::plain("src/plain.rs"),
        ];
        let entry5 = HistoryEntry::Response {
            id: Some(5),
            message: "Refactored".to_string(),
            commit: None,
            summary: None,
            files: entry5_files.clone(),
        };
        let item5 = HistoryManager::entry_to_ui_item(5, &entry5);
        assert_eq!(item5.files, entry5_files);

        // A review record maps its summary into the history item text so the
        // analytical report survives the streaming turn.
        let entry4 = HistoryEntry::Review {
            id: Some(4),
            model: "test/reviewer".to_string(),
            user_prompt: Some("Focus on locking".to_string()),
            files: vec!["src/lib.rs".to_string()],
            findings_count: 2,
            summary: Some("Locking is racy under load.".to_string()),
        };
        let item4 = HistoryManager::entry_to_ui_item(4, &entry4);
        assert_eq!(item4.id, 4);
        assert_eq!(item4.kind, UiHistoryKind::System);
        assert!(item4.text.contains("2 findings"));
        assert!(item4.text.contains("Focus on locking"));
        assert!(item4.text.contains("Locking is racy under load."));
        assert_eq!(item4.summary, Some("Review: 2 findings".to_string()));
        assert_eq!(item4.files, vec![HistoryFileChange::plain("src/lib.rs")]);
    }

    #[test]
    fn test_get_ui_slice_pagination() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut hm = HistoryManager::new(root);

        for i in 1..=5 {
            hm.record_turn(format!("Turn {}", i), vec![]).unwrap();
        }

        // Default last 2 (limit 2, before_id: None)
        let (items, has_more, total) = hm.get_ui_slice(2, None).unwrap();
        assert_eq!(total, 5);
        assert!(has_more);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, 4);
        assert_eq!(items[1].id, 5);

        // Paginate before id 4
        let (items2, has_more2, total2) = hm.get_ui_slice(2, Some(4)).unwrap();
        assert_eq!(total2, 5);
        assert!(has_more2);
        assert_eq!(items2.len(), 2);
        assert_eq!(items2[0].id, 2);
        assert_eq!(items2[1].id, 3);

        // Paginate before id 2
        let (items3, has_more3, total3) = hm.get_ui_slice(2, Some(2)).unwrap();
        assert_eq!(total3, 5);
        assert!(!has_more3);
        assert_eq!(items3.len(), 1);
        assert_eq!(items3[0].id, 1);
    }

    #[test]
    fn test_history_manager_initialization_with_corrupted_lines() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let storage = SessionStorage::new(root.clone());

        let entry1 = HistoryEntry::Turn {
            id: Some(10),
            prompt: "Prompt 10".to_string(),
            context: vec![],
            context_watermark: None,
        };
        let entry2 = HistoryEntry::Turn {
            id: Some(20),
            prompt: "Prompt 20".to_string(),
            context: vec![],
            context_watermark: None,
        };

        storage.append_entry(&entry1).unwrap();
        let history_file = storage.history_path();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&history_file)
            .unwrap();
        writeln!(file, "not valid jsonl").unwrap();
        file.flush().unwrap();
        storage.append_entry(&entry2).unwrap();

        // Create a new HistoryManager and verify max_id and prompt injection
        let mut hm = HistoryManager::new(root);
        assert_eq!(hm.current_max_id, 20);

        let history_tag = hm.format_history_tag().unwrap();
        assert!(history_tag.contains("Prompt 10"));
        assert!(history_tag.contains("Prompt 20"));

        let next_entry = hm.record_turn("Prompt 21", vec![]).unwrap();
        assert_eq!(next_entry.id(), Some(21));
    }

    #[test]
    fn test_listener_receives_typed_mutations() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut hm = HistoryManager::new(root);

        let received: Arc<std::sync::Mutex<Vec<HistoryMutation>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        hm.set_listener(move |mutation| {
            let mut guard = sink.lock().unwrap();
            guard.push(mutation);
        });

        for i in 1..=4 {
            hm.record_turn(format!("Task {}", i), vec![]).unwrap();
            hm.record_response(format!("Done {}", i), None, None, vec![])
                .unwrap();
        }

        // Compaction must be reported as a structural mutation carrying the
        // summary and the ids of the preserved tail, not as another append.
        let (_, tail) = hm.prepare_compaction(2).unwrap().unwrap();
        let expected_tail_ids: Vec<u64> = tail.iter().filter_map(|entry| entry.id()).collect();
        assert_eq!(expected_tail_ids, vec![5, 6, 7, 8]);
        hm.apply_compaction("Earlier tasks summarized.", tail)
            .unwrap();

        let mutations = received.lock().unwrap();
        // Eight appends (four turns, four responses), then one compaction.
        assert_eq!(mutations.len(), 9);
        for mutation in &mutations[..8] {
            assert!(matches!(mutation, HistoryMutation::Appended { .. }));
        }
        match &mutations[8] {
            HistoryMutation::Compacted { summary, tail_ids } => {
                assert_eq!(summary.kind, UiHistoryKind::System);
                assert_eq!(summary.text, "Earlier tasks summarized.");
                assert_eq!(tail_ids, &expected_tail_ids);
            }
            other => panic!("expected a compaction mutation, got {:?}", other),
        }
    }
}
