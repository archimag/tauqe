use anyhow::{Context, Result};
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use super::entry::HistoryEntry;

/// Manages persistent JSONL session history in `.tauqe/history.jsonl`.
#[derive(Debug, Clone)]
pub struct SessionStorage {
    repo_root: PathBuf,
}

impl SessionStorage {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
        }
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn tauqe_dir(&self) -> PathBuf {
        self.repo_root.join(".tauqe")
    }

    pub fn history_path(&self) -> PathBuf {
        self.tauqe_dir().join("history.jsonl")
    }

    pub fn append_entry(&self, entry: &HistoryEntry) -> Result<()> {
        let line = entry.to_jsonl_line()?;
        let dir = self.tauqe_dir();
        let file_path = self.history_path();

        // The write, flush, and `fsync` barrier run off the async executor
        // thread so a slow disk never stalls the bounded turn or the UI.
        crate::blocking::run_blocking(move || -> Result<()> {
            create_dir_all(&dir)
                .with_context(|| format!("Failed to create .tauqe directory: {:?}", dir))?;

            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file_path)
                .with_context(|| {
                    format!("Failed to open history file for appending: {:?}", file_path)
                })?;

            writeln!(file, "{line}")
                .with_context(|| format!("Failed to write history entry to: {:?}", file_path))?;
            file.flush()
                .with_context(|| format!("Failed to flush history file: {:?}", file_path))?;
            file.sync_data()
                .with_context(|| format!("Failed to sync history file: {:?}", file_path))?;

            Ok(())
        })
    }

    pub fn read_entries(&self) -> Result<Vec<HistoryEntry>> {
        let file_path = self.history_path();

        // The metadata probe, the open, and the line-by-line JSONL streaming
        // run off the async executor thread, so a cold or slow disk never
        // stalls the bounded turn, provider streaming, or RPC handling.
        crate::blocking::run_blocking(move || -> Result<Vec<HistoryEntry>> {
            if !file_path.is_file() {
                return Ok(Vec::new());
            }

            let file = File::open(&file_path)
                .with_context(|| format!("Failed to open history file: {:?}", file_path))?;
            let reader = BufReader::new(file);
            let mut entries = Vec::new();

            for (line_idx, line_res) in reader.lines().enumerate() {
                let line = match line_res {
                    Ok(l) => l,
                    Err(err) => {
                        tracing::warn!(
                            "Failed reading line {} from history file {:?}: {}",
                            line_idx + 1,
                            file_path,
                            err
                        );
                        continue;
                    }
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match HistoryEntry::from_jsonl_line(trimmed) {
                    Ok(entry) => entries.push(entry),
                    Err(err) => {
                        tracing::warn!(
                            "Skipping invalid JSONL at line {} in history file {:?}: {}",
                            line_idx + 1,
                            file_path,
                            err
                        );
                    }
                }
            }

            Ok(entries)
        })
    }

    /// Atomically rewrites the history file with the provided entries.
    pub fn rewrite_entries(&self, entries: &[HistoryEntry]) -> Result<()> {
        let mut payload = String::new();
        for entry in entries {
            let line = entry.to_jsonl_line()?;
            payload.push_str(&line);
            payload.push('\n');
        }

        let dir = self.tauqe_dir();
        let target_path = self.history_path();
        let temp_filename = format!(
            "{}.tmp.{}",
            target_path.file_name().and_then(|f| f.to_str()).unwrap_or("history.jsonl"),
            std::process::id()
        );
        let temp_path = dir.join(temp_filename);

        // The write, flush, and `fsync` barrier, plus the atomic rename, run
        // off the async executor thread so a slow disk never stalls the turn.
        crate::blocking::run_blocking(move || -> Result<()> {
            create_dir_all(&dir)
                .with_context(|| format!("Failed to create .tauqe directory: {:?}", dir))?;

            {
                let mut temp_file = File::create(&temp_path).with_context(|| {
                    format!("Failed to create temp history file: {:?}", temp_path)
                })?;

                temp_file.write_all(payload.as_bytes()).with_context(|| {
                    format!("Failed to write to temp history file: {:?}", temp_path)
                })?;
                temp_file.flush().with_context(|| {
                    format!("Failed to flush temp history file: {:?}", temp_path)
                })?;
                temp_file.sync_all().with_context(|| {
                    format!("Failed to sync temp history file: {:?}", temp_path)
                })?;
            }

            std::fs::rename(&temp_path, &target_path).with_context(|| {
                format!(
                    "Failed to atomically replace history file from {:?} to {:?}",
                    temp_path, target_path
                )
            })?;

            Ok(())
        })
    }

    pub fn clear(&self) -> Result<()> {
        let file_path = self.history_path();

        // The existence probe and the deletion run off the async executor
        // thread for the same reason the writes do: history cleanup happens
        // inside the turn pipeline, never on a bare Tokio worker thread.
        crate::blocking::run_blocking(move || -> Result<()> {
            if file_path.exists() {
                std::fs::remove_file(&file_path)
                    .with_context(|| format!("Failed to delete history file: {:?}", file_path))?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_storage_append_read_rewrite_clear() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let storage = SessionStorage::new(repo_root);

        assert!(storage.read_entries().unwrap().is_empty());

        let entry1 = HistoryEntry::Turn {
            id: Some(1),
            prompt: "Implement auth".to_string(),
            context: vec!["auth.rs".to_string()],
        };
        let entry2 = HistoryEntry::Response {
            id: Some(2),
            message: "Added token auth".to_string(),
            commit: Some("c123456".to_string()),
            summary: Some("Add token auth".to_string()),
            files: vec!["auth.rs".to_string()],
        };

        storage.append_entry(&entry1).unwrap();
        storage.append_entry(&entry2).unwrap();

        let entries = storage.read_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], entry1);
        assert_eq!(entries[1], entry2);

        // Test rewrite
        let entry_summary = HistoryEntry::Summary {
            id: Some(1),
            text: "Compacted auth task".to_string(),
        };
        storage.rewrite_entries(&[entry_summary.clone(), entry2.clone()]).unwrap();
        let rewritten = storage.read_entries().unwrap();
        assert_eq!(rewritten, vec![entry_summary, entry2]);

        // Test clear
        storage.clear().unwrap();
        assert!(storage.read_entries().unwrap().is_empty());
    }

    #[test]
    fn test_storage_read_entries_skips_corrupted_lines() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let storage = SessionStorage::new(repo_root);

        let entry1 = HistoryEntry::Turn {
            id: Some(1),
            prompt: "First turn".to_string(),
            context: vec![],
        };
        let entry2 = HistoryEntry::Turn {
            id: Some(2),
            prompt: "Second turn".to_string(),
            context: vec![],
        };

        storage.append_entry(&entry1).unwrap();

        // Inject corrupted lines directly into the file
        let history_file = storage.history_path();
        let mut file = OpenOptions::new().append(true).open(&history_file).unwrap();
        writeln!(file, "{{corrupted json line").unwrap();
        writeln!(file, "{{\"type\":\"unknown_variant\"}}").unwrap();
        file.flush().unwrap();

        storage.append_entry(&entry2).unwrap();

        let entries = storage.read_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], entry1);
        assert_eq!(entries[1], entry2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_storage_read_entries_and_clear_on_multi_thread_runtime() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let storage = SessionStorage::new(repo_root);

        // The missing-file fast path must stay valid off the executor too.
        assert!(storage.read_entries().unwrap().is_empty());

        let entry = HistoryEntry::Turn {
            id: Some(1),
            prompt: "Read history off the executor".to_string(),
            context: vec!["history/storage.rs".to_string()],
        };
        storage.append_entry(&entry).unwrap();

        let entries = storage.read_entries().unwrap();
        assert_eq!(entries, vec![entry]);

        storage.clear().unwrap();
        assert!(storage.read_entries().unwrap().is_empty());
    }
}
