use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;
use tauqe_protocol::{Plan, PlanItem, PlanItemStatus, PlanRejection, PlanSummary};
use tauqe_tools::atomic_write::write_atomic;

mod apply;
mod horizon;
mod immutability;

pub use horizon::merge_subplan;

/// Execution mode governing plan mutation permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanMutationMode {
    /// Isolated step execution: any plan creation, modification, or deletion is forbidden.
    IsolatedStep,
    /// REPL mode: only proposing new plans (`save`) is permitted. Modification (`update`) and deletion (`delete`) are forbidden.
    Repl,
    /// Discussion mode: full plan creation, modification, and deletion are permitted.
    Discussion,
}

pub struct PlanStorage;

/// Process-global lock serializing plan storage mutations.
///
/// Plan storage performs non-atomic read-modify-write cycles (load, mutate, save).
/// Without serialization, concurrent tasks (UI RPC handlers and batch execution)
/// race on the same plan file and silently overwrite each other's changes. Because
/// the server is a single process, a process-global mutex makes each cycle atomic
/// with respect to all other in-process work.
static PLAN_STORAGE_LOCK: Mutex<()> = Mutex::new(());
/// Plan the write barrier normalized and validated, ready to be stored.
///
/// The barrier lives in one function so the write path and the non-persisting
/// probe run the very same preparation: the probe discards the document instead
/// of writing it, which is what makes its verdict the verdict of the application.
struct PreparedPlan {
    /// Normalized copy of the plan, exactly as it would be written.
    plan: Plan,
    /// File the plan belongs to, reused when the plan already lives under a
    /// differently sanitized name.
    target_file: PathBuf,
}

/// Repository snapshot one plan batch resolves its mutations against.
///
/// The snapshot is read from storage once, under the process-global lock, and
/// records the document of every stored plan together with the file that carries
/// it. A mutation resolves its identity, its target file, and the stored document
/// from the snapshot and advances it in memory, so a batch of `k` mutations over
/// `n` stored plans reads each stored document once instead of `O(k·n)` times.
#[derive(Debug, Default)]
struct PlanBatchSnapshot {
    /// Stored plans in listing order: creation time, newest first.
    plans: Vec<Plan>,
    /// Storage file of each plan, positionally parallel to `plans`.
    files: Vec<PathBuf>,
    /// Lowercased plan id to the position of the plan in `plans`.
    index: HashMap<String, usize>,
}

impl PlanBatchSnapshot {
    /// Loads every stored plan together with the file that carries it.
    ///
    /// A missing storage directory is an empty repository, while a directory that
    /// cannot be read is reported: the difference decides whether a batch creates
    /// the first plan or writes over a repository it never saw.
    fn load_unlocked(repo_root: &Path) -> Result<Self> {
        let dir = PlanStorage::plans_dir(repo_root);
        if !dir.is_dir() {
            return Ok(Self::default());
        }

        let entries = std::fs::read_dir(&dir)
            .with_context(|| format!("Failed to read plans directory at {}", dir.display()))?;
        let mut stored: Vec<(Plan, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<Plan>(&content) {
                    Ok(plan) => stored.push((plan, path)),
                    Err(err) => tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "Skipping corrupted plan file"
                    ),
                },
                Err(err) => tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "Failed to read plan file"
                ),
            }
        }

        // Creation time, not modification time, defines the tree order: a
        // long-lived plan that was edited a moment ago must not jump above a
        // newly created one, so the listing stays stable across edits.
        stored.sort_by_key(|(plan, _)| std::cmp::Reverse(plan.created_at));
        Ok(Self::from_stored(stored))
    }

    /// Builds the snapshot from stored documents paired with their files.
    fn from_stored(stored: Vec<(Plan, PathBuf)>) -> Self {
        let mut snapshot = Self::default();
        for (plan, file) in stored {
            snapshot.insert(plan, file);
        }
        snapshot
    }

    /// Consumes the snapshot and returns its plans in listing order.
    fn into_plans(self) -> Vec<Plan> {
        self.plans
    }

    /// Returns every stored plan in listing order.
    fn plans(&self) -> &[Plan] {
        &self.plans
    }

    /// Returns the stored document of `id` and the file that carries it.
    fn find(&self, id: &str) -> Option<(&Plan, &Path)> {
        let position = *self.index.get(&id.to_ascii_lowercase())?;
        Some((
            self.plans.get(position)?,
            self.files.get(position)?.as_path(),
        ))
    }

    /// Records the document a mutation produced, replacing the stored one.
    ///
    /// A plan the snapshot does not carry is appended, so a later mutation of the
    /// same batch resolves against the plan an earlier mutation created.
    fn remember(&mut self, plan: Plan, file: PathBuf) {
        match self.index.get(&plan.id.to_ascii_lowercase()).copied() {
            Some(position) => {
                if let Some(stored) = self.plans.get_mut(position) {
                    *stored = plan;
                }
                if let Some(stored) = self.files.get_mut(position) {
                    *stored = file;
                }
            }
            None => self.insert(plan, file),
        }
    }

    /// Appends one stored plan and its file to the snapshot.
    fn insert(&mut self, plan: Plan, file: PathBuf) {
        self.index
            .insert(plan.id.to_ascii_lowercase(), self.plans.len());
        self.plans.push(plan);
        self.files.push(file);
    }
}

impl PlanStorage {
    /// Acquires the process-global plan storage lock.
    ///
    /// Recovers the guard if the lock was poisoned by a previous panic, so a single
    /// failure never permanently wedges plan storage.
    ///
    /// Every public entry point acquires this lock inside
    /// [`crate::blocking::run_blocking`], so neither the wait for the lock nor the
    /// file I/O that follows it ever occupies a Tokio worker thread.
    fn lock() -> MutexGuard<'static, ()> {
        PLAN_STORAGE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Returns the directory path for local plan storage: `<repo_root>/.tauqe/plans`.
    pub fn plans_dir(repo_root: &Path) -> PathBuf {
        repo_root.join(".tauqe").join("plans")
    }

