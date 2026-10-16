use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSession, ReviewSummary};

pub struct ReviewStorage;

/// Process-global lock serializing review storage mutations.
///
/// Review storage performs non-atomic read-modify-write cycles (load, mutate, save).
/// Without serialization, concurrent tasks (UI RPC handlers and batch execution)
/// race on the same review file and silently overwrite each other's changes.
static REVIEW_STORAGE_LOCK: Mutex<()> = Mutex::new(());

impl ReviewStorage {
    /// Acquires the process-global review storage lock.
    ///
    /// Recovers the guard if the lock was poisoned by a previous panic, so a single
    /// failure never permanently wedges review storage.
    fn lock() -> MutexGuard<'static, ()> {
        REVIEW_STORAGE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Returns the directory path for local review sessions: `<repo_root>/.tauqe/reviews`.
    pub fn reviews_dir(repo_root: &Path) -> PathBuf {
        repo_root.join(".tauqe").join("reviews")
    }

    /// Sanitizes an arbitrary ID into a safe file name.
    pub fn sanitize_id(id: &str) -> String {
        let sanitized: String = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let trimmed = sanitized.trim_matches('-');
        if trimmed.is_empty() {
            "default".to_string()
        } else {
            trimmed.to_string()
        }
    }

    /// Migrates legacy monolithic `review.json` or `latest.json` files into the
    /// multi-file `.tauqe/reviews/{id}.json` layout.
    ///
    /// Migration is a startup-only, one-shot operation: read paths never invoke
    /// it, so listing and loading reviews carry no write side effects (CQS).
    /// Every failure is logged and degrades gracefully, leaving the legacy
    /// source file in place instead of aborting the process.
    pub fn migrate_legacy_if_needed(repo_root: &Path) {
        let _guard = Self::lock();
        Self::migrate_legacy_if_needed_unlocked(repo_root);
    }

    /// Lock-free core of [`Self::migrate_legacy_if_needed`]; caller must hold storage lock.
    fn migrate_legacy_if_needed_unlocked(repo_root: &Path) {
        let dir = Self::reviews_dir(repo_root);

        // 1. Legacy `.tauqe/review.json`
        let legacy_root = repo_root.join(".tauqe").join("review.json");
        if let Some(mut session) = Self::read_legacy_session(&legacy_root) {
            if session.id.trim().is_empty() {
                session.id = "migrated-review".to_string();
            }
            Self::store_migrated_session_unlocked(repo_root, &session, &legacy_root);
        }

        // 2. Legacy `.tauqe/reviews/review.json`
        let legacy_reviews_file = dir.join("review.json");
        if let Some(mut session) = Self::read_legacy_session(&legacy_reviews_file) {
            if session.id.trim().is_empty() {
                session.id = "default".to_string();
            }
            Self::store_migrated_session_unlocked(repo_root, &session, &legacy_reviews_file);
        }

        // 3. Legacy `.tauqe/reviews/latest.json`
        let legacy_latest_file = dir.join("latest.json");
        if let Some(mut session) = Self::read_legacy_session(&legacy_latest_file) {
            if session.id.trim().is_empty() || session.id == "latest" {
                session.id = format!("review-{}", session.created_at);
            }
            if Self::store_migrated_session_unlocked(repo_root, &session, &legacy_latest_file) {
                let active_file = dir.join("active");
                if !active_file.is_file() {
                    if let Err(err) = Self::save_active_id_unlocked(repo_root, Some(&session.id)) {
                        tracing::warn!(
                            "Failed to mark migrated review '{}' as active: {:#}",
                            session.id,
                            err
                        );
                    }
                }
            }
        }
    }

