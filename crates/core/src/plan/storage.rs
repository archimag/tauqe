use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;
use tauqe_protocol::{Plan, PlanItem, PlanItemStatus, PlanSummary};

mod apply;
mod horizon;
mod immutability;

/// Execution mode governing plan mutation permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanMutationMode {
    /// Isolated step execution: any plan creation, modification, or deletion is forbidden.
    IsolatedStep,
    /// Develop mode: only proposing new plans (`save`) is permitted. Modification (`update`) and deletion (`delete`) are forbidden.
    Develop,
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

impl PlanStorage {
    /// Acquires the process-global plan storage lock.
    ///
    /// Recovers the guard if the lock was poisoned by a previous panic, so a single
    /// failure never permanently wedges plan storage.
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

    /// Lists all stored plans from `.tauqe/plans`, ordered by `updated_at` descending.
    ///
    /// Acquires the process-global storage lock so a listing never observes a plan
    /// file mid-rename while another task is persisting it.
    pub fn list_plans(repo_root: &Path) -> Result<Vec<PlanSummary>> {
        let _guard = Self::lock();
        Self::list_plans_unlocked(repo_root)
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
        let _guard = Self::lock();
        Self::load_plan_unlocked(repo_root, id)
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
        let _guard = Self::lock();
        Self::load_all_unlocked(repo_root)
    }

    /// Lock-free core of [`Self::load_all`]; the caller must hold the storage lock.
    fn load_all_unlocked(repo_root: &Path) -> Result<Vec<Plan>> {
        let dir = Self::plans_dir(repo_root);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut plans = Vec::new();
        let entries = std::fs::read_dir(&dir)
            .with_context(|| format!("Failed to read plans directory at {}", dir.display()))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("json") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<Plan>(&content) {
                        Ok(plan) => plans.push(plan),
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
        }

        plans.sort_by_key(|p| std::cmp::Reverse(p.updated_at));
        Ok(plans)
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
        let _guard = Self::lock();
        Self::save_plan_unlocked(repo_root, plan)
    }

    /// Lock-free core of [`Self::save_plan`]; the caller must hold the storage lock.
    ///
    /// The persisted plan enforces the leaf-only file invariant: any item with
    /// children is a conceptual container and is stored without `files`.
    /// Also enforces the `Done is Immutable` invariant against any existing on-disk
    /// plan, and refuses to overwrite a file already holding a different plan whose
    /// sanitized storage name collides with this one.
    fn save_plan_unlocked(repo_root: &Path, plan: &Plan) -> Result<()> {
        let dir = Self::plans_dir(repo_root);
        std::fs::create_dir_all(&dir).with_context(|| {
            format!("Failed to create plan storage directory at {}", dir.display())
        })?;

        let safe_id = Self::sanitize_id(&plan.id);
        let default_target = dir.join(format!("{safe_id}.json"));

        let mut normalized = plan.clone();
        normalized.normalize_leaf_files();
        normalized.refresh_parent_statuses();

        // Reuse the file that already stores this plan even when its name differs
        // from the sanitized id (for example after an id casing change). Writing to
        // the resolved file keeps a single plan per id on disk instead of spawning a
        // duplicate `{safe_id}.json` beside the original.
        let target_file = if default_target.is_file() {
            default_target
        } else {
            Self::find_plan_file_unlocked(repo_root, &plan.id).unwrap_or(default_target)
        };
        let temp_file = target_file.with_extension("json.tmp");

        let existing = if target_file.is_file() {
            let content = std::fs::read_to_string(&target_file)
                .with_context(|| format!("Failed to read existing plan file at {}", target_file.display()))?;
            Some(serde_json::from_str::<Plan>(&content)
                .with_context(|| format!("Failed to parse existing plan JSON at {}", target_file.display()))?)
        } else {
            None
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

        let json = serde_json::to_string_pretty(&normalized)
            .context("Failed to serialize plan to JSON")?;
        std::fs::write(&temp_file, json).with_context(|| {
            format!("Failed to write temporary plan file at {}", temp_file.display())
        })?;

        std::fs::rename(&temp_file, &target_file).with_context(|| {
            format!("Failed to atomically rename plan file to {}", target_file.display())
        })?;

        // If no active plan is set, make this plan active so clients always have
        // a persisted active-plan marker to render.
        if let Ok(None) = Self::load_active_id_unlocked(repo_root) {
            let _ = Self::save_active_id_unlocked(repo_root, Some(&plan.id));
        }

        Ok(())
    }

    /// Deletes a plan from storage. Returns `true` if a file was deleted.
    ///
    /// Serializes against concurrent storage mutations through the process-global lock.
    pub fn delete_plan(repo_root: &Path, id: &str) -> Result<bool> {
        let _guard = Self::lock();
        Self::delete_plan_unlocked(repo_root, id)
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
        let _guard = Self::lock();
        Self::load_active_id_unlocked(repo_root)
    }

    /// Lock-free core of [`Self::load_active_id`]; the caller must hold the storage lock.
    fn load_active_id_unlocked(repo_root: &Path) -> Result<Option<String>> {
        let active_file = Self::plans_dir(repo_root).join("active");
        if !active_file.is_file() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&active_file)
            .with_context(|| format!("Failed to read active plan marker at {}", active_file.display()))?;
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
        let _guard = Self::lock();
        Self::save_active_id_unlocked(repo_root, id)
    }