    /// Sanitizes an arbitrary ID or slug into a safe file name.
    fn sanitize_id(id: &str) -> String {
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

    /// Lists all stored plans from `.tauqe/plans`, ordered by `created_at` descending.
    ///
    /// Acquires the process-global storage lock so a listing never observes a plan
    /// file mid-rename while another task is persisting it.
    pub fn list_plans(repo_root: &Path) -> Result<Vec<PlanSummary>> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::list_plans_unlocked(repo_root)
        })
    }

    /// Lock-free core of [`Self::list_plans`]; the caller must hold the storage lock.
    fn list_plans_unlocked(repo_root: &Path) -> Result<Vec<PlanSummary>> {
        Ok(Self::load_all_unlocked(repo_root)?
            .into_iter()
            .map(|plan| plan.summary())
            .collect())
    }

    /// Loads a specific plan by its exact ID or file name.
    ///
    /// Acquires the process-global storage lock, so a read never races a concurrent
    /// atomic rename in a write path.
    pub fn load_plan(repo_root: &Path, id: &str) -> Result<Option<Plan>> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::load_plan_unlocked(repo_root, id)
        })
    }

    /// Lock-free core of [`Self::load_plan`]; the caller must hold the storage lock.
    fn load_plan_unlocked(repo_root: &Path, id: &str) -> Result<Option<Plan>> {
        let dir = Self::plans_dir(repo_root);
        let safe_id = Self::sanitize_id(id);
        let file_path = dir.join(format!("{safe_id}.json"));

        if file_path.is_file() {
            let content = std::fs::read_to_string(&file_path)
                .with_context(|| format!("Failed to read plan file at {}", file_path.display()))?;
            let plan: Plan = serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse plan JSON at {}", file_path.display()))?;

            // A sanitized-name collision (for example "team/auth" and "team-auth")
            // can surface a file that stores a different plan. Return it only when it
            // actually carries the requested id; otherwise fall through to exact
            // resolution so a lookup never leaks an unrelated plan.
            if plan.id.eq_ignore_ascii_case(id) {
                return Ok(Some(plan));
            }
        }

        // Resolve the id against every stored plan. This covers a plan stored under
        // an alternatively sanitized file name and exact/normalized id matching.
        if let Ok(plans) = Self::load_all_unlocked(repo_root) {
            if let Some(plan) = crate::plan::matcher::resolve_plan_id_exact(id, &plans) {
                return Ok(Some(plan.clone()));
            }
        }
        Ok(None)
    }

    /// Loads all full plans from disk.
    ///
    /// Acquires the process-global storage lock, so a read never races a concurrent
    /// atomic rename in a write path.
    pub fn load_all(repo_root: &Path) -> Result<Vec<Plan>> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::load_all_unlocked(repo_root)
        })
    }

    /// Lock-free core of [`Self::load_all`]; the caller must hold the storage lock.
    fn load_all_unlocked(repo_root: &Path) -> Result<Vec<Plan>> {
        Ok(PlanBatchSnapshot::load_unlocked(repo_root)?.into_plans())
    }

    /// Resolves the on-disk file that currently stores the plan identified by `id`,
    /// or `None` when no stored plan matches.
    ///
    /// The storage file name is normally `sanitize_id(id)`, but an id change (for
    /// example a casing change) leaves the plan under a differently sanitized name.
    /// Resolving the real file lets [`Self::save_plan_unlocked`] update and
    /// [`Self::delete_plan_unlocked`] remove the existing artifact instead of
    /// creating or leaving behind a duplicate plan file with the same id.
    ///
    /// Lock-free; the caller must hold the storage lock.
    fn find_plan_file_unlocked(repo_root: &Path, id: &str) -> Option<PathBuf> {
        let dir = Self::plans_dir(repo_root);
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(_) => continue,
            };
            let plan = match serde_json::from_str::<Plan>(&content) {
                Ok(plan) => plan,
                Err(_) => continue,
            };
            if plan.id.eq_ignore_ascii_case(id) {
                return Some(path);
            }
        }
        None
    }

    /// Saves a plan into `.tauqe/plans/<id>.json` atomically.
    ///
    /// Serializes against concurrent storage mutations through the process-global
    /// plan storage lock, so the read-validate-write cycle never interleaves.
    pub fn save_plan(repo_root: &Path, plan: &Plan) -> Result<()> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::save_plan_unlocked(repo_root, plan)
        })
    }

    /// Lock-free core of [`Self::save_plan`]; the caller must hold the storage lock.
    ///
    /// The barrier itself lives in [`Self::prepare_plan_unlocked`], which is also
    /// what the non-persisting probe runs, so this function only creates the
    /// storage directory, writes the prepared document, and records the active-plan
    /// marker.
    fn save_plan_unlocked(repo_root: &Path, plan: &Plan) -> Result<()> {
        // The directory is created before the barrier resolves the storage file,
        // because that resolution inspects the directory it is about to write into.
        Self::ensure_plans_dir(repo_root)?;
        let prepared = Self::prepare_plan_unlocked(repo_root, plan, None)?;
        Self::write_prepared_unlocked(repo_root, &prepared)
    }

    /// Saves one plan of a batch through the snapshot of that batch.
    ///
    /// The snapshot already resolved the stored document and the file that carries
    /// it, so a mutation writes that file directly instead of scanning the storage
    /// directory for the plan, and it advances the snapshot with the document that
    /// reached disk instead of reloading the repository afterwards. The returned
    /// document is exactly the one a later mutation of the batch resolves against.
    fn save_plan_in_snapshot_unlocked(
        repo_root: &Path,
        snapshot: &mut PlanBatchSnapshot,
        plan: &Plan,
    ) -> Result<Plan> {
        Self::ensure_plans_dir(repo_root)?;
        let prepared = Self::prepare_plan_unlocked(repo_root, plan, Some(snapshot))?;
        Self::write_prepared_unlocked(repo_root, &prepared)?;
        snapshot.remember(prepared.plan.clone(), prepared.target_file);
        Ok(prepared.plan)
    }
    /// Creates the plan storage directory.
    fn ensure_plans_dir(repo_root: &Path) -> Result<()> {
        let dir = Self::plans_dir(repo_root);
        std::fs::create_dir_all(&dir).with_context(|| {
            format!(
                "Failed to create plan storage directory at {}",
                dir.display()
            )
        })?;
        Ok(())
    }

    /// Writes one prepared plan and records the active-plan marker when the
    /// repository has none, so clients always have a marker to render.
    fn write_prepared_unlocked(repo_root: &Path, prepared: &PreparedPlan) -> Result<()> {
        let json = serde_json::to_string_pretty(&prepared.plan)
            .context("Failed to serialize plan to JSON")?;
        write_atomic(&prepared.target_file, json.as_bytes())?;

        if let Ok(None) = Self::load_active_id_unlocked(repo_root) {
            let _ = Self::save_active_id_unlocked(repo_root, Some(&prepared.plan.id));
        }
        Ok(())
    }

    /// Reads and decodes the plan stored at `path`.
    fn read_plan_file_unlocked(path: &Path) -> Result<Plan> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read existing plan file at {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse existing plan JSON at {}", path.display()))
    }

    /// Normalizes a plan and runs the single structural barrier of persistence
    /// without writing a byte.
    ///
    /// The barrier is shared by the write path and the non-persisting probe, which
    /// is what makes their verdicts identical: the leaf-only file invariant, the
    /// `Done is Immutable` check against the stored document, the refusal to
    /// overwrite a colliding storage name, and the structural validation of the
    /// whole plan all run here, in this order, for both contracts. A plan that
    /// passes therefore persists, and a plan that fails is refused identically for
    /// every mutation source.
    ///
    /// The stored document and the file that carries it arrive through `snapshot`
    /// when the caller resolved them under the storage lock already: a batch hands
    /// its snapshot over, so a mutation neither scans the storage directory nor
    /// re-reads a document the batch holds, while a single save passes `None` and
    /// resolves the one file it addresses from disk.
    fn prepare_plan_unlocked(
        repo_root: &Path,
        plan: &Plan,
        snapshot: Option<&PlanBatchSnapshot>,
    ) -> Result<PreparedPlan> {
        let dir = Self::plans_dir(repo_root);
        let safe_id = Self::sanitize_id(&plan.id);
        let default_target = dir.join(format!("{safe_id}.json"));

        let mut normalized = plan.clone();
        normalized.implementation.normalize_leaf_files();
        // Provenance is harness-authored: a commit belongs to a finished step and a
        // blocker report to a stalled one. Dropping an unearned claim here, where
        // every mutation source converges, is what keeps the documented invariant
        // true on disk, and it keeps the probe that runs this same preparation
        // predicting the application's verdict exactly.
        normalized.implementation.normalize_lifecycle_provenance();
        normalized.implementation.refresh_parent_statuses();

        let (target_file, existing) = if let Some(snapshot) = snapshot {
            match snapshot.find(&plan.id) {
                Some((stored, file)) => (file.to_path_buf(), Some(stored.clone())),
                // An id the snapshot does not carry owns no storage file yet. A
                // default target already occupied by another artifact is resolved
                // from disk, so a sanitized-name collision and a document that
                // cannot be decoded keep the diagnostics a single save reports.
                None if !default_target.is_file() => (default_target, None),
                None => {
                    let stored = Self::read_plan_file_unlocked(&default_target)?;
                    (default_target, Some(stored))
                }
            }
        } else {
            // Reuse the file that already stores this plan even when its name
            // differs from the sanitized id (for example after an id casing
            // change). Writing to the resolved file keeps a single plan per id
            // on disk instead of spawning a duplicate `{safe_id}.json` beside
            // the original.
            let target_file = if default_target.is_file() {
                default_target
            } else {
                Self::find_plan_file_unlocked(repo_root, &plan.id).unwrap_or(default_target)
            };
            let existing = if target_file.is_file() {
                Some(Self::read_plan_file_unlocked(&target_file)?)
            } else {
                None
            };
            (target_file, existing)
        };

        if let Some(existing_plan) = existing {
            if !existing_plan.id.eq_ignore_ascii_case(&plan.id) {
                anyhow::bail!(
                    "Cannot save plan '{}': storage name '{}.json' is already used by plan '{}'; choose a distinct plan id",
                    plan.id,
                    safe_id,
                    existing_plan.id
                );
            }
            Self::validate_done_immutability(&existing_plan, &normalized)?;
        }

        // The single structural barrier of plan persistence: every mutation
        // source (an explicit `save`, a delta `update`, a discussion payload,
        // and the programmatic horizon primitives) reaches disk through this
        // preparation, so a structurally invalid plan is rejected identically for
        // all of them. The normalized copy is validated because it is exactly
        // what would be written.
        if let Err(errors) = normalized.validate() {
            let reasons: Vec<String> = errors.iter().map(|error| error.to_string()).collect();
            anyhow::bail!(
                "Cannot save plan '{}': {}",
                normalized.id,
                reasons.join("; ")
            );
        }

        Ok(PreparedPlan {
            plan: normalized,
            target_file,
        })
    }

    /// Deletes a plan from storage. Returns `true` if a file was deleted.
    ///
    /// Serializes against concurrent storage mutations through the process-global lock.
    pub fn delete_plan(repo_root: &Path, id: &str) -> Result<bool> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::delete_plan_unlocked(repo_root, id)
        })
    }

    /// Lock-free core of [`Self::delete_plan`]; the caller must hold the storage lock.
    fn delete_plan_unlocked(repo_root: &Path, id: &str) -> Result<bool> {
        let safe_id = Self::sanitize_id(id);

        // Delete the file that actually stores this plan. Resolving by id (rather
        // than by the default sanitized name) finds a plan living under an
        // alternatively sanitized name and never removes a colliding file that
        // stores a different plan.
        let target_file = match Self::find_plan_file_unlocked(repo_root, id) {
            Some(existing) => existing,
            None => return Ok(false),
        };

        std::fs::remove_file(&target_file)
            .with_context(|| format!("Failed to remove plan file at {}", target_file.display()))?;

        // If the deleted plan was active, clear active marker
        if let Ok(Some(act)) = Self::load_active_id_unlocked(repo_root) {
            if act == id || act == safe_id {
                let _ = Self::save_active_id_unlocked(repo_root, None);
            }
        }
        Ok(true)
    }

    /// Loads the active plan ID from `.tauqe/plans/active`.
    pub fn load_active_id(repo_root: &Path) -> Result<Option<String>> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::load_active_id_unlocked(repo_root)
        })
    }

    /// Lock-free core of [`Self::load_active_id`]; the caller must hold the storage lock.
    fn load_active_id_unlocked(repo_root: &Path) -> Result<Option<String>> {
        let active_file = Self::plans_dir(repo_root).join("active");
        if !active_file.is_file() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&active_file).with_context(|| {
            format!(
                "Failed to read active plan marker at {}",
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

    /// Saves or clears the active plan ID in `.tauqe/plans/active`.
    ///
    /// Serializes against concurrent storage mutations through the process-global lock.
    pub fn save_active_id(repo_root: &Path, id: Option<&str>) -> Result<()> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::save_active_id_unlocked(repo_root, id)
        })
    }

    /// Lock-free core of [`Self::save_active_id`]; the caller must hold the storage lock.
    fn save_active_id_unlocked(repo_root: &Path, id: Option<&str>) -> Result<()> {
        let dir = Self::plans_dir(repo_root);
        std::fs::create_dir_all(&dir).with_context(|| {
            format!(
                "Failed to create plan storage directory at {}",
                dir.display()
            )
        })?;

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

    /// Updates an architectural concept item recursively.
    pub fn update_concept_item(
        repo_root: &Path,
        plan_id: &str,
        concept_id: &str,
        title: Option<&str>,
        details: Option<&str>,
    ) -> Result<Option<Plan>> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::update_concept_item_unlocked(repo_root, plan_id, concept_id, title, details)
        })
    }

    /// Lock-free core of [`Self::update_concept_item`]; the caller must hold the storage lock.
    fn update_concept_item_unlocked(
        repo_root: &Path,
        plan_id: &str,
        concept_id: &str,
        title: Option<&str>,
        details: Option<&str>,
    ) -> Result<Option<Plan>> {
        let mut plan = match Self::load_plan_unlocked(repo_root, plan_id)? {
            Some(p) => p,
            None => return Ok(None),
        };

        fn visit_concept_mut(
            items: &mut [tauqe_protocol::ConceptItem],
            target_id: &str,
            title: Option<&str>,
            details: Option<&str>,
        ) -> bool {
            for item in items {
                if item.id.eq_ignore_ascii_case(target_id) {
                    if let Some(t) = title {
                        item.title = t.to_string();
                    }
                    if let Some(d) = details {
                        item.details = Some(d.to_string());
                    }
                    return true;
                }
                if visit_concept_mut(&mut item.children, target_id, title, details) {
                    return true;
                }
            }
            false
        }

        let updated = visit_concept_mut(&mut plan.concept, concept_id, title, details);
        if updated {
            plan.updated_at = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            Self::save_plan_unlocked(repo_root, &plan)?;
            Ok(Some(plan))
        } else {
            Ok(None)
        }
    }

    /// Updates status of a plan item recursively.
    ///
    /// `blocked_reason` carries the diagnostic report of the semantic blocker that
    /// stopped the step. It is persisted only when the step enters
    /// [`PlanItemStatus::Blocked`]; every other transition drops the stale report,
    /// so a resumed step never keeps describing a blocker that no longer holds.
    ///
    /// `commit` is the identifier of the Git commit that completed the step. It is
    /// written by the very transition that records the status, so a `Done` step
    /// keeps a durable pointer to the change set it produced instead of a
    /// completion nobody can attribute. Every other transition passes `None`.
    pub fn update_item(
        repo_root: &Path,
        plan_id: &str,
        item_id: &str,
        status: Option<PlanItemStatus>,
        blocked_reason: Option<&str>,
        commit: Option<&str>,
    ) -> Result<Option<Plan>> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::update_item_unlocked(repo_root, plan_id, item_id, status, blocked_reason, commit)
        })
    }

    /// Lock-free core of [`Self::update_item`]; the caller must hold the storage lock.
    fn update_item_unlocked(
        repo_root: &Path,
        plan_id: &str,
        item_id: &str,
        status: Option<PlanItemStatus>,
        blocked_reason: Option<&str>,
        commit: Option<&str>,
    ) -> Result<Option<Plan>> {
        let mut plan = match Self::load_plan_unlocked(repo_root, plan_id)? {
            Some(p) => p,
            None => return Ok(None),
        };

        fn visit_mut(
            items: &mut [PlanItem],
            target_id: &str,
            status: Option<PlanItemStatus>,
            blocked_reason: Option<&str>,
            commit: Option<&str>,
        ) -> bool {
            for item in items {
                if item.id.eq_ignore_ascii_case(target_id) {
                    if let Some(s) = status {
                        item.status = s;
                        // The identifier of the change set that completed the
                        // step belongs to the completed record: it is stored
                        // once, together with the transition that finishes the
                        // step, and is never restated by a later one.
                        if let Some(commit) = commit.map(str::trim).filter(|hash| !hash.is_empty())
                        {
                            item.commit = Some(commit.to_string());
                        }
                        // The diagnostic report belongs to the Blocked state
                        // alone: entering it stores the supplied reason, leaving
                        // it drops the stale report so a stored plan never keeps
                        // describing a blocker that has been resolved already.
                        item.blocked_reason = if s == PlanItemStatus::Blocked {
                            blocked_reason
                                .map(str::trim)
                                .filter(|reason| !reason.is_empty())
                                .map(str::to_string)
                        } else {
                            None
                        };
                    }
                    return true;
                }
                if visit_mut(
                    &mut item.children,
                    target_id,
                    status,
                    blocked_reason,
                    commit,
                ) {
                    return true;
                }
            }
            false
        }

        let updated = visit_mut(
            &mut plan.implementation.items,
            item_id,
            status,
            blocked_reason,
            commit,
        );
        if updated {
            plan.implementation.refresh_parent_statuses();
            plan.updated_at = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            Self::save_plan_unlocked(repo_root, &plan)?;
            Ok(Some(plan))
        } else {
            Ok(None)
        }
    }
}

