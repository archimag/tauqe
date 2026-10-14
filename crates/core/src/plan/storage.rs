use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;
use tauqe_protocol::{
    DiscussionPlanAction, DiscussionPlanUpdate, Plan, PlanItem, PlanItemStatus, PlanSummary,
};

pub struct PlanStorage;

/// Process-global lock serializing plan storage mutations.
///
/// Plan storage performs non-atomic read-modify-write cycles (load, mutate, save).
/// Without serialization, concurrent tasks (UI RPC handlers and batch execution)
/// race on the same plan file and silently overwrite each other's changes. Because
/// the server is a single process, a process-global mutex makes each cycle atomic
/// with respect to all other in-process work.
static PLAN_STORAGE_LOCK: Mutex<()> = Mutex::new(());

/// How a `save` action reconciles the item list of an already-stored plan.
///
/// `save` is a full replacement by contract, but it is frequently emitted by an
/// autonomous turn that only intends to refresh the conceptual model. To keep an
/// incidental `save` from silently discarding established steps and their
/// progress, the stored items survive unless the caller explicitly opts into a
/// full replacement.
#[derive(Debug, Clone, Copy)]
enum SaveItemsReplacement {
    /// Replace the stored items with the incoming list, even when it is empty.
    /// Reserved for the explicit structured command path.
    Replace,
    /// Preserve established items unless the incoming `save` carries a non-empty
    /// item list. Used by the implicit text-tag path.
    Guarded,
}

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
    pub fn list_plans(repo_root: &Path) -> Result<Vec<PlanSummary>> {
        let dir = Self::plans_dir(repo_root);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut summaries = Vec::new();
        let entries = std::fs::read_dir(&dir)
            .with_context(|| format!("Failed to read plans directory at {}", dir.display()))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("json") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<Plan>(&content) {
                        Ok(plan) => summaries.push(plan.summary()),
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

        summaries.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(summaries)
    }

    /// Loads a specific plan by its exact ID or file name.
    pub fn load_plan(repo_root: &Path, id: &str) -> Result<Option<Plan>> {
        let dir = Self::plans_dir(repo_root);
        let safe_id = Self::sanitize_id(id);
        let file_path = dir.join(format!("{safe_id}.json"));

        if !file_path.is_file() {
            // Check if any existing plan file matches the ID
            if let Ok(plans) = Self::load_all(repo_root) {
                if let Some(plan) = crate::plan::matcher::resolve_plan_id(id, &plans, None) {
                    return Ok(Some(plan.clone()));
                }
            }
            return Ok(None);
        }

        let content = std::fs::read_to_string(&file_path)
            .with_context(|| format!("Failed to read plan file at {}", file_path.display()))?;
        let plan: Plan = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse plan JSON at {}", file_path.display()))?;

        Ok(Some(plan))
    }

    /// Loads all full plans from disk.
    pub fn load_all(repo_root: &Path) -> Result<Vec<Plan>> {
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

    /// Saves a plan into `.tauqe/plans/<id>.json` atomically.
    ///
    /// The persisted plan enforces the leaf-only file invariant: any item with
    /// children is a conceptual container and is stored without `files`.
    /// Validates that any items marked as `Done` in `existing` are preserved immutably in `updated`:
    /// - Cannot be removed or deleted.
    /// - Cannot regress status (must remain `Done`).
    /// - Cannot modify `title`, `details`, or target `files`.
    pub fn validate_done_immutability(existing: &Plan, updated: &Plan) -> Result<()> {
        fn collect_done_items<'a>(items: &'a [PlanItem], out: &mut Vec<&'a PlanItem>) {
            for item in items {
                if item.status == PlanItemStatus::Done {
                    out.push(item);
                }
                collect_done_items(&item.children, out);
            }
        }

        let mut done_items = Vec::new();
        collect_done_items(&existing.items, &mut done_items);

        for done_item in done_items {
            let updated_item = match updated.find_item(&done_item.id) {
                Some(it) => it,
                None => {
                    anyhow::bail!(
                        "Cannot remove completed step '{}' (status: DONE): completed steps are immutable",
                        done_item.id
                    );
                }
            };

            if updated_item.status != PlanItemStatus::Done {
                anyhow::bail!(
                    "Cannot regress status of completed step '{}' from DONE to {}: completed steps are immutable",
                    done_item.id,
                    updated_item.status
                );
            }

            if updated_item.title.trim() != done_item.title.trim() {
                anyhow::bail!(
                    "Cannot modify title of completed step '{}' ('{}' -> '{}'): completed steps are immutable",
                    done_item.id,
                    done_item.title.trim(),
                    updated_item.title.trim()
                );
            }

            let old_details = done_item.details.as_deref().map(str::trim).filter(|s| !s.is_empty());
            let new_details = updated_item.details.as_deref().map(str::trim).filter(|s| !s.is_empty());
            if old_details != new_details {
                anyhow::bail!(
                    "Cannot modify details of completed step '{}': completed steps are immutable",
                    done_item.id
                );
            }

            if updated_item.files != done_item.files {
                anyhow::bail!(
                    "Cannot modify target files of completed step '{}': completed steps are immutable",
                    done_item.id
                );
            }

            if updated_item.readonly_files != done_item.readonly_files {
                anyhow::bail!(
                    "Cannot modify reference files of completed step '{}': completed steps are immutable",
                    done_item.id
                );
            }

            if updated_item.concept_refs != done_item.concept_refs {
                anyhow::bail!(
                    "Cannot modify concept references of completed step '{}': completed steps are immutable",
                    done_item.id
                );
            }

            if updated_item.depends_on != done_item.depends_on {
                anyhow::bail!(
                    "Cannot modify dependencies of completed step '{}': completed steps are immutable",
                    done_item.id
                );
            }
        }

        Ok(())
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
        let target_file = dir.join(format!("{safe_id}.json"));
        let temp_file = dir.join(format!("{safe_id}.json.tmp"));

        let mut normalized = plan.clone();
        normalized.normalize_leaf_files();
        normalized.refresh_parent_statuses();

        let existing = if target_file.is_file() {
            let content = std::fs::read_to_string(&target_file)
                .with_context(|| format!("Failed to read existing plan file at {}", target_file.display()))?;
            Some(serde_json::from_str::<Plan>(&content)
                .with_context(|| format!("Failed to parse existing plan JSON at {}", target_file.display()))?)
        } else if let Ok(all) = Self::load_all(repo_root) {
            all.into_iter().find(|p| p.id.eq_ignore_ascii_case(&plan.id))
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
        let dir = Self::plans_dir(repo_root);
        let safe_id = Self::sanitize_id(id);
        let target_file = dir.join(format!("{safe_id}.json"));

        if target_file.is_file() {
            std::fs::remove_file(&target_file)
                .with_context(|| format!("Failed to remove plan file at {}", target_file.display()))?;

            // If the deleted plan was active, clear active marker
            if let Ok(Some(act)) = Self::load_active_id(repo_root) {
                if act == id || act == safe_id {
                    let _ = Self::save_active_id_unlocked(repo_root, None);
                }
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Loads the active plan ID from `.tauqe/plans/active`.
    pub fn load_active_id(repo_root: &Path) -> Result<Option<String>> {
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
        let mut plan = match Self::load_plan(repo_root, plan_id)? {
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
        let mut plan = match Self::load_plan(repo_root, plan_id)? {
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

    /// Applies pre-parsed plan tags and diagnostics to local storage.
    ///
    /// Serializes the whole batch against concurrent storage mutations through the
    /// process-global lock, so a batch never interleaves with another task's writes.
    ///
    /// This is the implicit text-tag entry point: an incidental `save` on an
    /// existing plan preserves its established items instead of erasing them.
    pub fn apply_parsed_plans(
        repo_root: &Path,
        parsed: Vec<crate::edits::protocol::xml::tags::ParsedPlanTag>,
        errors: Vec<String>,
    ) -> PlanApplicationOutcome {
        Self::apply_parsed_plans_with_replacement(
            repo_root,
            parsed,
            errors,
            SaveItemsReplacement::Guarded,
        )
    }

    /// Applies pre-parsed plan tags under an explicit item replacement contract.
    ///
    /// Serializes the whole batch against concurrent storage mutations through the
    /// process-global lock, so a batch never interleaves with another task's writes.
    fn apply_parsed_plans_with_replacement(
        repo_root: &Path,
        parsed: Vec<crate::edits::protocol::xml::tags::ParsedPlanTag>,
        errors: Vec<String>,
        replacement: SaveItemsReplacement,
    ) -> PlanApplicationOutcome {
        let _guard = Self::lock();
        Self::apply_parsed_plans_unlocked(repo_root, parsed, errors, replacement)
    }

    /// Lock-free core of [`Self::apply_parsed_plans`]; the caller must hold the storage lock.
    fn apply_parsed_plans_unlocked(
        repo_root: &Path,
        parsed: Vec<crate::edits::protocol::xml::tags::ParsedPlanTag>,
        mut errors: Vec<String>,
        replacement: SaveItemsReplacement,
    ) -> PlanApplicationOutcome {
        if parsed.is_empty() && errors.is_empty() {
            return PlanApplicationOutcome::default();
        }

        let mut applied = Vec::new();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let all_plans = Self::load_all(repo_root).unwrap_or_default();

        for tag in parsed {
            let action = tag.action.to_lowercase();
            let matched_plan = crate::plan::matcher::resolve_plan_id(&tag.id, &all_plans, None).cloned();

            match action.as_str() {
                "save" => {
                    if tag.concept.is_empty() {
                        errors.push(format!(
                            "Cannot save plan '{}': plan must contain a <concept> section defining architectural intent and invariants",
                            tag.id
                        ));
                        continue;
                    }
                    let mut plan = matched_plan.unwrap_or_else(|| Plan {
                        id: tag.id.clone(),
                        title: tag.title.clone().unwrap_or_else(|| tag.id.clone()),
                        description: tag.description.clone(),
                        created_at: now,
                        updated_at: now,
                        concept: Vec::new(),
                        items: Vec::new(),
                    });
                    if let Some(t) = tag.title {
                        plan.title = t;
                    }
                    if tag.description.is_some() {
                        plan.description = tag.description;
                    }
                    if !tag.concept.is_empty() || plan.concept.is_empty() {
                        plan.concept = tag.concept;
                    }
                    match replacement {
                        // An explicit replacement always wins, even when it clears
                        // the stored steps.
                        SaveItemsReplacement::Replace => plan.items = tag.items,
                        // A guarded save must never erase established work: the
                        // incoming items replace the plan only when they are
                        // non-empty.
                        SaveItemsReplacement::Guarded => {
                            if !tag.items.is_empty() {
                                plan.items = tag.items;
                            }
                        }
                    }
                    plan.refresh_parent_statuses();
                    plan.updated_at = now;
                    match Self::save_plan_unlocked(repo_root, &plan) {
                        Ok(()) => {
                            if let Ok(None) = Self::load_active_id(repo_root) {
                                let _ = Self::save_active_id_unlocked(repo_root, Some(&plan.id));
                            }
                            applied.push(plan);
                        }
                        Err(err) => {
                            errors.push(format!("Failed to save plan '{}': {:#}", plan.id, err));
                        }
                    }
                }
                "update" => {
                    if let Some(mut plan) = matched_plan.or_else(|| Self::load_plan(repo_root, &tag.id).ok().flatten()) {
                        if let Some(t) = tag.title {
                            plan.title = t;
                        }
                        if tag.description.is_some() {
                            plan.description = tag.description;
                        }

                        // Update or append concept items
                        fn update_concept_single(
                            concepts: &mut [tauqe_protocol::ConceptItem],
                            update: &tauqe_protocol::ConceptItem,
                        ) -> bool {
                            for item in concepts.iter_mut() {
                                if item.id.eq_ignore_ascii_case(&update.id) {
                                    if !update.title.is_empty() {
                                        item.title = update.title.clone();
                                    }
                                    if update.details.is_some() {
                                        item.details = update.details.clone();
                                    }
                                    return true;
                                }
                                if update_concept_single(&mut item.children, update) {
                                    return true;
                                }
                            }
                            false
                        }

                        for c_update in &tag.concept {
                            if !update_concept_single(&mut plan.concept, c_update) {
                                plan.concept.push(c_update.clone());
                            }
                        }

                        fn update_single(items: &mut [PlanItem], update: &PlanItem) -> bool {
                            for item in items.iter_mut() {
                                if item.id.eq_ignore_ascii_case(&update.id) {
                                    item.status = update.status;
                                    if update.details.is_some() {
                                        item.details = update.details.clone();
                                    }
                                    if !update.title.is_empty() {
                                        item.title = update.title.clone();
                                    }
                                    if !update.files.is_empty() {
                                        item.files = update.files.clone();
                                    }
                                    if !update.concept_refs.is_empty() {
                                        item.concept_refs = update.concept_refs.clone();
                                    }
                                    return true;
                                }
                                if update_single(&mut item.children, update) {
                                    return true;
                                }
                            }
                            false
                        }

                        let mut missing_items = Vec::new();
                        for update_item in &tag.items {
                            if !update_single(&mut plan.items, update_item) {
                                missing_items.push(update_item.id.clone());
                            }
                        }
                        if !missing_items.is_empty() {
                            errors.push(format!(
                                "Item(s) {} not found in plan '{}'",
                                missing_items.iter().map(|id| format!("'{id}'")).collect::<Vec<_>>().join(", "),
                                plan.id
                            ));
                        }

                        plan.refresh_parent_statuses();
                        plan.updated_at = now;
                        match Self::save_plan_unlocked(repo_root, &plan) {
                            Ok(()) => {
                                applied.push(plan);
                            }
                            Err(err) => {
                                errors.push(format!("Failed to save updated plan '{}': {:#}", plan.id, err));
                            }
                        }
                    } else {
                        let available: Vec<String> = all_plans.iter().map(|p| p.id.clone()).collect();
                        let hint = if available.is_empty() {
                            "no plans exist in storage".to_string()
                        } else {
                            format!("available plans: {}", available.join(", "))
                        };
                        errors.push(format!(
                            "Cannot update plan '{}': plan not found ({hint})",
                            tag.id
                        ));
                    }
                }
                "delete" => {
                    match Self::delete_plan_unlocked(repo_root, &tag.id) {
                        Ok(true) => {}
                        Ok(false) => {
                            errors.push(format!("Cannot delete plan '{}': plan not found", tag.id));
                        }
                        Err(err) => {
                            errors.push(format!("Failed to delete plan '{}': {:#}", tag.id, err));
                        }
                    }
                }
        other => {
            errors.push(format!("Unsupported plan action '{other}' (expected save, update, or delete)"));
        }
    }
}

PlanApplicationOutcome { applied, errors }
    }

    /// Applies a structured `DiscussionPlanUpdate` directly to local plan storage.
    pub fn apply_discussion_plan_update(
        repo_root: &Path,
        update: &DiscussionPlanUpdate,
    ) -> PlanApplicationOutcome {
        let all_plans = Self::load_all(repo_root).unwrap_or_default();
        let active_id = Self::load_active_id(repo_root).ok().flatten();
        let plan_id = update
            .id
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| {
                crate::plan::matcher::resolve_plan_id("", &all_plans, active_id.as_deref())
                    .map(|p| p.id.clone())
            })
            .or(active_id)
            .unwrap_or_else(|| "default".to_string());

        let action = if update.action == DiscussionPlanAction::Update
            && all_plans.is_empty()
            && Self::load_plan(repo_root, &plan_id).ok().flatten().is_none()
        {
            "save".to_string()
        } else {
            update.action.to_string()
        };

        if action == "save" && update.concept.is_empty() {
            return PlanApplicationOutcome {
                applied: Vec::new(),
                errors: vec![format!(
                    "Cannot save plan '{}': plan must contain a concept tree defining architectural intent and invariants",
                    plan_id
                )],
            };
        }

        let parsed_tag = crate::edits::protocol::xml::tags::ParsedPlanTag {
            action,
            id: plan_id,
            title: update.title.clone(),
            description: update.description.clone(),
            concept: update.concept.clone(),
            items: update.items.clone(),
        };
        // The structured discussion command is the explicit architect path: its
        // typed, deliberate item list (including an explicit empty list) fully
        // replaces the stored plan.
        Self::apply_parsed_plans_with_replacement(
            repo_root,
            vec![parsed_tag],
            Vec::new(),
            SaveItemsReplacement::Replace,
        )
    }

    /// Applies plan tags (`<plan>...</plan>`) from model output to local storage.
    pub fn apply_plan_tags(repo_root: &Path, text: &str) -> PlanApplicationOutcome {
let (parsed, errors) = crate::edits::protocol::xml::tags::parse_plan_tags_with_diagnostics(text);
Self::apply_parsed_plans(repo_root, parsed, errors)
    }

    /// Locally replaces a blocked leaf step with a structured sub-tree produced by the
    /// Architect during semantic recovery (Rolling Wave Planning).
    ///
    /// The graft is strictly local: the target step must be an executable leaf that is
    /// not `Done` (completed steps are immutable). The sub-tree items are spliced into
    /// the target's position, so the preceding tasks and the independent remainder of
    /// the plan stay untouched. Every downstream task that depended on the replaced
    /// step is re-pointed at the exit items of the grafted sub-tree, keeping the
    /// dependent tail waiting on the decomposed work. Leaf-only file invariants are
    /// normalized before the plan is persisted atomically.
    pub fn graft_subtree(
        repo_root: &Path,
        plan_id: &str,
        target_step_id: &str,
        new_subtree: Vec<PlanItem>,
    ) -> Result<Plan> {
        let _guard = Self::lock();
        Self::graft_subtree_unlocked(repo_root, plan_id, target_step_id, new_subtree)
    }

    /// Lock-free core of [`Self::graft_subtree`]; the caller must hold the storage lock.
    fn graft_subtree_unlocked(
        repo_root: &Path,
        plan_id: &str,
        target_step_id: &str,
        new_subtree: Vec<PlanItem>,
    ) -> Result<Plan> {
        if new_subtree.is_empty() {
            anyhow::bail!(
                "Cannot graft an empty sub-tree over step '{}'",
                target_step_id
            );
        }

        let mut plan = match Self::load_plan(repo_root, plan_id)? {
            Some(p) => p,
            None => anyhow::bail!("Plan '{}' not found", plan_id),
        };

        let target = match plan.find_item(target_step_id) {
            Some(item) => item.clone(),
            None => anyhow::bail!(
                "Step '{}' not found in plan '{}'",
                target_step_id,
                plan_id
            ),
        };

        if !target.is_leaf() {
            anyhow::bail!(
                "Step '{}' ({}) is not a leaf and cannot be grafted; graft only over executable leaf steps",
                target.id,
                target.title
            );
        }

        if target.status == PlanItemStatus::Done {
            anyhow::bail!(
                "Step '{}' is DONE and immutable; completed steps cannot be replaced",
                target.id
            );
        }

        let exits = subtree_exit_ids(&new_subtree);

        // Re-point the dependent tail before splicing so the grafted sub-tree itself is
        // never rewritten.
        if !exits.is_empty() {
            remap_dependencies(&mut plan.items, &target.id, &exits);
        }

        if !splice_replacement(&mut plan.items, &target.id, &new_subtree) {
            anyhow::bail!(
                "Failed to locate step '{}' for grafting in plan '{}'",
                target.id,
                plan_id
            );
        }

        if let Some(cycle_step) = detect_dependency_cycle(&plan.items) {
            anyhow::bail!(
                "Cycle detected in plan dependencies after grafting: step '{}' participates in a dependency loop",
                cycle_step
            );
        }

        plan.normalize_leaf_files();
        plan.refresh_parent_statuses();
        plan.updated_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self::save_plan_unlocked(repo_root, &plan)?;
        Ok(plan)
    }
}

/// Splices `replacement` into the position of the item whose id matches `target_id`,
/// preserving sibling order. Returns true when the target was found and replaced.
fn splice_replacement(
    items: &mut Vec<PlanItem>,
    target_id: &str,
    replacement: &[PlanItem],
) -> bool {
    if let Some(index) = items
        .iter()
        .position(|item| item.id.eq_ignore_ascii_case(target_id))
    {
        items.splice(index..=index, replacement.iter().cloned());
        return true;
    }
    for item in items.iter_mut() {
        if splice_replacement(&mut item.children, target_id, replacement) {
            return true;
        }
    }
    false
}

/// Rewrites every `depends_on` reference to `old_id` into references to `new_ids`,
/// preserving unrelated dependencies and de-duplicating the result.
fn remap_dependencies(items: &mut [PlanItem], old_id: &str, new_ids: &[String]) {
    for item in items.iter_mut() {
        if item
            .depends_on
            .iter()
            .any(|dep| dep.eq_ignore_ascii_case(old_id))
        {
            let mut remapped: Vec<String> = Vec::with_capacity(item.depends_on.len());
            for dep in &item.depends_on {
                if dep.eq_ignore_ascii_case(old_id) {
                    for new_id in new_ids {
                        if !remapped.iter().any(|r| r.eq_ignore_ascii_case(new_id)) {
                            remapped.push(new_id.clone());
                        }
                    }
                } else if !remapped.iter().any(|r| r.eq_ignore_ascii_case(dep)) {
                    remapped.push(dep.clone());
                }
            }
            item.depends_on = remapped;
        }
        remap_dependencies(&mut item.children, old_id, new_ids);
    }
}

/// Collects the exit item ids of a grafted sub-tree: the top-level items that no other
/// item in the sub-tree depends on. When every top-level item is referenced (or the
/// sub-tree holds a single item), the last item acts as the continuation point.
fn subtree_exit_ids(subtree: &[PlanItem]) -> Vec<String> {
    let mut referenced: HashSet<String> = HashSet::new();
    fn collect(items: &[PlanItem], referenced: &mut HashSet<String>) {
        for item in items {
            for dep in &item.depends_on {
                referenced.insert(dep.to_ascii_lowercase());
            }
            collect(&item.children, referenced);
        }
    }
    collect(subtree, &mut referenced);

    let mut exits: Vec<String> = subtree
        .iter()
        .filter(|item| !referenced.contains(&item.id.to_ascii_lowercase()))
        .map(|item| item.id.clone())
        .collect();

    if exits.is_empty() {
        if let Some(last) = subtree.last() {
            exits.push(last.id.clone());
        }
    }
    exits
}

/// Detects a dependency cycle in the plan's `depends_on` graph.
///
/// Every plan item is a node; each id in its `depends_on` list is a directed edge
/// to the referenced item. Returns the id of a step that closes a cycle when one
/// exists, or `None` when the graph is acyclic. References to unknown ids are
/// ignored here, since readiness evaluation reports them separately.
fn detect_dependency_cycle(items: &[PlanItem]) -> Option<String> {
    #[derive(Clone, Copy)]
    enum Mark {
        Visiting,
        Done,
    }

    fn collect(items: &[PlanItem], graph: &mut HashMap<String, Vec<String>>) {
        for item in items {
            let deps = item
                .depends_on
                .iter()
                .map(|dep| dep.to_ascii_lowercase())
                .collect();
            graph.insert(item.id.to_ascii_lowercase(), deps);
            collect(&item.children, graph);
        }
    }

    fn visit(
        node: &str,
        graph: &HashMap<String, Vec<String>>,
        marks: &mut HashMap<String, Mark>,
    ) -> Option<String> {
        match marks.get(node).copied() {
            Some(Mark::Visiting) => return Some(node.to_string()),
            Some(Mark::Done) => return None,
            None => {}
        }
        marks.insert(node.to_string(), Mark::Visiting);
        if let Some(deps) = graph.get(node) {
            for dep in deps {
                if !graph.contains_key(dep) {
                    continue;
                }
                if let Some(cycle) = visit(dep, graph, marks) {
                    return Some(cycle);
                }
            }
        }
        marks.insert(node.to_string(), Mark::Done);
        None
    }

    let mut graph: HashMap<String, Vec<String>> = HashMap::new();
    collect(items, &mut graph);

    let mut marks: HashMap<String, Mark> = HashMap::new();
    for node in graph.keys() {
        if let Some(cycle) = visit(node, &graph, &mut marks) {
            return Some(cycle);
        }
    }
    None
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
    fn test_apply_plan_tags() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let model_text = r#"
I have prepared the plan:
<plan action="save" id="jwt-auth" title="JWT Auth">
  <description>Auth migration</description>
  <concept>
    <concept_item id="c-1" title="Stateless tokens" />
  </concept>
  <item id="1" title="Model" status="todo" />
  <item id="2" title="Handler" status="todo" />
</plan>
"#;

        let affected = PlanStorage::apply_plan_tags(root, model_text);
        assert_eq!(affected.applied.len(), 1);
        assert!(affected.errors.is_empty());
        assert_eq!(affected.applied[0].id, "jwt-auth");
        assert_eq!(affected.applied[0].concept.len(), 1);
        assert_eq!(affected.applied[0].items.len(), 2);
        assert_eq!(PlanStorage::load_active_id(root).unwrap(), Some("jwt-auth".to_string()));

        let update_text = r#"
Step 1 is done:
<plan action="update" id="jwt-auth">
  <item id="1" status="done" />
</plan>
"#;
        let affected_update = PlanStorage::apply_plan_tags(root, update_text);
        assert_eq!(affected_update.applied.len(), 1);
        assert!(affected_update.errors.is_empty());
        assert_eq!(affected_update.applied[0].items[0].status, PlanItemStatus::Done);
        assert_eq!(affected_update.applied[0].items[1].status, PlanItemStatus::Todo);
    }

    #[test]
    fn test_apply_plan_tags_diagnostics() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // 1. Unclosed tag
        let unclosed = "<plan action=\"save\" id=\"broken\">";
        let res = PlanStorage::apply_plan_tags(root, unclosed);
        assert!(res.applied.is_empty());
        assert!(!res.errors.is_empty());
        assert!(res.errors[0].contains("missing closing tag"));

        // 2. Non-existent plan on update
        let update_missing = "<plan action=\"update\" id=\"non-existent\"><item id=\"1\" status=\"done\" /></plan>";
        let res_missing = PlanStorage::apply_plan_tags(root, update_missing);
        assert!(res_missing.applied.is_empty());
        assert_eq!(res_missing.errors.len(), 1);
        assert!(res_missing.errors[0].contains("Cannot update plan 'non-existent'"));

        // 3. Non-existent item on update
        let valid_plan = Plan {
            id: "my-plan".to_string(),
            title: "P".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "T".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                files: vec![],
                readonly_files: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        };
        PlanStorage::save_plan(root, &valid_plan).unwrap();

        let update_item_missing = "<plan action=\"update\" id=\"my-plan\"><item id=\"99\" status=\"done\" /></plan>";
        let res_item = PlanStorage::apply_plan_tags(root, update_item_missing);
        assert_eq!(res_item.applied.len(), 1);
        assert_eq!(res_item.errors.len(), 1);
        assert!(res_item.errors[0].contains("Item(s) '99' not found in plan 'my-plan'"));
    }

    #[test]
    fn test_apply_discussion_plan_update() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let update = DiscussionPlanUpdate {
            action: DiscussionPlanAction::Save,
            id: Some("structured-auth".to_string()),
            title: Some("Structured Auth".to_string()),
            description: Some("Implement auth via structured output".to_string()),
            concept: vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Structured auth requirement".to_string(),
                details: None,
                children: vec![],
            }],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Protocol types".to_string(),
                details: Some("Schemas and filters".to_string()),
                status: PlanItemStatus::InProgress,
                files: vec![],
                readonly_files: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        };

        let outcome = PlanStorage::apply_discussion_plan_update(root, &update);
        assert!(outcome.errors.is_empty());
        assert_eq!(outcome.applied.len(), 1);
        assert_eq!(outcome.applied[0].id, "structured-auth");
        assert_eq!(PlanStorage::load_active_id(root).unwrap(), Some("structured-auth".to_string()));

        let update2 = DiscussionPlanUpdate {
            action: DiscussionPlanAction::Update,
            id: Some("structured-auth".to_string()),
            title: None,
            description: None,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Protocol types".to_string(),
                details: Some("Updated details".to_string()),
                status: PlanItemStatus::InProgress,
                files: vec![],
                readonly_files: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        };

        let outcome2 = PlanStorage::apply_discussion_plan_update(root, &update2);
        assert!(outcome2.errors.is_empty());
        assert_eq!(outcome2.applied[0].items[0].details.as_deref(), Some("Updated details"));
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
    fn test_save_plan_clears_items_when_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan {
            id: "test-plan".to_string(),
            title: "Test".to_string(),
            description: Some("Desc".to_string()),
            created_at: 100,
            updated_at: 100,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                ..Default::default()
            }],
        };
        PlanStorage::save_plan(root, &plan).unwrap();
        assert_eq!(PlanStorage::load_plan(root, "test-plan").unwrap().unwrap().items.len(), 1);

        let update = DiscussionPlanUpdate {
            action: DiscussionPlanAction::Save,
            id: Some("test-plan".to_string()),
            title: Some("Test".to_string()),
            description: Some("Updated Desc without items".to_string()),
            concept: vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Architecture concept".to_string(),
                details: None,
                children: vec![],
            }],
            items: vec![],
        };
        let outcome = PlanStorage::apply_discussion_plan_update(root, &update);
        assert!(outcome.errors.is_empty());
        assert_eq!(outcome.applied.len(), 1);
        assert_eq!(outcome.applied[0].items.len(), 0);

        let reloaded = PlanStorage::load_plan(root, "test-plan").unwrap().unwrap();
        assert_eq!(reloaded.items.len(), 0);
        assert_eq!(reloaded.description.as_deref(), Some("Updated Desc without items"));
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
    fn test_validate_done_immutability_prevents_deletion() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan {
            id: "done-plan".to_string(),
            title: "Done Plan".to_string(),
            description: None,
            created_at: 10,
            updated_at: 10,
            concept: vec![],
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Step 1".to_string(),
                    status: PlanItemStatus::Done,
                    ..Default::default()
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Step 2".to_string(),
                    status: PlanItemStatus::Todo,
                    ..Default::default()
                },
            ],
        };
        PlanStorage::save_plan(root, &plan).unwrap();

        // Attempt to remove Step 1
        let truncated = Plan {
            id: "done-plan".to_string(),
            title: "Done Plan".to_string(),
            description: None,
            created_at: 10,
            updated_at: 20,
            concept: vec![],
            items: vec![PlanItem {
                id: "2".to_string(),
                title: "Step 2".to_string(),
                status: PlanItemStatus::Todo,
                ..Default::default()
            }],
        };
        let err = PlanStorage::save_plan(root, &truncated).unwrap_err();
        assert!(err.to_string().contains("Cannot remove completed step '1'"));
    }

    #[test]
    fn test_validate_done_immutability_prevents_regression_and_mutation() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan {
            id: "done-plan".to_string(),
            title: "Done Plan".to_string(),
            description: None,
            created_at: 10,
            updated_at: 10,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Original Title".to_string(),
                details: Some("Original Details".to_string()),
                status: PlanItemStatus::Done,
                files: vec!["src/done.rs".to_string()],
                readonly_files: vec!["src/contract.rs".to_string()],
                concept_refs: vec!["c-1".to_string()],
                depends_on: vec!["0".to_string()],
                ..Default::default()
            }],
        };
        PlanStorage::save_plan(root, &plan).unwrap();

        // 1. Status regression
        let mut regressed = plan.clone();
        regressed.items[0].status = PlanItemStatus::Todo;
        let err1 = PlanStorage::save_plan(root, &regressed).unwrap_err();
        assert!(err1.to_string().contains("Cannot regress status of completed step '1'"));

        // 2. Title mutation
        let mut title_mutated = plan.clone();
        title_mutated.items[0].title = "Altered Title".to_string();
        let err2 = PlanStorage::save_plan(root, &title_mutated).unwrap_err();
        assert!(err2.to_string().contains("Cannot modify title of completed step '1'"));

        // 3. Details mutation
        let mut details_mutated = plan.clone();
        details_mutated.items[0].details = Some("Altered Details".to_string());
        let err3 = PlanStorage::save_plan(root, &details_mutated).unwrap_err();
        assert!(err3.to_string().contains("Cannot modify details of completed step '1'"));

        // 4. Target files mutation
        let mut files_mutated = plan.clone();
        files_mutated.items[0].files = vec!["src/other.rs".to_string()];
        let err4 = PlanStorage::save_plan(root, &files_mutated).unwrap_err();
        assert!(err4.to_string().contains("Cannot modify target files of completed step '1'"));

        // 5. Reference files mutation
        let mut readonly_mutated = plan.clone();
        readonly_mutated.items[0].readonly_files = vec!["src/other.rs".to_string()];
        let err5 = PlanStorage::save_plan(root, &readonly_mutated).unwrap_err();
        assert!(err5
            .to_string()
            .contains("Cannot modify reference files of completed step '1'"));

        // 6. Concept references mutation
        let mut concept_mutated = plan.clone();
        concept_mutated.items[0].concept_refs = vec!["c-2".to_string()];
        let err6 = PlanStorage::save_plan(root, &concept_mutated).unwrap_err();
        assert!(err6
            .to_string()
            .contains("Cannot modify concept references of completed step '1'"));

        // 7. Dependency graph mutation
        let mut deps_mutated = plan.clone();
        deps_mutated.items[0].depends_on = vec!["2".to_string()];
        let err7 = PlanStorage::save_plan(root, &deps_mutated).unwrap_err();
        assert!(err7
            .to_string()
            .contains("Cannot modify dependencies of completed step '1'"));
    }

    #[test]
    fn test_apply_parsed_plans_and_discussion_enforce_done_immutability() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let initial_text = r#"
