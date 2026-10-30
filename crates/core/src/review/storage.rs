mod groups;
mod items;
mod post_analysis;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::atomic_write::write_atomic;
use std::sync::{Mutex, MutexGuard};
use tauqe_protocol::{ReviewSession, ReviewSummary};

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
mod tests;