/// Result of applying plan operations from model output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanApplicationOutcome {
    pub applied: Vec<Plan>,
    /// Mutations the storage barrier rejected; a rejected mutation leaves the
    /// stored plan untouched.
    ///
    /// Every entry is classified where the refusal was produced, so a caller
    /// reads the category from the typed field and never from the wording.
    pub errors: Vec<PlanRejection>,
    /// Repairs the lifecycle barrier applied to an otherwise valid mutation.
    ///
    /// A repaired mutation is applied, so its reports never share the rejection
    /// channel: a caller that treats `errors` as failures stays accurate while
    /// the repair remains visible.
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{DiscussionPlanAction, DiscussionPlanUpdate, PlanRejectionKind};
    use tempfile::tempdir;

    /// Renders the reported wording of a batch of typed rejections, so an
    /// assertion reads the surface text without the entry categories.
    fn rejection_messages(rejections: &[PlanRejection]) -> String {
        rejections
            .iter()
            .map(|rejection| rejection.message.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_plan_storage_crud_and_active() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan::from_parts(
            "auth-flow".to_string(),
            "Authentication Flow".to_string(),
            Some("OAuth2 + JWT".to_string()),
            100,
            100,
            vec![],
            vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Models".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    blocked_reason: None,
                    commit: None,
                    files: vec![],
                    readonly_files: vec![],
                    outer_docs: vec![],
                    concept_refs: vec![],
                    depends_on: vec![],
                    children: vec![PlanItem {
                        id: "1.1".to_string(),
                        title: "Token struct".to_string(),
                        details: None,
                        status: PlanItemStatus::Todo,
                        blocked_reason: None,
                        commit: None,
                        files: vec!["src/token.rs".to_string()],
                        readonly_files: vec![],
                        outer_docs: vec![],
                        concept_refs: vec![],
                        depends_on: vec![],
                        children: vec![],
                    }],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Endpoint".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    blocked_reason: None,
                    commit: None,
                    files: vec!["src/endpoint.rs".to_string()],
                    readonly_files: vec![],
                    outer_docs: vec![],
                    concept_refs: vec![],
                    depends_on: vec![],
                    children: vec![],
                },
            ],
        );

        // 1. Save
        PlanStorage::save_plan(root, &plan).unwrap();

        // 2. List
        let list = PlanStorage::list_plans(root).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "auth-flow");
        // Progress counts executable leaves: items "1.1" and "2" (the "1" parent
        // is a conceptual container and is not counted).
        assert_eq!(list[0].total_items, 2);

        // 3. Load
        let loaded = PlanStorage::load_plan(root, "auth-flow").unwrap().unwrap();
        assert_eq!(loaded.title, "Authentication Flow");

        // 4. Update child item
        let updated = PlanStorage::update_item(
            root,
            "auth-flow",
            "1.1",
            Some(PlanItemStatus::Done),
            None,
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            updated.implementation.items[0].children[0].status,
            PlanItemStatus::Done
        );

        // 5. Active plan marker
        PlanStorage::save_active_id(root, Some("auth-flow")).unwrap();
        assert_eq!(
            PlanStorage::load_active_id(root).unwrap(),
            Some("auth-flow".to_string())
        );

        // 6. Delete
        let deleted = PlanStorage::delete_plan(root, "auth-flow").unwrap();
        assert!(deleted);
        assert_eq!(PlanStorage::list_plans(root).unwrap().len(), 0);
        assert_eq!(PlanStorage::load_active_id(root).unwrap(), None);
    }

    #[test]
    fn test_plan_storage_concept_item_update() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan::from_parts(
            "arch-plan".to_string(),
            "Arch Plan".to_string(),
            None,
            100,
            100,
            vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Original title".to_string(),
                details: None,
                children: vec![],
            }],
            vec![],
        );
        PlanStorage::save_plan(root, &plan).unwrap();

        let updated = PlanStorage::update_concept_item(
            root,
            "arch-plan",
            "c-1",
            Some("New title"),
            Some("New details"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(updated.concept[0].title, "New title");
        assert_eq!(updated.concept[0].details.as_deref(), Some("New details"));
    }

    #[test]
    fn test_save_plan_normalizes_leaf_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan::from_parts(
            "leaf-only".to_string(),
            "Leaf Only".to_string(),
            None,
            1,
            1,
            vec![],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Parent".to_string(),
                status: PlanItemStatus::Todo,
                files: vec!["src/parent.rs".to_string()],
                children: vec![PlanItem {
                    id: "1.1".to_string(),
                    title: "Leaf".to_string(),
                    status: PlanItemStatus::Todo,
                    files: vec!["src/leaf.rs".to_string()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        );

        PlanStorage::save_plan(root, &plan).unwrap();

        let loaded = PlanStorage::load_plan(root, "leaf-only").unwrap().unwrap();
        assert!(loaded.implementation.items[0].files.is_empty());
        assert_eq!(
            loaded.implementation.items[0].children[0].files,
            vec!["src/leaf.rs".to_string()]
        );
    }

    #[test]
    fn test_save_plan_does_not_overwrite_differing_id_with_fuzzy_match() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan1 = Plan::from_parts(
            "plan-1".to_string(),
            "Plan One".to_string(),
            None,
            100,
            100,
            vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Invariant 1".to_string(),
                details: None,
                children: vec![],
            }],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                status: PlanItemStatus::Done,
                files: vec!["src/step.rs".to_string()],
                ..Default::default()
            }],
        );
        PlanStorage::save_plan(root, &plan1).unwrap();

        // Saving "plan-2" (Levenshtein distance 1 from "plan-1") must NOT overwrite "plan-1"
        let plan2 = Plan::from_parts(
            "plan-2".to_string(),
            "Plan Two".to_string(),
            None,
            100,
            100,
            vec![tauqe_protocol::ConceptItem {
                id: "c-2".to_string(),
                title: "Invariant 2".to_string(),
                details: None,
                children: vec![],
            }],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1 in plan 2".to_string(),
                status: PlanItemStatus::Todo,
                files: vec!["src/step.rs".to_string()],
                ..Default::default()
            }],
        );
        PlanStorage::save_plan(root, &plan2).unwrap();

        let loaded1 = PlanStorage::load_plan(root, "plan-1").unwrap().unwrap();
        assert_eq!(loaded1.title, "Plan One");
        let loaded2 = PlanStorage::load_plan(root, "plan-2").unwrap().unwrap();
        assert_eq!(loaded2.title, "Plan Two");
    }
    #[test]
    fn test_save_plan_detects_sanitized_id_collision() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let first = Plan::from_parts(
            "team/auth".to_string(),
            "Team Auth".to_string(),
            None,
            0,
            0,
            vec![],
            vec![],
        );
        PlanStorage::save_plan(root, &first).unwrap();

        // "team-auth" sanitizes to the same storage name as "team/auth".
        let colliding = Plan::from_parts(
            "team-auth".to_string(),
            "Team Auth Alternate".to_string(),
            None,
            0,
            0,
            vec![],
            vec![],
        );
        let err = PlanStorage::save_plan(root, &colliding).unwrap_err();
        assert!(err.to_string().contains("already used by plan 'team/auth'"));

        // The original plan is preserved unchanged.
        let loaded = PlanStorage::load_plan(root, "team/auth").unwrap().unwrap();
        assert_eq!(loaded.id, "team/auth");
        assert_eq!(loaded.title, "Team Auth");
    }

    #[test]
    fn test_list_and_load_all_skip_corrupted_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let valid = Plan::from_parts(
            "valid-plan".to_string(),
            "Valid".to_string(),
            None,
            0,
            0,
            vec![],
            vec![],
        );
        PlanStorage::save_plan(root, &valid).unwrap();

        let corrupt_path = PlanStorage::plans_dir(root).join("broken.json");
        std::fs::write(&corrupt_path, b"{ not a valid plan }").unwrap();

        let summaries = PlanStorage::list_plans(root).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "valid-plan");

        let all = PlanStorage::load_all(root).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "valid-plan");
    }

    #[test]
    fn test_list_plans_orders_by_creation_time() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // The two plans disagree on which timestamp is larger: the older plan
        // was modified most recently, while the newer plan was never touched
        // after creation. Creation time must decide the order, so a freshly
        // edited plan never climbs above a newly created one.
        let older = Plan::from_parts(
            "older".to_string(),
            "Older".to_string(),
            None,
            100,
            900,
            vec![],
            vec![],
        );
        let newer = Plan::from_parts(
            "newer".to_string(),
            "Newer".to_string(),
            None,
            200,
            200,
            vec![],
            vec![],
        );
        PlanStorage::save_plan(root, &older).unwrap();
        PlanStorage::save_plan(root, &newer).unwrap();

        let ids: Vec<String> = PlanStorage::list_plans(root)
            .unwrap()
            .into_iter()
            .map(|summary| summary.id)
            .collect();
        assert_eq!(ids, vec!["newer".to_string(), "older".to_string()]);
    }

    #[test]
    fn test_concurrent_updates_are_serialized_without_lost_writes() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let items: Vec<PlanItem> = (0..16)
            .map(|i| PlanItem {
                id: format!("item-{i}"),
                title: format!("Item {i}"),
                status: PlanItemStatus::Todo,
                files: vec![format!("src/item-{i}.rs")],
                ..Default::default()
            })
            .collect();

        let plan = Plan::from_parts(
            "concurrent-plan".to_string(),
            "Concurrent".to_string(),
            None,
            0,
            0,
            vec![],
            items,
        );
        PlanStorage::save_plan(&root, &plan).unwrap();

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let _ = PlanStorage::update_item(
                        &root,
                        "concurrent-plan",
                        &format!("item-{i}"),
                        Some(PlanItemStatus::Done),
                        None,
                        None,
                    );
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let loaded = PlanStorage::load_plan(&root, "concurrent-plan")
            .unwrap()
            .unwrap();
        let done = loaded
            .implementation
            .items
            .iter()
            .filter(|item| item.status == PlanItemStatus::Done)
            .count();
        assert_eq!(done, 16);
    }

    #[test]
    fn test_save_plan_reuses_differing_filename_instead_of_duplicating() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let first = Plan::from_parts(
            "Tauqe-Server".to_string(),
            "First".to_string(),
            None,
            0,
            0,
            vec![],
            vec![],
        );
        PlanStorage::save_plan(root, &first).unwrap();
        assert!(PlanStorage::plans_dir(root)
            .join("Tauqe-Server.json")
            .is_file());

        // A casing change yields a different sanitized file name; the existing file
        // must be reused instead of writing a second `tauqe-server.json`.
        let second = Plan::from_parts(
            "tauqe-server".to_string(),
            "Second".to_string(),
            None,
            0,
            0,
            vec![],
            vec![],
        );
        PlanStorage::save_plan(root, &second).unwrap();

        let json_files: Vec<_> = std::fs::read_dir(PlanStorage::plans_dir(root))
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        assert_eq!(
            json_files.len(),
            1,
            "expected a single plan file, found {json_files:?}"
        );

        let loaded = PlanStorage::load_plan(root, "tauqe-server")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.title, "Second");
    }

    #[test]
    fn test_delete_plan_resolves_differing_filename() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan::from_parts(
            "Legacy Name".to_string(),
            "Legacy".to_string(),
            None,
            0,
            0,
            vec![],
            vec![],
        );
        PlanStorage::save_plan(root, &plan).unwrap();
        let stored = PlanStorage::plans_dir(root).join("Legacy-Name.json");
        assert!(stored.is_file());

        // Deleting with a different casing resolves the real artifact rather than
        // the absent default target.
        let deleted = PlanStorage::delete_plan(root, "legacy name").unwrap();
        assert!(deleted);
        assert!(!stored.is_file());
    }

    #[test]
    fn test_load_and_delete_respect_sanitized_id_collision() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan::from_parts(
            "team/auth".to_string(),
            "Team Auth".to_string(),
            None,
            0,
            0,
            vec![],
            vec![],
        );
        PlanStorage::save_plan(root, &plan).unwrap();

        // Loading a non-existent colliding ID must return None rather than the
        // colliding plan stored under the same sanitized file name.
        let result = PlanStorage::load_plan(root, "team-auth").unwrap();
        assert!(result.is_none());

        // Deleting a non-existent colliding ID must return false and keep the plan.
        let deleted = PlanStorage::delete_plan(root, "team-auth").unwrap();
        assert!(!deleted);

        let still_exists = PlanStorage::load_plan(root, "team/auth").unwrap();
        assert!(still_exists.is_some());
    }

    #[test]
    fn test_atomic_writes_leave_no_temp_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan::from_parts(
            "atomic-plan".to_string(),
            "Atomic".to_string(),
            None,
            0,
            0,
            vec![],
            vec![],
        );
        PlanStorage::save_plan(root, &plan).unwrap();
        PlanStorage::save_active_id(root, Some("atomic-plan")).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(PlanStorage::plans_dir(root))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "unexpected temp files: {leftovers:?}");

        assert_eq!(
            PlanStorage::load_active_id(root).unwrap(),
            Some("atomic-plan".to_string())
        );
    }

    // Plan storage performs blocking file I/O and waits on a synchronous mutex.
    // Driving the full lifecycle from async tasks on a multi-threaded runtime
    // must neither panic nor stall the executor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_plan_storage_lifecycle_on_multi_thread_runtime() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let plan = Plan::from_parts(
            "runtime-plan".to_string(),
            "Runtime Plan".to_string(),
            None,
            0,
            0,
            vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Invariant".to_string(),
                details: None,
                children: vec![],
            }],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                status: PlanItemStatus::Todo,
                files: vec!["src/step.rs".to_string()],
                ..Default::default()
            }],
        );

        PlanStorage::save_plan(&root, &plan).unwrap();
        assert!(PlanStorage::plans_dir(&root)
            .join("runtime-plan.json")
            .is_file());
        assert_eq!(
            PlanStorage::load_active_id(&root).unwrap(),
            Some("runtime-plan".to_string())
        );

        let summaries = PlanStorage::list_plans(&root).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "runtime-plan");

        let loaded = PlanStorage::load_plan(&root, "runtime-plan")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.implementation.items.len(), 1);
        assert_eq!(loaded.implementation.items[0].status, PlanItemStatus::Todo);

        let updated = PlanStorage::update_item(
            &root,
            "runtime-plan",
            "1",
            Some(PlanItemStatus::Done),
            None,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(updated.implementation.items[0].status, PlanItemStatus::Done);

        let refined = PlanStorage::update_concept_item(
            &root,
            "runtime-plan",
            "c-1",
            Some("Refined invariant"),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(refined.concept[0].title, "Refined invariant");

        PlanStorage::save_active_id(&root, None).unwrap();
        assert_eq!(PlanStorage::load_active_id(&root).unwrap(), None);

        assert!(PlanStorage::delete_plan(&root, "runtime-plan").unwrap());
        assert!(PlanStorage::load_all(&root).unwrap().is_empty());
    }

    #[test]
    fn test_update_item_persists_and_clears_blocked_reason() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan::from_parts(
            "blocked-plan".to_string(),
            "Blocked".to_string(),
            None,
            0,
            0,
            vec![],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                status: PlanItemStatus::Todo,
                files: vec!["src/step.rs".to_string()],
                ..Default::default()
            }],
        );
        PlanStorage::save_plan(root, &plan).unwrap();

        // Entering BLOCKED persists the reported diagnostic, trimmed so the
        // stored report never carries stray padding.
        let blocked = PlanStorage::update_item(
            root,
            "blocked-plan",
            "1",
            Some(PlanItemStatus::Blocked),
            Some("  missing git  "),
            None,
        )
        .unwrap()
        .unwrap();
        let blocked_item = blocked.implementation.find_item("1").unwrap();
        assert_eq!(blocked_item.status, PlanItemStatus::Blocked);
        assert_eq!(blocked_item.blocked_reason.as_deref(), Some("missing git"));

        // The report is durable: it survives a reload from disk.
        let stored = PlanStorage::load_plan(root, "blocked-plan")
            .unwrap()
            .unwrap();
        let stored_item = stored.implementation.find_item("1").unwrap();
        assert_eq!(stored_item.blocked_reason.as_deref(), Some("missing git"));

        // Leaving BLOCKED drops the stale report deterministically.
        let resumed = PlanStorage::update_item(
            root,
            "blocked-plan",
            "1",
            Some(PlanItemStatus::InProgress),
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let resumed_item = resumed.implementation.find_item("1").unwrap();
        assert_eq!(resumed_item.status, PlanItemStatus::InProgress);
        assert!(resumed_item.blocked_reason.is_none());
    }

    /// Builds one executable leaf of a stored plan that declares its target file.
    fn plan_leaf(id: &str, file: &str) -> PlanItem {
        PlanItem {
            id: id.to_string(),
            title: format!("Step {id}"),
            status: PlanItemStatus::Todo,
            files: vec![file.to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn test_save_plan_rejects_duplicate_step_ids() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let first = plan_leaf("1", "src/first.rs");
        let second = plan_leaf("1", "src/second.rs");
        let plan = Plan::from_parts(
            "duplicate-steps".to_string(),
            "Duplicate Steps".to_string(),
            None,
            0,
            0,
            vec![],
            vec![first, second],
        );

        let err = PlanStorage::save_plan(root, &plan).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Cannot save plan 'duplicate-steps'"),
            "{message}"
        );
        assert!(message.contains("Duplicate step ID: '1'"), "{message}");
        assert!(PlanStorage::load_plan(root, "duplicate-steps")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_save_plan_rejects_dependency_cycle() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let mut first = plan_leaf("a", "src/a.rs");
        first.depends_on = vec!["b".to_string()];
        let mut second = plan_leaf("b", "src/b.rs");
        second.depends_on = vec!["a".to_string()];
        let plan = Plan::from_parts(
            "cyclic-plan".to_string(),
            "Cyclic Plan".to_string(),
            None,
            0,
            0,
            vec![],
            vec![first, second],
        );

        let err = PlanStorage::save_plan(root, &plan).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Cannot save plan 'cyclic-plan'"),
            "{message}"
        );
        assert!(message.contains("Cyclic dependency detected"), "{message}");
        assert!(PlanStorage::load_plan(root, "cyclic-plan")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_save_plan_rejects_a_pending_leaf_without_target_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let step = PlanItem {
            id: "1".to_string(),
            title: "Step 1".to_string(),
            status: PlanItemStatus::Todo,
            ..Default::default()
        };
        let plan = Plan::from_parts(
            "fileless-step".to_string(),
            "Fileless Step".to_string(),
            None,
            0,
            0,
            vec![],
            vec![step],
        );

        let err = PlanStorage::save_plan(root, &plan).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Cannot save plan 'fileless-step'"),
            "{message}"
        );
        assert!(message.contains("must declare a target file"), "{message}");
        assert!(PlanStorage::load_plan(root, "fileless-step")
            .unwrap()
            .is_none());
    }

    /// The write barrier is the single structural gate of plan persistence: the
    /// programmatic API, the text-tag contour, and the structured discussion
    /// contour all converge on it, so one defect is rejected identically through
    /// each of them and nothing reaches disk.
    #[test]
    fn test_every_plan_mutation_source_shares_the_structural_barrier() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let invalid_items = vec![
            plan_leaf("1", "src/first.rs"),
            plan_leaf("1", "src/second.rs"),
        ];

        // 1. The programmatic API.
        let direct = Plan::from_parts(
            "barrier-direct".to_string(),
            "Barrier Direct".to_string(),
            None,
            0,
            0,
            vec![],
            invalid_items.clone(),
        );
        let direct_err = PlanStorage::save_plan(root, &direct).unwrap_err();
        assert!(
            direct_err.to_string().contains("Duplicate step ID: '1'"),
            "{direct_err}"
        );

        // 2. The structured discussion contour.
        let update = DiscussionPlanUpdate {
            action: DiscussionPlanAction::Save,
            id: Some("barrier-structured".to_string()),
            title: Some("Barrier Structured".to_string()),
            description: None,
            concept: vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Invariant".to_string(),
                details: None,
                children: vec![],
            }],
            items: invalid_items,
        };
        let structured_outcome = PlanStorage::apply_discussion_plan_update(root, &update);
        let structured_errors = rejection_messages(&structured_outcome.errors);
        assert!(
            structured_outcome
                .errors
                .iter()
                .all(|rejection| rejection.kind == PlanRejectionKind::PayloadDefect),
            "a structural defect is a payload defect: {:?}",
            structured_outcome.errors
        );
        assert!(structured_outcome.applied.is_empty());
        assert!(
            structured_errors.contains("Cannot save plan 'barrier-structured'"),
            "{structured_errors}"
        );

        // No path persisted anything.
        for id in ["barrier-direct", "barrier-structured"] {
            assert!(
                PlanStorage::load_plan(root, id).unwrap().is_none(),
                "the rejected plan '{id}' must never reach disk"
            );
        }
    }
}
