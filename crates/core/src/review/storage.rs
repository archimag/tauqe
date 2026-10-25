use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::atomic_write::write_atomic;
use std::sync::{Mutex, MutexGuard};
use tauqe_protocol::{
    ArchitectTier, IssueStatus, Plan, PlanItem, ReviewIssue, ReviewItem, ReviewSession,
    ReviewSummary,
};

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
    ///
    /// Every public entry point acquires this lock inside
    /// [`crate::blocking::run_blocking`], so neither the wait for the lock nor the
    /// file I/O that follows it ever occupies a Tokio worker thread.
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

    /// Lists summaries of all review sessions, ordered chronologically.
    pub fn list_summaries(repo_root: &Path) -> Result<Vec<ReviewSummary>> {
        let reviews = Self::load_all(repo_root)?;
        Ok(reviews.into_iter().map(|r| r.summary()).collect())
    }

    /// Lists all review sessions from `.tauqe/reviews`, ordered by `created_at` descending.
    pub fn list_reviews(repo_root: &Path) -> Result<Vec<ReviewSession>> {
        Self::load_all(repo_root)
    }

    /// Migrates a legacy single-file review dump into per-session files.
    ///
    /// Earlier Tauqe versions kept the latest review session in a single
    /// `.tauqe/reviews/latest.json` file. This runs exactly once at daemon
    /// startup: when the legacy file is present it is read, written back as
    /// `<session-id>.json`, and removed. Keeping the migration on the startup
    /// path (and never on reads) keeps listing and loading free of write side
    /// effects and preserves deterministic read behavior.
    pub fn migrate_legacy_if_needed(repo_root: &Path) {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::migrate_legacy_if_needed_unlocked(repo_root);
        });
    }

    /// Lock-free core of [`Self::migrate_legacy_if_needed`]; the caller must hold
    /// the storage lock.
    fn migrate_legacy_if_needed_unlocked(repo_root: &Path) {
        let legacy_file = Self::reviews_dir(repo_root).join("latest.json");
        if !legacy_file.is_file() {
            return;
        }

        let content = match std::fs::read_to_string(&legacy_file) {
            Ok(content) => content,
            Err(err) => {
                tracing::warn!(
                    path = %legacy_file.display(),
                    error = %err,
                    "Failed to read legacy review file"
                );
                return;
            }
        };

        let session = match serde_json::from_str::<ReviewSession>(&content) {
            Ok(session) => session,
            Err(err) => {
                tracing::warn!(
                    path = %legacy_file.display(),
                    error = %err,
                    "Failed to parse legacy review file"
                );
                return;
            }
        };

        // Remove the legacy container before writing the migrated session. The
        // legacy file stores the very session being migrated, so the storage
        // resolver would otherwise select `latest.json` as this session's
        // current file and the save would land back in the file we are about to
        // delete. The raw legacy content is kept in memory and restored if the
        // write fails, so a failed migration never loses the session.
        if let Err(err) = std::fs::remove_file(&legacy_file) {
            tracing::warn!(
                path = %legacy_file.display(),
                error = %err,
                "Failed to remove legacy review file before migration"
            );
            return;
        }

        if let Err(err) = Self::save_session_unlocked(repo_root, &session) {
            tracing::warn!(
                path = %legacy_file.display(),
                error = %err,
                "Failed to migrate legacy review file"
            );
            // Restore the original legacy file so the session is never lost.
            let _ = std::fs::write(&legacy_file, &content);
        }
    }

    /// Loads all review sessions from disk.
    ///
    /// Acquires the process-global review storage lock, so a read never races a
    /// concurrent atomic rename in a write path.
    pub fn load_all(repo_root: &Path) -> Result<Vec<ReviewSession>> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::load_all_unlocked(repo_root)
        })
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

        // Order chronologically: the persisted `created_at` is the authoritative
        // instant, and the UUIDv7 embedded timestamp breaks same-second ties so a
        // directory listing always follows creation order. The raw id is the final
        // stable tiebreaker, so the order never depends on `read_dir` iteration.
        reviews.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| session_uuid_timestamp(&b.id).cmp(&session_uuid_timestamp(&a.id)))
                .then_with(|| b.id.cmp(&a.id))
        });
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

        let default_matches = std::fs::read_to_string(&default_target)
            .ok()
            .and_then(|content| serde_json::from_str::<ReviewSession>(&content).ok())
            .is_some_and(|session| session.id.eq_ignore_ascii_case(id));
        if default_target.is_file() && default_matches {
            return Some(default_target);
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
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::load_review_unlocked(repo_root, id)
        })
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
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::load_latest_unlocked(repo_root)
        })
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
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::save_session_unlocked(repo_root, session)
        })
    }

    /// Lock-free core of [`Self::save_session`]; caller must hold storage lock.
    fn save_session_unlocked(repo_root: &Path, session: &ReviewSession) -> Result<()> {
        let dir = Self::reviews_dir(repo_root);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create reviews directory at {}", dir.display()))?;

        let safe_id = Self::sanitize_id(&session.id);
        let default_target = dir.join(format!("{safe_id}.json"));

        // Reuse the file that already stores this session even when its name differs
        // from the sanitized id, so saving never spawns a duplicate session file.
        let target_file =
            Self::find_session_file_unlocked(repo_root, &session.id).unwrap_or(default_target);

        // Refuse to overwrite a file that already stores a different review session
        // whose sanitized storage name collides with this one (for example
        // "team/auth" and "team-auth" both sanitize to "team-auth.json"), so a save
        // never silently replaces an unrelated session.
        if target_file.is_file() {
            let content = std::fs::read_to_string(&target_file).with_context(|| {
                format!(
                    "Failed to read existing review file at {}",
                    target_file.display()
                )
            })?;
            let existing: ReviewSession = serde_json::from_str(&content).with_context(|| {
                format!(
                    "Failed to parse existing review JSON at {}",
                    target_file.display()
                )
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
        write_atomic(&target_file, json.as_bytes())?;

        // If no active review is set, make this session active
        if let Ok(None) = Self::load_active_id_unlocked(repo_root) {
            let _ = Self::save_active_id_unlocked(repo_root, Some(&session.id));
        }

        Ok(())
    }

    /// Deletes a review session from storage. Returns `true` if a file was deleted.
    pub fn delete_review(repo_root: &Path, id: &str) -> Result<bool> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::delete_review_unlocked(repo_root, id)
        })
    }

    /// Lock-free core of [`Self::delete_review`]; caller must hold storage lock.
    fn delete_review_unlocked(repo_root: &Path, id: &str) -> Result<bool> {
        let dir = Self::reviews_dir(repo_root);
        let safe_id = Self::sanitize_id(id);

        // Delete the file that actually stores this session. When the session lives
        // under a differently sanitized name, removing only the default target would
        // leave the real artifact behind. If the default target exists but stores a
        // different session (collision), it must never be removed.
        let target_file = match Self::find_session_file_unlocked(repo_root, id) {
            Some(existing) => existing,
            None => {
                let default_target = dir.join(format!("{safe_id}.json"));
                if !default_target.is_file() {
                    return Ok(false);
                }
                let stores_other = std::fs::read_to_string(&default_target)
                    .ok()
                    .and_then(|content| serde_json::from_str::<ReviewSession>(&content).ok())
                    .is_some_and(|session| !session.id.eq_ignore_ascii_case(id));
                if stores_other {
                    return Ok(false);
                }
                default_target
            }
        };

        std::fs::remove_file(&target_file).with_context(|| {
            format!("Failed to remove review file at {}", target_file.display())
        })?;

        if let Ok(Some(active)) = Self::load_active_id_unlocked(repo_root) {
            if active.eq_ignore_ascii_case(id) || active.eq_ignore_ascii_case(&safe_id) {
                let _ = Self::save_active_id_unlocked(repo_root, None);
            }
        }
        Ok(true)
    }

    /// Loads the active review ID from `.tauqe/reviews/active`.
    pub fn load_active_id(repo_root: &Path) -> Result<Option<String>> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::load_active_id_unlocked(repo_root)
        })
    }

    /// Lock-free core of [`Self::load_active_id`]; the caller must hold the storage lock.
    fn load_active_id_unlocked(repo_root: &Path) -> Result<Option<String>> {
        let active_file = Self::reviews_dir(repo_root).join("active");
        if !active_file.is_file() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&active_file).with_context(|| {
            format!(
                "Failed to read active review marker at {}",
                active_file.display()
            )
        })?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    /// Saves or clears the active review ID in `.tauqe/reviews/active`.
    pub fn save_active_id(repo_root: &Path, id: Option<&str>) -> Result<()> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::save_active_id_unlocked(repo_root, id)
        })
    }

    /// Lock-free core of [`Self::save_active_id`]; caller must hold storage lock.
    fn save_active_id_unlocked(repo_root: &Path, id: Option<&str>) -> Result<()> {
        let dir = Self::reviews_dir(repo_root);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create reviews directory at {}", dir.display()))?;

        let active_file = dir.join("active");
        match id {
            Some(val) if !val.trim().is_empty() => {
                write_atomic(&active_file, val.trim().as_bytes())?;
            }
            _ => {
                if active_file.is_file() {
                    let _ = std::fs::remove_file(&active_file);
                }
            }
        }
        Ok(())
    }

    /// Updates a review item by id within the explicitly named session.
    ///
    /// The session id is mandatory: review mutations are strictly scoped to the
    /// session the caller names, so the process-global active marker is never
    /// consulted as an implicit fallback.
    pub fn update_session_item<F>(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        updater: F,
    ) -> Result<Option<ReviewIssue>>
    where
        F: FnOnce(&mut ReviewIssue),
    {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::update_session_item_unlocked(repo_root, review_id, item_id, updater)
        })
    }

    /// Lock-free core of [`Self::update_session_item`]; caller must hold storage lock.
    fn update_session_item_unlocked<F>(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        updater: F,
    ) -> Result<Option<ReviewIssue>>
    where
        F: FnOnce(&mut ReviewIssue),
    {
        let mut session = match Self::load_review_unlocked(repo_root, review_id)? {
            Some(s) => s,
            None => return Ok(None),
        };

        let updated_issue = if let Some(item) = session.items.iter_mut().find(|it| it.id == item_id)
        {
            let mut issue = ReviewIssue::from(item.clone());
            updater(&mut issue);
            let mut updated_item = ReviewItem::from(issue.clone());
            // The issue view carries no planning provenance, so the Architect
            // configuration and the reprojected sub-step are carried over
            // explicitly instead of being dropped by the round-trip.
            updated_item.subplan_architect = item.subplan_architect.clone();
            updated_item.subplan_last_replanned_step = item.subplan_last_replanned_step.clone();
            *item = updated_item;
            Some(issue)
        } else {
            None
        };

        if updated_issue.is_some() {
            Self::save_session_unlocked(repo_root, &session)?;
        }

        Ok(updated_issue)
    }

    /// Updates status of a review item within the explicitly named review session.
    pub fn update_session_item_status(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        status: IssueStatus,
    ) -> Result<Option<ReviewIssue>> {
        Self::update_session_item(repo_root, review_id, item_id, |issue| {
            issue.status = status;
        })
    }

    /// Replaces the uncompleted horizon of a review item's subplan while preserving
    /// any already completed (`Done`) steps.
    ///
    /// `last_replanned_step` names the sub-step whose horizon this wave
    /// reprojects. It is recorded on the finding, so a repeated failure of the
    /// same sub-step is recognized from persisted state instead of in-memory
    /// supervisor state.
    pub fn replace_uncompleted_subplan(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        new_subplan: Vec<PlanItem>,
        architect: ArchitectTier,
        last_replanned_step: Option<String>,
    ) -> Result<Option<ReviewIssue>> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::replace_uncompleted_subplan_unlocked(
                repo_root,
                review_id,
                item_id,
                new_subplan,
                architect,
                last_replanned_step,
            )
        })
    }

    /// Lock-free core of [`Self::replace_uncompleted_subplan`]; caller must hold storage lock.
    fn replace_uncompleted_subplan_unlocked(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        new_subplan: Vec<PlanItem>,
        architect: ArchitectTier,
        last_replanned_step: Option<String>,
    ) -> Result<Option<ReviewIssue>> {
        let mut session = match Self::load_review_unlocked(repo_root, review_id)? {
            Some(s) => s,
            None => return Ok(None),
        };

        let updated_issue = if let Some(item) = session.items.iter_mut().find(|it| it.id == item_id)
        {
            // Work on the canonical `ReviewIssue` model so the plan and status
            // are read and mutated through their authoritative field names, then
            // persist only the fields this primitive owns.
            let mut issue = ReviewIssue::from(item.clone());
            let merged = crate::plan::storage::merge_subplan(&issue.plan, new_subplan)?;
            let mut plan_wrapper = Plan {
                items: merged,
                ..Default::default()
            };
            plan_wrapper.normalize_leaf_files();
            plan_wrapper.refresh_parent_statuses();
            issue.plan = plan_wrapper.items;

            // Planning settles the finding out of its planning status: `Refine`
            // requested the decomposition and `InProgress` was published for the
            // duration of the synthesis, so both resolve to `Todo` once the
            // horizon is persisted.
            if matches!(issue.status, IssueStatus::Refine | IssueStatus::InProgress)
                && !issue.plan.is_empty()
            {
                issue.status = IssueStatus::Todo;
            }

            item.status = issue.status;
            item.subplan = issue.plan.clone();
            // The finding records which Architect configuration produced the
            // horizon, so a later replanning wave can reproduce or escalate it.
            item.subplan_architect = Some(architect);
            // The reprojected sub-step is persisted with the subplan, so the
            // next wave reads from disk whether it already failed a round.
            item.subplan_last_replanned_step = last_replanned_step;
            Some(issue)
        } else {
            None
        };

        if updated_issue.is_some() {
            Self::save_session_unlocked(repo_root, &session)?;
        }

        Ok(updated_issue)
    }

    /// Atomically replaces the embedded subplan of a review item.
    ///
    /// The supplied subplan is the authoritative working horizon of the
    /// finding, so it is normalized before it is persisted. Container steps
    /// carry no authoritative status of their own: their status is a pure
    /// function of their leaves. [`Plan::refresh_parent_statuses`] recomputes
    /// that derived value, so a stored container can never disagree with the
    /// children it aggregates. The whole read-modify-write cycle runs inside a
    /// single acquisition of the process-global storage lock, so a concurrent
    /// update cannot interleave between the load and the save.
    pub fn update_subplan(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        subplan: Vec<PlanItem>,
    ) -> Result<Option<ReviewIssue>> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::update_subplan_unlocked(repo_root, review_id, item_id, subplan)
        })
    }

    /// Lock-free core of [`Self::update_subplan`]; caller must hold storage lock.
    fn update_subplan_unlocked(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        subplan: Vec<PlanItem>,
    ) -> Result<Option<ReviewIssue>> {
        let mut session = match Self::load_review_unlocked(repo_root, review_id)? {
            Some(s) => s,
            None => return Ok(None),
        };

        let updated_issue = if let Some(item) = session.items.iter_mut().find(|it| it.id == item_id)
        {
            // Normalize the derived container statuses and store the result
            // through the canonical `ReviewIssue::plan` field.
            let mut issue = ReviewIssue::from(item.clone());
            let mut plan = Plan {
                items: subplan,
                ..Default::default()
            };
            plan.refresh_parent_statuses();
            issue.plan = plan.items;
            item.subplan = issue.plan.clone();
            Some(issue)
        } else {
            None
        };

        if updated_issue.is_some() {
            Self::save_session_unlocked(repo_root, &session)?;
        }

        Ok(updated_issue)
    }
}