<plan action="save" id="guard-plan" title="Guard">
  <concept>
    <concept_item id="c-1" title="Invariant" />
  </concept>
  <item id="1" title="Step 1" status="done" />
  <item id="2" title="Step 2" status="todo" />
</plan>
"#;
        let initial_res = PlanStorage::apply_plan_tags(root, initial_text);
        assert!(initial_res.errors.is_empty());
        assert_eq!(initial_res.applied.len(), 1);

        // 1. apply_plan_tags update attempting status regression
        let invalid_update = r#"
<plan action="update" id="guard-plan">
  <item id="1" status="todo" />
</plan>
"#;
        let invalid_res = PlanStorage::apply_plan_tags(root, invalid_update);
        assert!(invalid_res.applied.is_empty());
        assert!(!invalid_res.errors.is_empty());
        assert!(invalid_res.errors[0].contains("Cannot regress status of completed step '1'"));

        // 2. apply_discussion_plan_update attempting to delete completed step
        let invalid_disc = DiscussionPlanUpdate {
            action: DiscussionPlanAction::Save,
            id: Some("guard-plan".to_string()),
            title: Some("Guard".to_string()),
            description: None,
            concept: vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Invariant".to_string(),
                details: None,
                children: vec![],
            }],
            items: vec![PlanItem {
                id: "2".to_string(),
                title: "Step 2".to_string(),
                status: PlanItemStatus::Done,
                ..Default::default()
            }],
        };
        let disc_res = PlanStorage::apply_discussion_plan_update(root, &invalid_disc);
        assert!(disc_res.applied.is_empty());
        assert!(!disc_res.errors.is_empty());
        assert!(disc_res.errors[0].contains("Cannot remove completed step '1'"));
    }

    #[test]
    fn test_graft_subtree_replaces_blocked_leaf_and_remaps_dependents() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan {
            id: "graft-plan".to_string(),
            title: "Graft Plan".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            concept: vec![],
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Done foundation".to_string(),
                    status: PlanItemStatus::Done,
                    ..Default::default()
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Blocked endpoint".to_string(),
                    details: Some("Implement handler".to_string()),
                    status: PlanItemStatus::Blocked,
                    files: vec!["src/endpoint.rs".to_string()],
                    ..Default::default()
                },
                PlanItem {
                    id: "3".to_string(),
                    title: "Dependent integration".to_string(),
                    status: PlanItemStatus::Todo,
                    files: vec!["src/integration.rs".to_string()],
                    depends_on: vec!["1".to_string(), "2".to_string()],
                    ..Default::default()
                },
            ],
        };
        PlanStorage::save_plan(root, &plan).unwrap();

        let replacement = vec![
            PlanItem {
                id: "2.1".to_string(),
                title: "Split model".to_string(),
                status: PlanItemStatus::Todo,
                files: vec!["src/model.rs".to_string()],
                readonly_files: vec!["src/contract.rs".to_string()],
                ..Default::default()
            },
            PlanItem {
                id: "2.2".to_string(),
                title: "Split handler".to_string(),
                status: PlanItemStatus::Todo,
                files: vec!["src/handler.rs".to_string()],
                depends_on: vec!["2.1".to_string()],
                ..Default::default()
            },
        ];

        let grafted =
            PlanStorage::graft_subtree(root, "graft-plan", "2", replacement).unwrap();

        // The blocked leaf is replaced by the sub-tree in place.
        assert_eq!(grafted.items.len(), 4);
        assert_eq!(grafted.items[0].id, "1");
        assert_eq!(grafted.items[1].id, "2.1");
        assert_eq!(grafted.items[2].id, "2.2");
        assert_eq!(grafted.items[3].id, "3");
        assert!(grafted.find_item("2").is_none());

        // The dependent tail is re-pointed at the sub-tree exit item.
        let dependent = grafted.find_item("3").unwrap();
        assert_eq!(dependent.depends_on, vec!["1".to_string(), "2.2".to_string()]);
    }

    #[test]
    fn test_graft_subtree_rejects_done_non_leaf_and_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan {
            id: "graft-reject".to_string(),
            title: "Graft Reject".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            concept: vec![],
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Done".to_string(),
                    status: PlanItemStatus::Done,
                    ..Default::default()
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Parent".to_string(),
                    status: PlanItemStatus::Todo,
                    children: vec![PlanItem {
                        id: "2.1".to_string(),
                        title: "Child".to_string(),
                        status: PlanItemStatus::Todo,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
        };
        PlanStorage::save_plan(root, &plan).unwrap();

        let replacement = vec![PlanItem {
            id: "new".to_string(),
            title: "New".to_string(),
            status: PlanItemStatus::Todo,
            ..Default::default()
        }];

        // DONE steps are immutable.
        let err_done =
            PlanStorage::graft_subtree(root, "graft-reject", "1", replacement.clone())
                .unwrap_err();
        assert!(err_done.to_string().contains("DONE and immutable"));

        // Only leaves can be grafted.
        let err_parent =
            PlanStorage::graft_subtree(root, "graft-reject", "2", replacement.clone())
                .unwrap_err();
        assert!(err_parent.to_string().contains("not a leaf"));

        // Empty sub-trees are rejected.
        let err_empty =
            PlanStorage::graft_subtree(root, "graft-reject", "2.1", Vec::new()).unwrap_err();
        assert!(err_empty.to_string().contains("empty sub-tree"));
    }

    #[test]
    fn test_graft_subtree_rejects_cyclic_dependencies() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan {
            id: "cycle-plan".to_string(),
            title: "Cycle Plan".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            concept: vec![],
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Blocked step".to_string(),
                    status: PlanItemStatus::Blocked,
                    ..Default::default()
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Dependent".to_string(),
                    status: PlanItemStatus::Todo,
                    depends_on: vec!["1".to_string()],
                    ..Default::default()
                },
            ],
        };
        PlanStorage::save_plan(root, &plan).unwrap();

        // The grafted exit node depends back on the dependent tail, closing a cycle.
        let replacement = vec![PlanItem {
            id: "1.1".to_string(),
            title: "Split".to_string(),
            status: PlanItemStatus::Todo,
            depends_on: vec!["2".to_string()],
            ..Default::default()
        }];

        let err = PlanStorage::graft_subtree(root, "cycle-plan", "1", replacement).unwrap_err();
        assert!(err
            .to_string()
            .contains("Cycle detected in plan dependencies after grafting"));
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
    fn test_apply_plan_tags_save_preserves_existing_items_when_none_provided() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let initial = r#"
