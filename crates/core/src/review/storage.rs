use anyhow::{Context, Result};
use std::path::Path;
use tauqe_protocol::{ReviewItem, ReviewSession, ReviewStatus};

pub struct ReviewStorage;

impl ReviewStorage {
    /// Loads the latest review session from `.tauqe/reviews/latest.json`.
    pub fn load_latest(repo_root: &Path) -> Result<Option<ReviewSession>> {
        let path = repo_root.join(".tauqe").join("reviews").join("latest.json");
        if !path.is_file() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read review session file at {}", path.display()))?;
        let session: ReviewSession = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse review session JSON at {}", path.display()))?;
        Ok(Some(session))
    }

    /// Saves the review session into `.tauqe/reviews/latest.json` atomically.
    pub fn save_session(repo_root: &Path, session: &ReviewSession) -> Result<()> {
        let reviews_dir = repo_root.join(".tauqe").join("reviews");
        std::fs::create_dir_all(&reviews_dir).with_context(|| {
            format!(
                "Failed to create review storage directory at {}",
                reviews_dir.display()
            )
        })?;

        let target_file = reviews_dir.join("latest.json");
        let temp_file = reviews_dir.join("latest.json.tmp");

        let json = serde_json::to_string_pretty(session)
            .context("Failed to serialize review session to JSON")?;
        std::fs::write(&temp_file, json).with_context(|| {
            format!(
                "Failed to write temporary review session at {}",
                temp_file.display()
            )
        })?;

        std::fs::rename(&temp_file, &target_file).with_context(|| {
            format!(
                "Failed to atomically persist review session to {}",
                target_file.display()
            )
        })?;

        Ok(())
    }

    /// Updates the status or checked state of a specific review item by id.
    pub fn update_item<F>(repo_root: &Path, item_id: u32, mut updater: F) -> Result<Option<ReviewItem>>
    where
        F: FnMut(&mut ReviewItem),
    {
        let mut session = match Self::load_latest(repo_root)? {
            Some(s) => s,
            None => return Ok(None),
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

    /// Convenience helper to update item status.
    pub fn update_item_status(
        repo_root: &Path,
        item_id: u32,
        status: ReviewStatus,
    ) -> Result<Option<ReviewItem>> {
        Self::update_item(repo_root, item_id, |item| {
            item.status = status;
        })
    }

    /// Convenience helper to update item checked flag.
    pub fn update_item_checked(
        repo_root: &Path,
        item_id: u32,
        is_checked: bool,
    ) -> Result<Option<ReviewItem>> {
        Self::update_item(repo_root, item_id, |item| {
            item.is_checked = is_checked;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::ReviewSeverity;
    use tempfile::tempdir;

    #[test]
    fn test_review_storage_save_load_update() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let session = ReviewSession {
            id: "rev-1".to_string(),
            created_at: 1000,
            model: "anthropic/claude-3.7-sonnet".to_string(),
            user_prompt: Some("focus on security".to_string()),
            target_files: vec!["crates/core/src/lib.rs".to_string()],
            items: vec![ReviewItem {
                id: 1,
                title: "Security finding".to_string(),
                severity: ReviewSeverity::Critical,
                status: ReviewStatus::Todo,
                is_checked: false,
                file_path: Some("crates/core/src/lib.rs".to_string()),
                line_range: Some((10, 20)),
                body: "Check input validation".to_string(),
            }],
            raw_markdown: "## [CRITICAL] Security finding\n...".to_string(),
        };

        ReviewStorage::save_session(root, &session).unwrap();

        let loaded = ReviewStorage::load_latest(root).unwrap().unwrap();
        assert_eq!(loaded.id, "rev-1");
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(loaded.items[0].status, ReviewStatus::Todo);
        assert!(!loaded.items[0].is_checked);

        // Update item checked
        let updated = ReviewStorage::update_item_checked(root, 1, true).unwrap().unwrap();
        assert!(updated.is_checked);

        // Update item status
        let updated = ReviewStorage::update_item_status(root, 1, ReviewStatus::Done).unwrap().unwrap();
        assert_eq!(updated.status, ReviewStatus::Done);

        let reloaded = ReviewStorage::load_latest(root).unwrap().unwrap();
        assert!(reloaded.items[0].is_checked);
        assert_eq!(reloaded.items[0].status, ReviewStatus::Done);
    }
}
