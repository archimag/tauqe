use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSession, ReviewSummary};

pub struct ReviewStorage;

impl ReviewStorage {
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

    /// Automatically migrates legacy monolithic `review.json` or `latest.json` into multi-file `.tauqe/reviews/{id}.json`.
    pub fn migrate_legacy_if_needed(repo_root: &Path) -> Result<()> {
        let dir = Self::reviews_dir(repo_root);

        // 1. Check legacy `.tauqe/review.json`
        let legacy_root = repo_root.join(".tauqe").join("review.json");
        if legacy_root.is_file() {
            if let Ok(content) = std::fs::read_to_string(&legacy_root) {
                if let Ok(mut session) = serde_json::from_str::<ReviewSession>(&content) {
                    if session.id.trim().is_empty() {
                        session.id = "migrated-review".to_string();
                    }
                    let _ = Self::save_session(repo_root, &session);
                    let _ = std::fs::remove_file(&legacy_root);
                }
            }
        }

        // 2. Check legacy `.tauqe/reviews/review.json`
        let legacy_reviews_file = dir.join("review.json");
        if legacy_reviews_file.is_file() {
            if let Ok(content) = std::fs::read_to_string(&legacy_reviews_file) {
                if let Ok(mut session) = serde_json::from_str::<ReviewSession>(&content) {
                    if session.id.trim().is_empty() {
                        session.id = "default".to_string();
                    }
                    let _ = Self::save_session(repo_root, &session);
                    let _ = std::fs::remove_file(&legacy_reviews_file);
                }
            }
        }

        // 3. Check legacy `.tauqe/reviews/latest.json`
        let legacy_latest_file = dir.join("latest.json");
        if legacy_latest_file.is_file() {
            if let Ok(content) = std::fs::read_to_string(&legacy_latest_file) {
                if let Ok(mut session) = serde_json::from_str::<ReviewSession>(&content) {
                    if session.id.trim().is_empty() || session.id == "latest" {
                        session.id = format!("review-{}", session.created_at);
                    }
                    let safe_id = Self::sanitize_id(&session.id);
                    let target_file = dir.join(format!("{safe_id}.json"));
                    if !target_file.is_file() {
                        let _ = Self::save_session(repo_root, &session);
                    }
                    let active_file = dir.join("active");
                    if !active_file.is_file() {
                        let _ = Self::save_active_id(repo_root, Some(&session.id));
                    }
                    let _ = std::fs::remove_file(&legacy_latest_file);
                }
            }
        }

        Ok(())
    }

    /// Lists summaries of all review sessions, ordered by `created_at` descending.
    pub fn list_summaries(repo_root: &Path) -> Result<Vec<ReviewSummary>> {
        let _ = Self::migrate_legacy_if_needed(repo_root);
        let reviews = Self::load_all(repo_root)?;
        Ok(reviews.into_iter().map(|r| r.summary()).collect())
    }

    /// Lists all review sessions from `.tauqe/reviews`, ordered by `created_at` descending.
    pub fn list_reviews(repo_root: &Path) -> Result<Vec<ReviewSession>> {
        Self::load_all(repo_root)
    }