<plan action="save" id="guarded-save" title="Guarded Save">
  <concept>
    <concept_item id="c-1" title="Concept" />
  </concept>
  <item id="1" title="Step 1" status="in_progress" />
  <item id="2" title="Step 2" status="todo" />
</plan>
"#;
        let seeded = PlanStorage::apply_plan_tags(root, initial);
        assert!(seeded.errors.is_empty(), "Errors: {:?}", seeded.errors);
        assert_eq!(seeded.applied.len(), 1);
        assert_eq!(seeded.applied[0].items.len(), 2);

        // A text-tag save that only refreshes the concept must not erase the steps.
        let concept_only = r#"
<plan action="save" id="guarded-save" title="Guarded Save">
  <concept>
    <concept_item id="c-1" title="Refined concept" />
  </concept>
</plan>
"#;
        let refreshed = PlanStorage::apply_plan_tags(root, concept_only);
        assert!(refreshed.errors.is_empty(), "Errors: {:?}", refreshed.errors);
        assert_eq!(refreshed.applied.len(), 1);
        assert_eq!(refreshed.applied[0].items.len(), 2);
        assert_eq!(
            refreshed.applied[0].items[0].status,
            PlanItemStatus::InProgress
        );

        let reloaded = PlanStorage::load_plan(root, "guarded-save").unwrap().unwrap();
        assert_eq!(reloaded.items.len(), 2);
        assert_eq!(reloaded.concept[0].title, "Refined concept");
    }

    #[test]
    fn test_apply_plan_tags_save_replaces_items_when_provided() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let initial = r#"
<plan action="save" id="guarded-replace" title="Guarded Replace">
  <concept>
    <concept_item id="c-1" title="Concept" />
  </concept>
  <item id="1" title="Step 1" status="todo" />
  <item id="2" title="Step 2" status="todo" />
</plan>
"#;
        let seeded = PlanStorage::apply_plan_tags(root, initial);
        assert!(seeded.errors.is_empty(), "Errors: {:?}", seeded.errors);

        // A save that supplies a replacement list still reshapes the plan.
        let reshaped = r#"
<plan action="save" id="guarded-replace" title="Guarded Replace">
  <concept>
    <concept_item id="c-1" title="Concept" />
  </concept>
  <item id="1" title="Step 1" status="done" />
</plan>
"#;
        let applied = PlanStorage::apply_plan_tags(root, reshaped);
        assert!(applied.errors.is_empty(), "Errors: {:?}", applied.errors);
        assert_eq!(applied.applied.len(), 1);
        assert_eq!(applied.applied[0].items.len(), 1);
        assert_eq!(applied.applied[0].items[0].status, PlanItemStatus::Done);
    }
}