    /// Reads and parses a legacy review file.
    ///
    /// Returns `None` when the file is absent, and logs a diagnostic (without
    /// propagating) when it exists but cannot be read or parsed, so a single
    /// malformed legacy file never blocks startup.
    fn read_legacy_session(legacy_file: &Path) -> Option<ReviewSession> {
        let content = match std::fs::read_to_string(legacy_file) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                tracing::warn!(
                    "Failed to read legacy review file {}: {}",
                    legacy_file.display(),
                    err
                );
                return None;
            }
        };
        match serde_json::from_str::<ReviewSession>(&content) {
            Ok(session) => Some(session),
            Err(err) => {
                tracing::warn!(
                    "Failed to parse legacy review file {}: {}",
                    legacy_file.display(),
                    err
                );
                None
            }
        }
    }

    /// Stores a migrated session in the multi-file layout and removes the legacy
    /// source file once the target exists.
    ///
    /// Returns `true` when the target session file is present after the call.
    /// Failures are logged; the legacy source is preserved on failure so no
    /// review data is silently lost.
    fn store_migrated_session_unlocked(
        repo_root: &Path,
        session: &ReviewSession,
        legacy_file: &Path,
    ) -> bool {
        let dir = Self::reviews_dir(repo_root);
        let safe_id = Self::sanitize_id(&session.id);
        let target_file = dir.join(format!("{safe_id}.json"));

        if !target_file.is_file() {
            if let Err(err) = Self::save_session_unlocked(repo_root, session) {
                tracing::warn!(
                    "Failed to migrate legacy review file {}: {:#}",
                    legacy_file.display(),
                    err
                );
                return false;
            }
        }

        // `save_session_unlocked` reuses whichever file already stores this id, so
        // when the legacy file still embeds the session id the save lands on that
        // legacy path instead of the canonical one. Rename the artifact onto its
        // canonical id-based name so the cleanup below never deletes the only copy.
        if legacy_file != target_file && !target_file.is_file() && legacy_file.is_file() {
            if let Err(err) = std::fs::rename(legacy_file, &target_file) {
                tracing::warn!(
                    "Failed to rename migrated review file {} to {}: {}",
                    legacy_file.display(),
                    target_file.display(),
                    err
                );
            }
        }

        // Discard the legacy source only once the canonical file is safely in place.
        if legacy_file != target_file && target_file.is_file() && legacy_file.is_file() {
            if let Err(err) = std::fs::remove_file(legacy_file) {
                tracing::warn!(
                    "Migrated legacy review file {} but failed to remove it: {}",
                    legacy_file.display(),
                    err
                );
            }
        }
        true
    }

    /// Lists summaries of all review sessions, ordered by `created_at` descending.
    pub fn list_summaries(repo_root: &Path) -> Result<Vec<ReviewSummary>> {
        let reviews = Self::load_all(repo_root)?;
        Ok(reviews.into_iter().map(|r| r.summary()).collect())
    }

    /// Lists all review sessions from `.tauqe/reviews`, ordered by `created_at` descending.
    pub fn list_reviews(repo_root: &Path) -> Result<Vec<ReviewSession>> {
        Self::load_all(repo_root)
    }

    /// Loads all review sessions from disk.
    ///
    /// Acquires the process-global review storage lock, so a read never races a
    /// concurrent atomic rename in a write path.
    pub fn load_all(repo_root: &Path) -> Result<Vec<ReviewSession>> {
        let _guard = Self::lock();
        Self::load_all_unlocked(repo_root)
    }

    /// Lock-free core of [`Self::load_all`]; the caller must hold the storage lock.
    fn load_all_unlocked(repo_root: &Path) -> Result<Vec<ReviewSession>> {
        let dir = Self::reviews_dir(repo_root);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut reviews = Vec::new();
        let entries = std::fs::read_dir(&dir)
            .with_context(|| format!("Failed to read reviews directory at {}", dir.display()))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("json") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<ReviewSession>(&content) {
                        Ok(review) => reviews.push(review),
                        Err(err) => tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "Skipping corrupted review file"
                        ),
                    },
                    Err(err) => tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "Failed to read review file"
                    ),
                }
            }
        }

        reviews.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        Ok(reviews)
    }

    /// Resolves the on-disk file that currently stores the review session identified
    /// by `id`, or `None` when no stored session matches.
    ///
    /// The storage file name is normally `sanitize_id(id)`, but an id change (for
    /// example a casing change) leaves the session under a differently sanitized
    /// name. Resolving the real file keeps save and delete operating on the existing
    /// artifact instead of creating or leaving behind a duplicate session file.
    ///
    /// Lock-free; the caller must hold the storage lock.
    fn find_session_file_unlocked(repo_root: &Path, id: &str) -> Option<PathBuf> {
        let dir = Self::reviews_dir(repo_root);
        let safe_id = Self::sanitize_id(id);
        let default_target = dir.join(format!("{safe_id}.json"));
        if default_target.is_file() {
            if let Ok(content) = std::fs::read_to_string(&default_target) {
                if let Ok(session) = serde_json::from_str::<ReviewSession>(&content) {
                    if session.id.eq_ignore_ascii_case(id) {
                        return Some(default_target);
                    }
                }
            }
        }

        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path == default_target
                || !path.is_file()
                || path.extension().and_then(|s| s.to_str()) != Some("json")
            {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(_) => continue,
            };
            let session = match serde_json::from_str::<ReviewSession>(&content) {
                Ok(session) => session,
                Err(_) => continue,
            };
            if session.id.eq_ignore_ascii_case(id) {
                return Some(path);
            }
        }
        None
    }

    /// Loads a specific review session by ID.
    ///
    /// Acquires the process-global review storage lock, so a read never races a
    /// concurrent atomic rename in a write path.
    pub fn load_review(repo_root: &Path, id: &str) -> Result<Option<ReviewSession>> {
        let _guard = Self::lock();
        Self::load_review_unlocked(repo_root, id)
    }

    /// Lock-free core of [`Self::load_review`]; the caller must hold the storage lock.
    fn load_review_unlocked(repo_root: &Path, id: &str) -> Result<Option<ReviewSession>> {
        let file_path = match Self::find_session_file_unlocked(repo_root, id) {
            Some(path) => path,
            None => return Ok(None),
        };

        let content = std::fs::read_to_string(&file_path)
            .with_context(|| format!("Failed to read review file at {}", file_path.display()))?;
        let session: ReviewSession = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse review JSON at {}", file_path.display()))?;
        Ok(Some(session))
    }

    /// Loads the active or latest review session.
    pub fn load_latest(repo_root: &Path) -> Result<Option<ReviewSession>> {
        let _guard = Self::lock();
        Self::load_latest_unlocked(repo_root)
    }

    /// Lock-free core of [`Self::load_latest`]; the caller must hold the storage lock.
    fn load_latest_unlocked(repo_root: &Path) -> Result<Option<ReviewSession>> {
        // 1. Try active review marker
        if let Ok(Some(active_id)) = Self::load_active_id_unlocked(repo_root) {
            if let Ok(Some(session)) = Self::load_review_unlocked(repo_root, &active_id) {
                return Ok(Some(session));
            }
        }

        // 2. Fall back to most recent review
        let all = Self::load_all_unlocked(repo_root)?;
        Ok(all.into_iter().next())
    }

    /// Saves a review session into `.tauqe/reviews/<id>.json` atomically.
    pub fn save_session(repo_root: &Path, session: &ReviewSession) -> Result<()> {
        let _guard = Self::lock();
        Self::save_session_unlocked(repo_root, session)
    }

    /// Lock-free core of [`Self::save_session`]; caller must hold storage lock.
    fn save_session_unlocked(repo_root: &Path, session: &ReviewSession) -> Result<()> {
        let dir = Self::reviews_dir(repo_root);
        std::fs::create_dir_all(&dir).with_context(|| {
            format!("Failed to create reviews directory at {}", dir.display())
        })?;

        let safe_id = Self::sanitize_id(&session.id);
        let default_target = dir.join(format!("{safe_id}.json"));

        // Reuse the file that already stores this session even when its name differs
        // from the sanitized id, so saving never spawns a duplicate session file.
        let target_file = Self::find_session_file_unlocked(repo_root, &session.id)
            .unwrap_or(default_target);
        let temp_file = target_file.with_extension("json.tmp");

        // Refuse to overwrite a file that already stores a different review session
        // whose sanitized storage name collides with this one (for example
        // "team/auth" and "team-auth" both sanitize to "team-auth.json"), so a save
        // never silently replaces an unrelated session.
        if target_file.is_file() {
            let content = std::fs::read_to_string(&target_file).with_context(|| {
                format!("Failed to read existing review file at {}", target_file.display())
            })?;
            let existing: ReviewSession = serde_json::from_str(&content).with_context(|| {
                format!("Failed to parse existing review JSON at {}", target_file.display())
            })?;
            if !existing.id.eq_ignore_ascii_case(&session.id) {
                anyhow::bail!(
                    "Cannot save review session '{}': storage name '{}.json' is already used by review session '{}'; choose a distinct review id",
                    session.id,
                    safe_id,
                    existing.id
                );
            }
        }

        let json = serde_json::to_string_pretty(session)
            .context("Failed to serialize review session to JSON")?;
        std::fs::write(&temp_file, json).with_context(|| {
            format!("Failed to write temporary review file at {}", temp_file.display())
        })?;

        std::fs::rename(&temp_file, &target_file).with_context(|| {
            format!("Failed to atomically rename review file to {}", target_file.display())
        })?;

        // If no active review is set, make this session active
        if let Ok(None) = Self::load_active_id_unlocked(repo_root) {
            let _ = Self::save_active_id_unlocked(repo_root, Some(&session.id));
        }

        Ok(())
    }

    /// Deletes a review session from storage. Returns `true` if a file was deleted.
    pub fn delete_review(repo_root: &Path, id: &str) -> Result<bool> {
        let _guard = Self::lock();
        Self::delete_review_unlocked(repo_root, id)
    }

    /// Lock-free core of [`Self::delete_review`]; caller must hold storage lock.
    fn delete_review_unlocked(repo_root: &Path, id: &str) -> Result<bool> {
        let dir = Self::reviews_dir(repo_root);
        let safe_id = Self::sanitize_id(id);

        // Delete the file that actually stores this session. When the session lives
        // under a differently sanitized name, removing only the default target would
        // leave the real artifact behind. If default target exists but stores a different
        // session (collision), ensure we never delete the unrelated file.
        let target_file = match Self::find_session_file_unlocked(repo_root, id) {
            Some(existing) => existing,
            None => {
                let default_target = dir.join(format!("{safe_id}.json"));
                if default_target.is_file() {
                    if let Ok(content) = std::fs::read_to_string(&default_target) {
                        if let Ok(session) = serde_json::from_str::<ReviewSession>(&content) {
                            if !session.id.eq_ignore_ascii_case(id) {
                                return Ok(false);
                            }
                        }
                    }
                    default_target
                } else {
                    return Ok(false);
                }
            }
        };

        std::fs::remove_file(&target_file)
            .with_context(|| format!("Failed to remove review file at {}", target_file.display()))?;

        if let Ok(Some(active)) = Self::load_active_id_unlocked(repo_root) {
            if active.eq_ignore_ascii_case(id) || active.eq_ignore_ascii_case(&safe_id) {
                let _ = Self::save_active_id_unlocked(repo_root, None);
            }
        }
        Ok(true)
    }

    /// Loads the active review ID from `.tauqe/reviews/active`.
    pub fn load_active_id(repo_root: &Path) -> Result<Option<String>> {
        let _guard = Self::lock();
        Self::load_active_id_unlocked(repo_root)
    }

    /// Lock-free core of [`Self::load_active_id`]; the caller must hold the storage lock.
    fn load_active_id_unlocked(repo_root: &Path) -> Result<Option<String>> {
        let active_file = Self::reviews_dir(repo_root).join("active");
        if !active_file.is_file() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&active_file)
            .with_context(|| format!("Failed to read active review marker at {}", active_file.display()))?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    /// Saves or clears the active review ID in `.tauqe/reviews/active`.
    pub fn save_active_id(repo_root: &Path, id: Option<&str>) -> Result<()> {
        let _guard = Self::lock();
        Self::save_active_id_unlocked(repo_root, id)
    }

    /// Lock-free core of [`Self::save_active_id`]; caller must hold storage lock.
    fn save_active_id_unlocked(repo_root: &Path, id: Option<&str>) -> Result<()> {
        let dir = Self::reviews_dir(repo_root);
        std::fs::create_dir_all(&dir).with_context(|| {
            format!("Failed to create reviews directory at {}", dir.display())
        })?;

        let active_file = dir.join("active");
        match id {
            Some(val) if !val.trim().is_empty() => {
                std::fs::write(&active_file, val.trim())
                    .with_context(|| format!("Failed to write active review marker to {}", active_file.display()))?;
            }
            _ => {
                if active_file.is_file() {
                    let _ = std::fs::remove_file(&active_file);
                }
            }
        }
        Ok(())
    }

    /// Updates a review item by id within a specific session (or active/latest session if None).
    pub fn update_session_item<F>(
        repo_root: &Path,
        review_id: Option<&str>,
        item_id: u32,
        updater: F,
    ) -> Result<Option<ReviewItem>>
    where
        F: FnOnce(&mut ReviewItem),
    {
        let _guard = Self::lock();
        Self::update_session_item_unlocked(repo_root, review_id, item_id, updater)
    }

    /// Lock-free core of [`Self::update_session_item`]; caller must hold storage lock.
    fn update_session_item_unlocked<F>(
        repo_root: &Path,
        review_id: Option<&str>,
        item_id: u32,
        updater: F,
    ) -> Result<Option<ReviewItem>>
    where
        F: FnOnce(&mut ReviewItem),
    {
        let mut session = match review_id {
            Some(id) => match Self::load_review_unlocked(repo_root, id)? {
                Some(s) => s,
                None => return Ok(None),
            },
            None => match Self::load_latest_unlocked(repo_root)? {
                Some(s) => s,
                None => return Ok(None),
            },
        };

        let updated_item = if let Some(item) = session.items.iter_mut().find(|it| it.id == item_id) {
            updater(item);
            Some(item.clone())
        } else {
            None
        };

        if updated_item.is_some() {
            Self::save_session_unlocked(repo_root, &session)?;
        }

        Ok(updated_item)
    }

    /// Updates status of a review item within a specific review session (or latest).
    pub fn update_session_item_status(
        repo_root: &Path,
        review_id: Option<&str>,
        item_id: u32,
        status: ReviewItemStatus,
    ) -> Result<Option<ReviewItem>> {
        Self::update_session_item(repo_root, review_id, item_id, |item| {
            item.status = status;
        })
    }

    /// Convenience helper for latest session item update.
    pub fn update_item<F>(repo_root: &Path, item_id: u32, updater: F) -> Result<Option<ReviewItem>>
    where
        F: FnOnce(&mut ReviewItem),
    {
        Self::update_session_item(repo_root, None, item_id, updater)
    }

    /// Convenience helper for latest session item status update.
    pub fn update_item_status(
        repo_root: &Path,
        item_id: u32,
        status: ReviewItemStatus,
    ) -> Result<Option<ReviewItem>> {
        Self::update_session_item_status(repo_root, None, item_id, status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::ReviewSeverity;
    use tempfile::tempdir;

    #[test]
    fn test_review_storage_crud_and_active() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let session = ReviewSession {
            id: "rev-sec-1".to_string(),
            title: "Security Audit".to_string(),
            created_at: 1000,
            model: tauqe_protocol::ModelRef::openrouter("anthropic/claude-3.7-sonnet"),
            description: Some("Initial security scan".to_string()),
            user_prompt: Some("focus on security".to_string()),
            target_files: vec!["crates/core/src/lib.rs".to_string()],
            items: vec![ReviewItem {
                id: 1,
                title: "Security finding".to_string(),
                model: None,
                severity: ReviewSeverity::Critical,
                status: ReviewItemStatus::Discussion,
                files: vec!["crates/core/src/lib.rs".to_string()],
                subplan: Vec::new(),
                resolution_notes: None,
                body: "Check input validation".to_string(),
            }],
            raw_markdown: "## [CRITICAL] Security finding\n...".to_string(),
        };

        // 1. Save
        ReviewStorage::save_session(root, &session).unwrap();

        // 2. List
        let list = ReviewStorage::list_reviews(root).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "rev-sec-1");

        // 3. Load by ID and Load Latest
        let loaded = ReviewStorage::load_review(root, "rev-sec-1").unwrap().unwrap();
        assert_eq!(loaded.title, "Security Audit");

        let latest = ReviewStorage::load_latest(root).unwrap().unwrap();
        assert_eq!(latest.id, "rev-sec-1");

        // 4. Update item status to Fixed
        let updated = ReviewStorage::update_session_item_status(
            root,
            Some("rev-sec-1"),
            1,
            ReviewItemStatus::Fixed,
        )
        .unwrap()
        .unwrap();
        assert_eq!(updated.status, ReviewItemStatus::Fixed);

        // 5. Active marker
        assert_eq!(ReviewStorage::load_active_id(root).unwrap(), Some("rev-sec-1".to_string()));
        ReviewStorage::save_active_id(root, None).unwrap();
        assert_eq!(ReviewStorage::load_active_id(root).unwrap(), None);

        // 6. Delete
        let deleted = ReviewStorage::delete_review(root, "rev-sec-1").unwrap();
        assert!(deleted);
        assert_eq!(ReviewStorage::list_reviews(root).unwrap().len(), 0);
    }

    #[test]
    fn test_legacy_migration() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let reviews_dir = ReviewStorage::reviews_dir(root);
        std::fs::create_dir_all(&reviews_dir).unwrap();

        let legacy_file = reviews_dir.join("latest.json");
        let legacy_raw_json = r#"{
            "id": "legacy-session-9",
            "title": "Legacy Audit",
            "created_at": 12345,
            "model": "test-model",
            "target_files": [],
            "items": [],
            "raw_markdown": ""
        }"#;
        std::fs::write(&legacy_file, legacy_raw_json).unwrap();

        // Migration is a startup-only operation: it no longer runs on reads.
        ReviewStorage::migrate_legacy_if_needed(root);

        let loaded = ReviewStorage::load_latest(root).unwrap().unwrap();
        assert_eq!(loaded.id, "legacy-session-9");
        assert!(!legacy_file.is_file());
        assert!(reviews_dir.join("legacy-session-9.json").is_file());
    }

    #[test]
    fn test_concurrent_review_updates_are_serialized_without_lost_writes() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let items: Vec<ReviewItem> = (0..16)
            .map(|i| ReviewItem {
                id: i,
                title: format!("Item {i}"),
                model: None,
                severity: tauqe_protocol::ReviewSeverity::Suggestion,
                status: ReviewItemStatus::Todo,
                files: vec![],
                subplan: Vec::new(),
                resolution_notes: None,
                body: "Check".to_string(),
            })
            .collect();

        let session = ReviewSession {
            id: "concurrent-review".to_string(),
            title: "Concurrent Review".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items,
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(&root, &session).unwrap();

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let _ = ReviewStorage::update_session_item_status(
                        &root,
                        Some("concurrent-review"),
                        i,
                        ReviewItemStatus::Fixed,
                    );
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let loaded = ReviewStorage::load_review(&root, "concurrent-review")
            .unwrap()
            .unwrap();
        let fixed = loaded
            .items
            .iter()
            .filter(|item| item.status == ReviewItemStatus::Fixed)
            .count();
        assert_eq!(fixed, 16);
    }

    #[test]
    fn test_load_all_skips_corrupted_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let session = ReviewSession {
            id: "valid-review".to_string(),
            title: "Valid Review".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(root, &session).unwrap();

        let corrupt_path = ReviewStorage::reviews_dir(root).join("broken.json");
        std::fs::write(&corrupt_path, b"{ not a valid review session }").unwrap();

        let all = ReviewStorage::load_all(root).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "valid-review");
    }

    #[test]
    fn test_save_session_reuses_differing_filename_instead_of_duplicating() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let first = ReviewSession {
            id: "Review One".to_string(),
            title: "First".to_string(),
            created_at: 1,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(root, &first).unwrap();
        assert!(ReviewStorage::reviews_dir(root).join("Review-One.json").is_file());

        // A casing change yields a different sanitized file name; the existing file
        // must be reused instead of writing a second `review-one.json`.
        let second = ReviewSession {
            id: "review one".to_string(),
            title: "Second".to_string(),
            created_at: 2,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(root, &second).unwrap();

        let json_files: Vec<_> = std::fs::read_dir(ReviewStorage::reviews_dir(root))
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        assert_eq!(
            json_files.len(),
            1,
            "expected a single review file, found {json_files:?}"
        );

        let loaded = ReviewStorage::load_review(root, "review one").unwrap().unwrap();
        assert_eq!(loaded.created_at, 2);
    }

    #[test]
    fn test_delete_review_resolves_differing_filename() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let session = ReviewSession {
            id: "Legacy Review".to_string(),
            title: "Legacy".to_string(),
            created_at: 1,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(root, &session).unwrap();
        let stored = ReviewStorage::reviews_dir(root).join("Legacy-Review.json");
        assert!(stored.is_file());

        let deleted = ReviewStorage::delete_review(root, "legacy review").unwrap();
        assert!(deleted);
        assert!(!stored.is_file());
    }

    #[test]
    fn test_save_session_detects_sanitized_id_collision() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let first = ReviewSession {
            id: "team/auth".to_string(),
            title: "Team Auth".to_string(),
            created_at: 1,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(root, &first).unwrap();

        // "team-auth" sanitizes to the same storage name as "team/auth".
        let colliding = ReviewSession {
            id: "team-auth".to_string(),
            title: "Team Auth Alternate".to_string(),
            created_at: 2,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![],
            raw_markdown: String::new(),
        };
        let err = ReviewStorage::save_session(root, &colliding).unwrap_err();
        assert!(err
            .to_string()
            .contains("already used by review session 'team/auth'"));

        // The original session is preserved unchanged.
        let loaded = ReviewStorage::load_review(root, "team/auth").unwrap().unwrap();
        assert_eq!(loaded.id, "team/auth");
        assert_eq!(loaded.title, "Team Auth");
    }

    #[test]
    fn test_load_and_delete_respect_sanitized_id_collision() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let session = ReviewSession {
            id: "team/auth".to_string(),
            title: "Team Auth".to_string(),
            created_at: 1,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(root, &session).unwrap();

        // Loading a non-existent colliding ID must return None rather than the colliding session.
        let result = ReviewStorage::load_review(root, "team-auth").unwrap();
        assert!(result.is_none());

        // Deleting a non-existent colliding ID must return false and not delete the existing session.
        let deleted = ReviewStorage::delete_review(root, "team-auth").unwrap();
        assert!(!deleted);

        let still_exists = ReviewStorage::load_review(root, "team/auth").unwrap();
        assert!(still_exists.is_some());
    }
}