    /// Loads all review sessions from disk.
    pub fn load_all(repo_root: &Path) -> Result<Vec<ReviewSession>> {
        let _ = Self::migrate_legacy_if_needed(repo_root);
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
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(review) = serde_json::from_str::<ReviewSession>(&content) {
                        reviews.push(review);
                    }
                }
            }
        }

        reviews.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        Ok(reviews)
    }

    /// Loads a specific review session by ID.
    pub fn load_review(repo_root: &Path, id: &str) -> Result<Option<ReviewSession>> {
        let _ = Self::migrate_legacy_if_needed(repo_root);
        let dir = Self::reviews_dir(repo_root);
        let safe_id = Self::sanitize_id(id);
        let file_path = dir.join(format!("{safe_id}.json"));

        if !file_path.is_file() {
            // Check if any existing review matches the id case-insensitively
            let all = Self::load_all(repo_root)?;
            for rev in all {
                if rev.id.eq_ignore_ascii_case(id) {
                    return Ok(Some(rev));
                }
            }
            return Ok(None);
        }

        let content = std::fs::read_to_string(&file_path)
            .with_context(|| format!("Failed to read review file at {}", file_path.display()))?;
        let session: ReviewSession = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse review JSON at {}", file_path.display()))?;
        Ok(Some(session))
    }

    /// Loads the active or latest review session.
    pub fn load_latest(repo_root: &Path) -> Result<Option<ReviewSession>> {
        let _ = Self::migrate_legacy_if_needed(repo_root);

        // 1. Try active review marker
        if let Ok(Some(active_id)) = Self::load_active_id(repo_root) {
            if let Ok(Some(session)) = Self::load_review(repo_root, &active_id) {
                return Ok(Some(session));
            }
        }

        // 2. Fall back to most recent review
        let all = Self::load_all(repo_root)?;
        Ok(all.into_iter().next())
    }

    /// Saves a review session into `.tauqe/reviews/<id>.json` atomically.
    pub fn save_session(repo_root: &Path, session: &ReviewSession) -> Result<()> {
        let dir = Self::reviews_dir(repo_root);
        std::fs::create_dir_all(&dir).with_context(|| {
            format!("Failed to create reviews directory at {}", dir.display())
        })?;

        let safe_id = Self::sanitize_id(&session.id);
        let target_file = dir.join(format!("{safe_id}.json"));
        let temp_file = dir.join(format!("{safe_id}.json.tmp"));

        let json = serde_json::to_string_pretty(session)
            .context("Failed to serialize review session to JSON")?;
        std::fs::write(&temp_file, json).with_context(|| {
            format!("Failed to write temporary review file at {}", temp_file.display())
        })?;

        std::fs::rename(&temp_file, &target_file).with_context(|| {
            format!("Failed to atomically rename review file to {}", target_file.display())
        })?;

        // If no active review is set, make this session active
        if let Ok(None) = Self::load_active_id(repo_root) {
            let _ = Self::save_active_id(repo_root, Some(&session.id));
        }

        Ok(())
    }

    /// Deletes a review session from storage. Returns `true` if a file was deleted.
    pub fn delete_review(repo_root: &Path, id: &str) -> Result<bool> {
        let dir = Self::reviews_dir(repo_root);
        let safe_id = Self::sanitize_id(id);
        let target_file = dir.join(format!("{safe_id}.json"));

        if target_file.is_file() {
            std::fs::remove_file(&target_file)
                .with_context(|| format!("Failed to remove review file at {}", target_file.display()))?;

            if let Ok(Some(active)) = Self::load_active_id(repo_root) {
                if active == id || active == safe_id {
                    let _ = Self::save_active_id(repo_root, None);
                }
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Loads the active review ID from `.tauqe/reviews/active`.
    pub fn load_active_id(repo_root: &Path) -> Result<Option<String>> {
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
        mut updater: F,
    ) -> Result<Option<ReviewItem>>
    where
        F: FnMut(&mut ReviewItem),
    {
        let mut session = match review_id {
            Some(id) => match Self::load_review(repo_root, id)? {
                Some(s) => s,
                None => return Ok(None),
            },
            None => match Self::load_latest(repo_root)? {
                Some(s) => s,
                None => return Ok(None),
            },
        };

        let mut updated_item = None;
        for item in &mut session.items {
            if item.id == item_id {
                updater(item);
                updated_item = Some(item.clone());
                break;
            }
        }

        if updated_item.is_some() {
            Self::save_session(repo_root, &session)?;
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
        F: FnMut(&mut ReviewItem),
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
            model: "anthropic/claude-3.7-sonnet".to_string(),
            description: Some("Initial security scan".to_string()),
            user_prompt: Some("focus on security".to_string()),
            target_files: vec!["crates/core/src/lib.rs".to_string()],
            items: vec![ReviewItem {
                id: 1,
                title: "Security finding".to_string(),
                severity: ReviewSeverity::Critical,
                status: ReviewItemStatus::Discussion,
                file_path: Some("crates/core/src/lib.rs".to_string()),
                line_range: Some((10, 20)),
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
        let legacy_session = ReviewSession {
            id: "legacy-session-9".to_string(),
            title: "Legacy Audit".to_string(),
            created_at: 12345,
            model: "test-model".to_string(),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![],
            raw_markdown: "".to_string(),
        };
        std::fs::write(&legacy_file, serde_json::to_string(&legacy_session).unwrap()).unwrap();

        // Migration should run transparently on load_latest or load_all
        let loaded = ReviewStorage::load_latest(root).unwrap().unwrap();
        assert_eq!(loaded.id, "legacy-session-9");
        assert!(!legacy_file.is_file());
        assert!(reviews_dir.join("legacy-session-9.json").is_file());
    }
}