/// Extracts the UUIDv7 millisecond timestamp embedded in a session id.
///
/// Sessions are identified by UUIDv7, so the leading 48 bits encode the creation
/// instant and comparing them reproduces creation order. A non-UUIDv7 id (a
/// hand-written or imported name) has no natural order and falls back to `0`,
/// sorting before every UUIDv7 id.
fn session_uuid_timestamp(id: &str) -> u64 {
    super::uuid::parse_uuid_v7_timestamp(id).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{IssueType, ReviewSeverity};
    use tempfile::tempdir;

    /// Architect provenance recorded by the synthesis primitive under test.
    fn test_architect() -> ArchitectTier {
        ArchitectTier::new(tauqe_protocol::ModelRef::openrouter("test/architect"), None)
    }

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
                subplan_last_replanned_step: None,
                model: None,
                issue_type: IssueType::Security,
                severity: ReviewSeverity::Critical,
                status: IssueStatus::Discussion,
                recommendations: vec![],
                files: vec!["crates/core/src/lib.rs".to_string()],
                readonly_files: vec![],
                subplan: Vec::new(),
                subplan_architect: None,
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
        let loaded = ReviewStorage::load_review(root, "rev-sec-1")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.title, "Security Audit");
        // Persisting and reloading a finding keeps its exact category instead
        // of collapsing it into the coarse severity class.
        assert_eq!(loaded.items[0].issue_type, IssueType::Security);
        assert_eq!(loaded.issues()[0].issue_type, IssueType::Security);

        let latest = ReviewStorage::load_latest(root).unwrap().unwrap();
        assert_eq!(latest.id, "rev-sec-1");

        // 4. Update item status to Fixed
        let updated =
            ReviewStorage::update_session_item_status(root, "rev-sec-1", 1, IssueStatus::Fixed)
                .unwrap()
                .unwrap();
        assert_eq!(updated.status, IssueStatus::Fixed);

        // 5. Active marker
        assert_eq!(
            ReviewStorage::load_active_id(root).unwrap(),
            Some("rev-sec-1".to_string())
        );
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
                subplan_last_replanned_step: None,
                model: None,
                issue_type: tauqe_protocol::IssueType::Quality,
                severity: tauqe_protocol::ReviewSeverity::Suggestion,
                status: IssueStatus::Todo,
                recommendations: vec![],
                files: vec![],
                readonly_files: vec![],
                subplan: Vec::new(),
                subplan_architect: None,
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
                        "concurrent-review",
                        i,
                        IssueStatus::Fixed,
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
            .filter(|item| item.status == IssueStatus::Fixed)
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
        assert!(ReviewStorage::reviews_dir(root)
            .join("Review-One.json")
            .is_file());

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

        let loaded = ReviewStorage::load_review(root, "review one")
            .unwrap()
            .unwrap();
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
        let loaded = ReviewStorage::load_review(root, "team/auth")
            .unwrap()
            .unwrap();
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

    #[test]
    fn test_update_session_item_targets_only_the_named_session() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let make_session = |id: &str| ReviewSession {
            id: id.to_string(),
            title: id.to_string(),
            created_at: 1,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![ReviewItem {
                id: 1,
                title: "Finding".to_string(),
                subplan_last_replanned_step: None,
                model: None,
                issue_type: IssueType::Quality,
                severity: ReviewSeverity::Suggestion,
                status: IssueStatus::Todo,
                recommendations: vec![],
                files: vec![],
                readonly_files: vec![],
                subplan: Vec::new(),
                subplan_architect: None,
                resolution_notes: None,
                body: String::new(),
            }],
            raw_markdown: String::new(),
        };

        ReviewStorage::save_session(root, &make_session("rev-a")).unwrap();
        ReviewStorage::save_session(root, &make_session("rev-b")).unwrap();
        // The active marker points at `rev-a`, but it is a UI pointer only and
        // must never steer a mutation.
        ReviewStorage::save_active_id(root, Some("rev-a")).unwrap();

        let updated =
            ReviewStorage::update_session_item_status(root, "rev-b", 1, IssueStatus::Fixed)
                .unwrap()
                .unwrap();
        assert_eq!(updated.status, IssueStatus::Fixed);

        let a = ReviewStorage::load_review(root, "rev-a").unwrap().unwrap();
        assert_eq!(a.find_item(1).unwrap().status, IssueStatus::Todo);
        let b = ReviewStorage::load_review(root, "rev-b").unwrap().unwrap();
        assert_eq!(b.find_item(1).unwrap().status, IssueStatus::Fixed);

        // A non-existent id resolves to nothing: there is no implicit fallback
        // to the active or latest session.
        let missing =
            ReviewStorage::update_session_item_status(root, "rev-missing", 1, IssueStatus::Fixed)
                .unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_atomic_writes_leave_no_temp_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let session = ReviewSession {
            id: "atomic-review".to_string(),
            title: "Atomic".to_string(),
            created_at: 1,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(root, &session).unwrap();
        ReviewStorage::save_active_id(root, Some("atomic-review")).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(ReviewStorage::reviews_dir(root))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "unexpected temp files: {leftovers:?}");

        assert_eq!(
            ReviewStorage::load_active_id(root).unwrap(),
            Some("atomic-review".to_string())
        );
    }

    // Review storage performs blocking file I/O and waits on a synchronous mutex.
    // Driving the full lifecycle from an async task on a multi-threaded runtime
    // must neither panic nor stall the executor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_review_storage_lifecycle_on_multi_thread_runtime() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let session = ReviewSession {
            id: "runtime-review".to_string(),
            title: "Runtime Review".to_string(),
            created_at: 500,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![ReviewItem {
                id: 1,
                title: "Finding".to_string(),
                subplan_last_replanned_step: None,
                model: None,
                issue_type: IssueType::Architecture,
                severity: ReviewSeverity::Warning,
                status: IssueStatus::Discussion,
                recommendations: vec![],
                files: vec![],
                readonly_files: vec![],
                subplan: Vec::new(),
                subplan_architect: None,
                resolution_notes: None,
                body: "Body".to_string(),
            }],
            raw_markdown: String::new(),
        };

        ReviewStorage::save_session(&root, &session).unwrap();
        assert!(ReviewStorage::reviews_dir(&root)
            .join("runtime-review.json")
            .is_file());
        assert_eq!(
            ReviewStorage::load_active_id(&root).unwrap(),
            Some("runtime-review".to_string())
        );

        let loaded = ReviewStorage::load_review(&root, "runtime-review")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(loaded.items[0].status, IssueStatus::Discussion);

        let updated = ReviewStorage::update_session_item_status(
            &root,
            "runtime-review",
            1,
            IssueStatus::Todo,
        )
        .unwrap()
        .unwrap();
        assert_eq!(updated.status, IssueStatus::Todo);

        let replaced = ReviewStorage::replace_uncompleted_subplan(
            &root,
            "runtime-review",
            1,
            vec![PlanItem {
                id: "s-1".to_string(),
                title: "Step".to_string(),
                status: tauqe_protocol::PlanItemStatus::Todo,
                ..Default::default()
            }],
            test_architect(),
            Some("failed-step".to_string()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(replaced.plan.len(), 1);
        assert_eq!(replaced.plan[0].id, "s-1");

        // The issue view carries no Architect provenance, so the recorded
        // configuration is verified on the finding that was persisted.
        let stored = ReviewStorage::load_review(&root, "runtime-review")
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.items[0].subplan_architect,
            Some(test_architect()),
            "the Architect configuration that produced the subplan must be recorded"
        );
        assert_eq!(
            stored.items[0].subplan_last_replanned_step.as_deref(),
            Some("failed-step"),
            "the reprojected sub-step must be recorded with the subplan"
        );

        assert_eq!(ReviewStorage::load_all(&root).unwrap().len(), 1);
        assert_eq!(
            ReviewStorage::load_latest(&root).unwrap().unwrap().id,
            "runtime-review"
        );

        ReviewStorage::save_active_id(&root, None).unwrap();
        assert_eq!(ReviewStorage::load_active_id(&root).unwrap(), None);

        assert!(ReviewStorage::delete_review(&root, "runtime-review").unwrap());
        assert!(ReviewStorage::load_all(&root).unwrap().is_empty());
    }

    #[test]
    fn test_review_storage_replace_uncompleted_subplan() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let session = ReviewSession {
            id: "rev-subplan-1".to_string(),
            title: "Subplan Audit".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![ReviewItem {
                id: 1,
                title: "Finding with subplan".to_string(),
                subplan_last_replanned_step: None,
                model: None,
                issue_type: IssueType::Architecture,
                severity: ReviewSeverity::Warning,
                status: IssueStatus::Discussion,
                recommendations: vec![],
                files: vec![],
                readonly_files: vec![],
                subplan: vec![
                    PlanItem {
                        id: "s-1".to_string(),
                        title: "Finished step".to_string(),
                        status: tauqe_protocol::PlanItemStatus::Done,
                        ..Default::default()
                    },
                    PlanItem {
                        id: "s-2".to_string(),
                        title: "Old step".to_string(),
                        status: tauqe_protocol::PlanItemStatus::Todo,
                        ..Default::default()
                    },
                ],
                subplan_architect: None,
                resolution_notes: None,
                body: "Body".to_string(),
            }],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(root, &session).unwrap();

        let new_subplan = vec![PlanItem {
            id: "s-3".to_string(),
            title: "New step".to_string(),
            status: tauqe_protocol::PlanItemStatus::Todo,
            ..Default::default()
        }];

        let updated = ReviewStorage::replace_uncompleted_subplan(
            root,
            "rev-subplan-1",
            1,
            new_subplan,
            test_architect(),
            Some("s-2".to_string()),
        )
        .unwrap()
        .unwrap();

        assert_eq!(updated.plan.len(), 2);
        assert_eq!(updated.plan[0].id, "s-1");
        assert_eq!(updated.plan[0].status, tauqe_protocol::PlanItemStatus::Done);
        assert_eq!(updated.plan[1].id, "s-3");

        let loaded = ReviewStorage::load_review(root, "rev-subplan-1")
            .unwrap()
            .unwrap();
        let issues = loaded.issues();
        let issue = &issues[0];
        assert_eq!(issue.plan.len(), 2);
        assert_eq!(issue.plan[0].id, "s-1");
        assert_eq!(issue.plan[1].id, "s-3");
        // The reprojected sub-step is persisted with the finding and survives a
        // reload, so escalation is derived from stored state.
        assert_eq!(
            loaded.items[0].subplan_last_replanned_step.as_deref(),
            Some("s-2")
        );
    }

    #[test]
    fn test_update_subplan_normalizes_derived_container_statuses() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let session = ReviewSession {
            id: "rev-update-subplan".to_string(),
            title: "Subplan Update Audit".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![ReviewItem {
                id: 1,
                title: "Finding awaiting a subplan".to_string(),
                subplan_last_replanned_step: None,
                model: None,
                issue_type: IssueType::Architecture,
                severity: ReviewSeverity::Warning,
                status: IssueStatus::Todo,
                recommendations: vec![],
                files: vec![],
                readonly_files: vec![],
                subplan: Vec::new(),
                subplan_architect: None,
                resolution_notes: None,
                body: "Body".to_string(),
            }],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(root, &session).unwrap();

        // The container arrives with a stale `Done` status while one of its
        // leaves is still `Todo`; the persisted status must be recomputed.
        let subplan = vec![PlanItem {
            id: "phase-1".to_string(),
            title: "Phase".to_string(),
            status: tauqe_protocol::PlanItemStatus::Done,
            children: vec![
                PlanItem {
                    id: "s-1".to_string(),
                    title: "Finished step".to_string(),
                    status: tauqe_protocol::PlanItemStatus::Done,
                    ..Default::default()
                },
                PlanItem {
                    id: "s-2".to_string(),
                    title: "Pending step".to_string(),
                    status: tauqe_protocol::PlanItemStatus::Todo,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }];

        let updated = ReviewStorage::update_subplan(root, "rev-update-subplan", 1, subplan)
            .unwrap()
            .unwrap();
        assert_eq!(updated.plan.len(), 1);
        assert_eq!(
            updated.plan[0].status,
            tauqe_protocol::PlanItemStatus::InProgress
        );

        let loaded = ReviewStorage::load_review(root, "rev-update-subplan")
            .unwrap()
            .unwrap();
        let issues = loaded.issues();
        let issue = &issues[0];
        assert_eq!(issue.plan.len(), 1);
        assert_eq!(
            issue.plan[0].status,
            tauqe_protocol::PlanItemStatus::InProgress
        );
        assert_eq!(issue.plan[0].children.len(), 2);

        // An unknown finding id resolves to nothing and never rewrites state.
        let missing =
            ReviewStorage::update_subplan(root, "rev-update-subplan", 99, Vec::new()).unwrap();
        assert!(missing.is_none());
        let unchanged = ReviewStorage::load_review(root, "rev-update-subplan")
            .unwrap()
            .unwrap();
        assert_eq!(unchanged.issues()[0].plan.len(), 1);
    }

    #[test]
    fn test_replace_uncompleted_subplan_transitions_refine_to_todo() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let session = ReviewSession {
            id: "rev-refine-to-todo".to_string(),
            title: "Refine Transition Audit".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![ReviewItem {
                id: 1,
                title: "Architectural finding".to_string(),
                subplan_last_replanned_step: None,
                model: None,
                issue_type: IssueType::Architecture,
                severity: ReviewSeverity::Warning,
                status: IssueStatus::Refine,
                recommendations: vec![],
                files: vec![],
                readonly_files: vec![],
                subplan: Vec::new(),
                subplan_architect: None,
                resolution_notes: None,
                body: "Body".to_string(),
            }],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(root, &session).unwrap();

        let new_subplan = vec![PlanItem {
            id: "step-1".to_string(),
            title: "Implementation step".to_string(),
            status: tauqe_protocol::PlanItemStatus::Todo,
            ..Default::default()
        }];

        let updated = ReviewStorage::replace_uncompleted_subplan(
            root,
            "rev-refine-to-todo",
            1,
            new_subplan,
            test_architect(),
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(updated.status, IssueStatus::Todo);

        let loaded = ReviewStorage::load_review(root, "rev-refine-to-todo")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.items[0].status, IssueStatus::Todo);
    }

    /// A finding is published as `InProgress` while the Architect synthesizes its
    /// horizon. Persisting that horizon must settle the finding on `Todo`, so a
    /// successfully planned finding is executable instead of remaining stuck in
    /// its planning status.
    #[test]
    fn test_replace_uncompleted_subplan_transitions_inprogress_to_todo() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let session = ReviewSession {
            id: "rev-inprogress-to-todo".to_string(),
            title: "Planning Status Audit".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![ReviewItem {
                id: 1,
                title: "Finding under planning".to_string(),
                subplan_last_replanned_step: None,
                model: None,
                issue_type: IssueType::Quality,
                severity: ReviewSeverity::Suggestion,
                status: IssueStatus::InProgress,
                recommendations: vec![],
                files: vec![],
                readonly_files: vec![],
                subplan: Vec::new(),
                subplan_architect: None,
                resolution_notes: None,
                body: "Body".to_string(),
            }],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(root, &session).unwrap();

        let new_subplan = vec![PlanItem {
            id: "step-1".to_string(),
            title: "Implementation step".to_string(),
            status: tauqe_protocol::PlanItemStatus::Todo,
            ..Default::default()
        }];

        let updated = ReviewStorage::replace_uncompleted_subplan(
            root,
            "rev-inprogress-to-todo",
            1,
            new_subplan,
            test_architect(),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(updated.status, IssueStatus::Todo);
        assert_eq!(updated.plan.len(), 1);

        let loaded = ReviewStorage::load_review(root, "rev-inprogress-to-todo")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.items[0].status, IssueStatus::Todo);
        assert_eq!(loaded.issues()[0].plan.len(), 1);
    }

    #[test]
    fn test_review_storage_extended_fields_persistence() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let session = ReviewSession {
            id: "rev-ext-1".to_string(),
            title: "Extended Fields Audit".to_string(),
            created_at: 2000,
            model: tauqe_protocol::ModelRef::openrouter("anthropic/claude-3.7-sonnet"),
            description: Some("Persistence check".to_string()),
            user_prompt: None,
            target_files: vec!["crates/core/src/lib.rs".to_string()],
            items: vec![ReviewItem {
                id: 1,
                title: "Finding with extended fields".to_string(),
                subplan_last_replanned_step: Some("s-9".to_string()),
                model: None,
                issue_type: IssueType::Architecture,
                severity: ReviewSeverity::Warning,
                status: IssueStatus::Discussion,
                recommendations: vec!["Step 1: refactor lock".to_string()],
                files: vec!["crates/core/src/state.rs".to_string()],
                readonly_files: vec!["crates/protocol/src/lib.rs".to_string()],
                subplan: Vec::new(),
                subplan_architect: None,
                resolution_notes: None,
                body: "Root cause analysis".to_string(),
            }],
            raw_markdown: String::new(),
        };

        ReviewStorage::save_session(root, &session).unwrap();

        let updated = ReviewStorage::update_session_item(root, "rev-ext-1", 1, |issue| {
            issue.status = IssueStatus::Fixed;
            issue
                .recommendations
                .push("Step 2: add regression test".to_string());
            issue
                .readonly_files
                .push("crates/core/src/config.rs".to_string());
            issue.note = Some("Resolved in PR #42".to_string());
        })
        .unwrap()
        .unwrap();

        assert_eq!(updated.status, IssueStatus::Fixed);
        assert_eq!(
            updated.recommendations,
            vec!["Step 1: refactor lock", "Step 2: add regression test"]
        );
        assert_eq!(
            updated.readonly_files,
            vec!["crates/protocol/src/lib.rs", "crates/core/src/config.rs"]
        );
        assert_eq!(updated.note.as_deref(), Some("Resolved in PR #42"));

        let loaded = ReviewStorage::load_review(root, "rev-ext-1")
            .unwrap()
            .unwrap();
        let issues = loaded.issues();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].status, IssueStatus::Fixed);
        assert_eq!(
            issues[0].recommendations,
            vec!["Step 1: refactor lock", "Step 2: add regression test"]
        );
        assert_eq!(
            issues[0].readonly_files,
            vec!["crates/protocol/src/lib.rs", "crates/core/src/config.rs"]
        );
        assert_eq!(issues[0].note.as_deref(), Some("Resolved in PR #42"));
        // The replan provenance is not part of the issue view, so an item
        // update must carry it over instead of dropping it.
        assert_eq!(
            loaded.items[0].subplan_last_replanned_step.as_deref(),
            Some("s-9")
        );
    }

    /// A finding update round-trips through `ReviewIssue` and back into
    /// `ReviewItem`. The semantic category is the single source of truth and
    /// must survive that trip verbatim: the pairs the coarse severity
    /// projection merges (`Bug`/`Security`, `Architecture`/`Performance`) must
    /// never collapse into one another, and the derived severity must be
    /// recomputed from the stored category on the next read.
    #[test]
    fn test_update_session_item_preserves_the_exact_issue_category() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let make_item = |id: u32, issue_type: IssueType, severity: ReviewSeverity| ReviewItem {
            id,
            title: format!("Finding {id}"),
            subplan_last_replanned_step: None,
            model: None,
            issue_type,
            severity,
            status: IssueStatus::Todo,
            recommendations: vec![],
            files: vec![],
            readonly_files: vec![],
            subplan: Vec::new(),
            subplan_architect: None,
            resolution_notes: None,
            body: String::new(),
        };

        let session = ReviewSession {
            id: "rev-category-fidelity".to_string(),
            title: "Category Fidelity".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![
                make_item(1, IssueType::Security, ReviewSeverity::Critical),
                make_item(2, IssueType::Performance, ReviewSeverity::Warning),
            ],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(root, &session).unwrap();

        let updated =
            ReviewStorage::update_session_item(root, "rev-category-fidelity", 1, |issue| {
                issue.status = IssueStatus::Fixed;
            })
            .unwrap()
            .unwrap();
        assert_eq!(updated.issue_type, IssueType::Security);

        let loaded = ReviewStorage::load_review(root, "rev-category-fidelity")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.find_item(1).unwrap().issue_type, IssueType::Security);
        assert_eq!(
            loaded.find_item(2).unwrap().issue_type,
            IssueType::Performance
        );
        // The presentation severity is derived from the stored category.
        assert_eq!(
            loaded.find_item(1).unwrap().severity,
            ReviewSeverity::Critical
        );
        assert_eq!(
            loaded.find_item(2).unwrap().severity,
            ReviewSeverity::Warning
        );

        let issues = loaded.issues();
        assert_eq!(issues[0].issue_type, IssueType::Security);
        assert_eq!(issues[1].issue_type, IssueType::Performance);
    }
}