    /// Lock-free core of [`Self::save_active_id`]; the caller must hold the storage lock.
    fn save_active_id_unlocked(repo_root: &Path, id: Option<&str>) -> Result<()> {
        let dir = Self::plans_dir(repo_root);
        std::fs::create_dir_all(&dir).with_context(|| {
            format!("Failed to create plan storage directory at {}", dir.display())
        })?;

        let active_file = dir.join("active");
        match id {
            Some(val) if !val.trim().is_empty() => {
                std::fs::write(&active_file, val.trim())
                    .with_context(|| format!("Failed to write active plan marker to {}", active_file.display()))?;
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
        let _guard = Self::lock();
        Self::update_concept_item_unlocked(repo_root, plan_id, concept_id, title, details)
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
    pub fn update_item(
        repo_root: &Path,
        plan_id: &str,
        item_id: &str,
        status: Option<PlanItemStatus>,
    ) -> Result<Option<Plan>> {
        let _guard = Self::lock();
        Self::update_item_unlocked(repo_root, plan_id, item_id, status)
    }

    /// Lock-free core of [`Self::update_item`]; the caller must hold the storage lock.
    fn update_item_unlocked(
        repo_root: &Path,
        plan_id: &str,
        item_id: &str,
        status: Option<PlanItemStatus>,
    ) -> Result<Option<Plan>> {
        let mut plan = match Self::load_plan_unlocked(repo_root, plan_id)? {
            Some(p) => p,
            None => return Ok(None),
        };

        fn visit_mut(
            items: &mut [PlanItem],
            target_id: &str,
            status: Option<PlanItemStatus>,
        ) -> bool {
            for item in items {
                if item.id.eq_ignore_ascii_case(target_id) {
                    if let Some(s) = status {
                        item.status = s;
                    }
                    return true;
                }
                if visit_mut(&mut item.children, target_id, status) {
                    return true;
                }
            }
            false
        }

        let updated = visit_mut(&mut plan.items, item_id, status);
        if updated {
            plan.refresh_parent_statuses();
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
    pub errors: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_plan_storage_crud_and_active() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan {
            id: "auth-flow".to_string(),
            title: "Authentication Flow".to_string(),
            description: Some("OAuth2 + JWT".to_string()),
            created_at: 100,
            updated_at: 100,
            concept: vec![],
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Models".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    files: vec![],
                    readonly_files: vec![],
                    concept_refs: vec![],
                    depends_on: vec![],
                    children: vec![PlanItem {
                        id: "1.1".to_string(),
                        title: "Token struct".to_string(),
                        details: None,
                        status: PlanItemStatus::Todo,
                        files: vec![],
                        readonly_files: vec![],
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
                    files: vec![],
                    readonly_files: vec![],
                    concept_refs: vec![],
                    depends_on: vec![],
                    children: vec![],
                },
            ],
        };

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
        )
        .unwrap()
        .unwrap();

        assert_eq!(updated.items[0].children[0].status, PlanItemStatus::Done);

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

        let plan = Plan {
            id: "arch-plan".to_string(),
            title: "Arch Plan".to_string(),
            description: None,
            created_at: 100,
            updated_at: 100,
            concept: vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Original title".to_string(),
                details: None,
                children: vec![],
            }],
            items: vec![],
        };
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

        let plan = Plan {
            id: "leaf-only".to_string(),
            title: "Leaf Only".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            concept: vec![],
            items: vec![PlanItem {
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
        };

        PlanStorage::save_plan(root, &plan).unwrap();

        let loaded = PlanStorage::load_plan(root, "leaf-only").unwrap().unwrap();
        assert!(loaded.items[0].files.is_empty());
        assert_eq!(loaded.items[0].children[0].files, vec!["src/leaf.rs".to_string()]);
    }

    #[test]
    fn test_save_plan_does_not_overwrite_differing_id_with_fuzzy_match() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan1_text = r#"
<plan action="save" id="plan-1" title="Plan One">
  <concept>
    <concept_item id="c-1" title="Invariant 1" />
  </concept>
  <item id="1" title="Step 1" status="done" />
</plan>
"#;
        let outcome1 = PlanStorage::apply_plan_tags(root, plan1_text);
        assert!(outcome1.errors.is_empty());
        assert_eq!(outcome1.applied.len(), 1);

        // Saving "plan-2" (Levenshtein distance 1 from "plan-1") must NOT overwrite "plan-1"
        let plan2_text = r#"
<plan action="save" id="plan-2" title="Plan Two">
  <concept>
    <concept_item id="c-2" title="Invariant 2" />
  </concept>
  <item id="1" title="Step 1 in plan 2" status="todo" />
</plan>
"#;
        let outcome2 = PlanStorage::apply_plan_tags(root, plan2_text);
        assert!(outcome2.errors.is_empty(), "Errors: {:?}", outcome2.errors);
        assert_eq!(outcome2.applied.len(), 1);
        assert_eq!(outcome2.applied[0].id, "plan-2");

        let p1 = PlanStorage::load_plan(root, "plan-1").unwrap().unwrap();
        let p2 = PlanStorage::load_plan(root, "plan-2").unwrap().unwrap();
        assert_eq!(p1.id, "plan-1");
        assert_eq!(p1.title, "Plan One");
        assert_eq!(p2.id, "plan-2");
        assert_eq!(p2.title, "Plan Two");
    }

    #[test]
    fn test_save_plan_detects_sanitized_id_collision() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let first = Plan {
            id: "team/auth".to_string(),
            title: "Team Auth".to_string(),
            ..Default::default()
        };
        PlanStorage::save_plan(root, &first).unwrap();

        // "team-auth" sanitizes to the same storage name as "team/auth".
        let colliding = Plan {
            id: "team-auth".to_string(),
            title: "Team Auth Alternate".to_string(),
            ..Default::default()
        };
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

        let valid = Plan {
            id: "valid-plan".to_string(),
            title: "Valid".to_string(),
            ..Default::default()
        };
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
    fn test_concurrent_updates_are_serialized_without_lost_writes() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let items: Vec<PlanItem> = (0..16)
            .map(|i| PlanItem {
                id: format!("item-{i}"),
                title: format!("Item {i}"),
                status: PlanItemStatus::Todo,
                ..Default::default()
            })
            .collect();

        let plan = Plan {
            id: "concurrent-plan".to_string(),
            title: "Concurrent".to_string(),
            items,
            ..Default::default()
        };
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

        let first = Plan {
            id: "Tauqe-Server".to_string(),
            title: "First".to_string(),
            ..Default::default()
        };
        PlanStorage::save_plan(root, &first).unwrap();
        assert!(PlanStorage::plans_dir(root).join("Tauqe-Server.json").is_file());

        // A casing change yields a different sanitized file name; the existing file
        // must be reused instead of writing a second `tauqe-server.json`.
        let second = Plan {
            id: "tauqe-server".to_string(),
            title: "Second".to_string(),
            ..Default::default()
        };
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

        let loaded = PlanStorage::load_plan(root, "tauqe-server").unwrap().unwrap();
        assert_eq!(loaded.title, "Second");
    }

    #[test]
    fn test_delete_plan_resolves_differing_filename() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan {
            id: "Legacy Name".to_string(),
            title: "Legacy".to_string(),
            ..Default::default()
        };
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

        let plan = Plan {
            id: "team/auth".to_string(),
            title: "Team Auth".to_string(),
            ..Default::default()
        };
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
}
