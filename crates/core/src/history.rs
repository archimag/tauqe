pub mod compaction;
pub mod entry;
pub mod storage;

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauqe_protocol::{UiHistoryItem, UiHistoryKind};

pub use compaction::{
    build_compacted_history, estimate_entries_tokens, partition_head_tail,
    DEFAULT_HISTORY_BUDGET_TOKENS, DEFAULT_TAIL_TURNS_COUNT,
};
pub use entry::HistoryEntry;
pub use storage::SessionStorage;

#[derive(Clone)]
pub struct HistoryEntryListener(pub Arc<dyn Fn(UiHistoryItem) + Send + Sync>);

impl std::fmt::Debug for HistoryEntryListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HistoryEntryListener(..)")
    }
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
        F: Fn(UiHistoryItem) + Send + Sync + 'static,
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
            },
            HistoryEntry::Turn { prompt, context, .. } => UiHistoryItem {
                id: final_id,
                kind: UiHistoryKind::User,
                text: prompt.clone(),
                summary: None,
                commit_hash: None,
                files: context.clone(),
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
                files: files.clone(),
            },
            HistoryEntry::Undo {
                commit,
                restored_checkpoint,
                ..
            } => {
                let text = if *restored_checkpoint {
                    format!("Undid commit {} and restored uncommitted changes", commit)
                } else {
                    format!("Undid commit {}", commit)
                };
                UiHistoryItem {
                    id: final_id,
                    kind: UiHistoryKind::System,
                    text,
                    summary: None,
                    commit_hash: Some(commit.clone()),
                    files: Vec::new(),
                }
            }
            HistoryEntry::Review {
                model,
                user_prompt,
                files,
                findings_count,
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
                UiHistoryItem {
                    id: final_id,
                    kind: UiHistoryKind::System,
                    text,
                    summary: Some(format!("Review: {} findings", findings_count)),
                    commit_hash: None,
                    files: files.clone(),
                }
            }
        }
    }

    fn notify_entry(&self, entry: &HistoryEntry) {
        if let Some(listener) = &self.listener {
            let id = entry.id().unwrap_or(self.current_max_id);
            let item = Self::entry_to_ui_item(id, entry);
            (listener.0)(item);
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
        };
        self.storage.append_entry(&entry)?;
        self.notify_entry(&entry);
        Ok(entry)
    }

    pub fn record_response(
        &mut self,
        message: impl Into<String>,
        commit: Option<String>,
        summary: Option<String>,
        files: Vec<String>,
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
        self.notify_entry(&entry);
        Ok(entry)
    }

    pub fn record_undo(
        &mut self,
        commit: impl Into<String>,
        restored_checkpoint: bool,
    ) -> Result<HistoryEntry> {
        self.current_max_id += 1;
        let entry = HistoryEntry::Undo {
            id: Some(self.current_max_id),
            commit: commit.into(),
            restored_checkpoint,
        };
        self.storage.append_entry(&entry)?;
        self.notify_entry(&entry);
        Ok(entry)
    }

    pub fn record_review(
        &mut self,
        model: impl Into<String>,
        user_prompt: Option<String>,
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
        };
        self.storage.append_entry(&entry)?;
        self.notify_entry(&entry);
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
    /// Returns an empty string if there are no history entries.
    pub fn format_history_tag(&self) -> Result<String> {
        let entries = self.get_entries()?;
        if entries.is_empty() {
            return Ok(String::new());
        }

        let mut output = String::from("<history>\n");
        for entry in &entries {
            output.push_str(&entry.to_jsonl_line()?);
            output.push('\n');
        }
        output.push_str("</history>");
        Ok(output)
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
        let summary_id = tail.first().and_then(|e| e.id()).map(|id| id.saturating_sub(1));
        let compacted = build_compacted_history(summary_id, summary_text.into(), tail);
        self.storage.rewrite_entries(&compacted)?;
        if let Some(summary_entry) = compacted.first() {
            if let Some(listener) = &self.listener {
                let id = summary_entry.id().unwrap_or(1);
                let item = Self::entry_to_ui_item(id, summary_entry);
                (listener.0)(item);
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
    use tempfile::tempdir;

    #[test]
    fn test_history_manager_flow() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut hm = HistoryManager::new(root);

        assert_eq!(hm.format_history_tag().unwrap(), "");

        hm.record_turn("Check git status", vec!["src/git.rs".to_string()]).unwrap();
        hm.record_response(
            "Checked git status",
            Some("f1a2b3c".to_string()),
            Some("Fix status check".to_string()),
            vec!["src/git.rs".to_string()],
        ).unwrap();
        hm.record_undo("f1a2b3c", true).unwrap();

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 3);

        let tag = hm.format_history_tag().unwrap();
        assert!(tag.starts_with("<history>\n"));
        assert!(tag.ends_with("\n</history>"));
        assert!(tag.contains(r#"{"type":"turn","id":1,"prompt":"Check git status","context":["src/git.rs"]}"#));
        assert!(tag.contains(r#"{"type":"undo","id":3,"commit":"f1a2b3c","restored_checkpoint":true}"#));

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
            hm.record_response(format!("Done {}", i), None, None, vec![]).unwrap();
        }

        let (head, tail) = hm.prepare_compaction(2).unwrap().unwrap();
        assert_eq!(head.len(), 4);
        assert_eq!(tail.len(), 4);

        hm.apply_compaction("Tasks 1 and 2 completed successfully.", tail).unwrap();

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
    fn test_entry_to_ui_item() {
        let entry1 = HistoryEntry::Summary { id: Some(1), text: "Summary text".to_string() };
        let item1 = HistoryManager::entry_to_ui_item(1, &entry1);
        assert_eq!(item1.id, 1);
        assert_eq!(item1.kind, UiHistoryKind::System);
        assert_eq!(item1.text, "Summary text");

        let entry2 = HistoryEntry::Turn {
            id: Some(2),
            prompt: "Write tests".to_string(),
            context: vec!["test.rs".to_string()],
        };
        let item2 = HistoryManager::entry_to_ui_item(2, &entry2);
        assert_eq!(item2.id, 2);
        assert_eq!(item2.kind, UiHistoryKind::User);
        assert_eq!(item2.text, "Write tests");
        assert_eq!(item2.files, vec!["test.rs"]);

        let entry3 = HistoryEntry::Response {
            id: Some(3),
            message: "Done".to_string(),
            commit: Some("a1b2c3d".to_string()),
            summary: Some("Add tests".to_string()),
            files: vec!["test.rs".to_string()],
        };
        let item3 = HistoryManager::entry_to_ui_item(3, &entry3);
        assert_eq!(item3.id, 3);
        assert_eq!(item3.kind, UiHistoryKind::Assistant);
        assert_eq!(item3.text, "Done");
        assert_eq!(item3.commit_hash, Some("a1b2c3d".to_string()));
        assert_eq!(item3.summary, Some("Add tests".to_string()));
        assert_eq!(item3.files, vec!["test.rs"]);

        let entry4 = HistoryEntry::Undo {
            id: Some(4),
            commit: "a1b2c3d".to_string(),
            restored_checkpoint: true,
        };
        let item4 = HistoryManager::entry_to_ui_item(4, &entry4);
        assert_eq!(item4.id, 4);
        assert_eq!(item4.kind, UiHistoryKind::System);
        assert!(item4.text.contains("a1b2c3d"));
        assert_eq!(item4.commit_hash, Some("a1b2c3d".to_string()));
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
}
