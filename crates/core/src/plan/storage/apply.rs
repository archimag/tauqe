use crate::edits::protocol::xml::tags::{ParsedPlanItem, ParsedPlanTag};
use std::path::Path;
use std::time::SystemTime;
use tauqe_protocol::{DiscussionPlanAction, DiscussionPlanUpdate, Plan, PlanItem};

use super::{PlanApplicationOutcome, PlanMutationMode, PlanStorage};

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
    /// Applies pre-parsed plan tags and diagnostics to local storage.
    ///
    /// Serializes the whole batch against concurrent storage mutations through the
    /// process-global lock, so a batch never interleaves with another task's writes.
    ///
    /// This is the implicit text-tag entry point: an incidental `save` on an
    /// existing plan preserves its established items instead of erasing them.
    pub fn apply_parsed_plans(
        repo_root: &Path,
        parsed: Vec<ParsedPlanTag>,
        errors: Vec<String>,
    ) -> PlanApplicationOutcome {
        Self::apply_parsed_plans_with_mode(repo_root, parsed, errors, PlanMutationMode::Discussion)
    }

    /// Applies pre-parsed plan tags and diagnostics to local storage under a specific [`PlanMutationMode`].
    pub fn apply_parsed_plans_with_mode(
        repo_root: &Path,
        parsed: Vec<ParsedPlanTag>,
        errors: Vec<String>,
        mode: PlanMutationMode,
    ) -> PlanApplicationOutcome {
        Self::apply_parsed_plans_with_replacement(
            repo_root,
            parsed,
            errors,
            SaveItemsReplacement::Guarded,
            mode,
        )
    }

    /// Applies pre-parsed plan tags under an explicit item replacement contract.
    ///
    /// Serializes the whole batch against concurrent storage mutations through the
    /// process-global lock, so a batch never interleaves with another task's writes.
    fn apply_parsed_plans_with_replacement(
        repo_root: &Path,
        parsed: Vec<ParsedPlanTag>,
        errors: Vec<String>,
        replacement: SaveItemsReplacement,
        mode: PlanMutationMode,
    ) -> PlanApplicationOutcome {
        // The lock wait, the plan reads, and the atomic writes of a whole batch
        // run off the async executor thread, so a slow disk never stalls the
        // bounded turn, provider streaming, or RPC handling.
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::apply_parsed_plans_unlocked(repo_root, parsed, errors, replacement, mode)
        })
    }

    /// Lock-free core of [`Self::apply_parsed_plans`]; the caller must hold the storage lock.
    fn apply_parsed_plans_unlocked(
        repo_root: &Path,
        parsed: Vec<ParsedPlanTag>,
        mut errors: Vec<String>,
        replacement: SaveItemsReplacement,
        mode: PlanMutationMode,
    ) -> PlanApplicationOutcome {
        if parsed.is_empty() && errors.is_empty() {
            return PlanApplicationOutcome::default();
        }

        let mut applied = Vec::new();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let all_plans = Self::load_all_unlocked(repo_root).unwrap_or_default();

        for tag in parsed {
            let action = tag.action.to_lowercase();

            match mode {
                PlanMutationMode::IsolatedStep => {
                    errors.push(format!(
                        "Cannot perform plan action '{}' during isolated step execution: plan mutations are strictly prohibited during step execution",
                        action
                    ));
                    continue;
                }
                PlanMutationMode::Develop => {
                    if action != "save" {
                        errors.push(format!(
                            "Cannot perform plan action '{}' in Develop mode: only 'save' is permitted (plan modifications and deletions belong exclusively in Discussion mode)",
                            action
                        ));
                        continue;
                    }
                    let already_exists =
                        crate::plan::matcher::resolve_plan_id_exact(&tag.id, &all_plans).is_some()
                            || applied
                                .iter()
                                .any(|p: &Plan| p.id.eq_ignore_ascii_case(&tag.id));
                    if already_exists {
                        errors.push(format!(
                            "Cannot modify existing plan '{}' in Develop mode: modifying existing plans belongs exclusively in Discussion mode",
                            tag.id
                        ));
                        continue;
                    }
                }
                PlanMutationMode::Discussion => {}
            }

            match action.as_str() {
                "save" => {
                    if tag.concept.is_empty() {
                        errors.push(format!(
                            "Cannot save plan '{}': plan must contain a <concept> section defining architectural intent and invariants",
                            tag.id
                        ));
                        continue;
                    }
                    let matched_plan =
                        crate::plan::matcher::resolve_plan_id_exact(&tag.id, &all_plans).cloned();
                    let mut plan = matched_plan.unwrap_or_else(|| Plan {
                        id: tag.id.clone(),
                        title: tag.title.clone().unwrap_or_else(|| tag.id.clone()),
                        description: tag.description.clone(),
                        created_at: now,
                        updated_at: now,
                        architect: None,
                        last_replanned_step: None,
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
                    let items: Vec<PlanItem> = tag
                        .items
                        .into_iter()
                        .map(ParsedPlanItem::into_plan_item)
                        .collect();
                    match replacement {
                        // An explicit replacement always wins, even when it clears
                        // the stored steps.
                        SaveItemsReplacement::Replace => plan.items = items,
                        // A guarded save must never erase established work: the
                        // incoming items replace the plan only when they are
                        // non-empty.
                        SaveItemsReplacement::Guarded => {
                            if !items.is_empty() {
                                plan.items = items;
                            }
                        }
                    }
                    plan.refresh_parent_statuses();
                    plan.updated_at = now;
                    match Self::save_plan_unlocked(repo_root, &plan) {
                        Ok(()) => {
                            if let Ok(None) = Self::load_active_id_unlocked(repo_root) {
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
                    let matched_plan =
                        match crate::plan::matcher::resolve_plan(&tag.id, &all_plans, None) {
                            crate::plan::matcher::PlanResolution::Found(p) => Some(p.clone()),
                            crate::plan::matcher::PlanResolution::Ambiguous(candidates) => {
                                let candidate_ids: Vec<String> =
                                    candidates.iter().map(|p| format!("'{}'", p.id)).collect();
                                errors.push(format!(
                                    "Cannot update plan '{}': ambiguous match, candidates: {}",
                                    tag.id,
                                    candidate_ids.join(", ")
                                ));
                                continue;
                            }
                            crate::plan::matcher::PlanResolution::NotFound => {
                                Self::load_plan_unlocked(repo_root, &tag.id).ok().flatten()
                            }
                        };

                    if let Some(mut plan) = matched_plan {
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

                        fn update_single(items: &mut [PlanItem], update: &ParsedPlanItem) -> bool {
                            for item in items.iter_mut() {
                                if item.id.eq_ignore_ascii_case(&update.id) {
                                    if let Some(s) = update.status {
                                        item.status = s;
                                    }
                                    if update.details.is_some() {
                                        item.details = update.details.clone();
                                    }
                                    if !update.title.is_empty() {
                                        item.title = update.title.clone();
                                    }
                                    if !update.files.is_empty() {
                                        item.files = update.files.clone();
                                    }
                                    if !update.readonly_files.is_empty() {
                                        item.readonly_files = update.readonly_files.clone();
                                    }
                                    if !update.concept_refs.is_empty() {
                                        item.concept_refs = update.concept_refs.clone();
                                    }
                                    if !update.depends_on.is_empty() {
                                        item.depends_on = update.depends_on.clone();
                                    }
                                    return true;
                                }
                                if update_single(&mut item.children, update) {
                                    return true;
                                }
                            }
                            false
                        }

                        // A delta `update` mirrors the concept-node upsert: an
                        // identifier that is not found is appended to the plan root
                        // instead of failing the whole mutation, so one `update`
                        // both patches established steps and adds new ones.
                        let errors_before = errors.len();
                        let mut added_items: Vec<PlanItem> = Vec::new();
                        for update_item in &tag.items {
                            if update_single(&mut plan.items, update_item) {
                                continue;
                            }
                            // Only executable leaves are added this way: creating or
                            // moving containers restructures the plan and stays the
                            // sole province of `save`.
                            if !update_item.children.is_empty() {
                                errors.push(format!(
                                    "Cannot add step '{}' to plan '{}': update may add leaf steps only, use save to restructure the plan",
                                    update_item.id, plan.id
                                ));
                                continue;
                            }
                            added_items.push(update_item.clone().into_plan_item());
                        }
                        plan.items.append(&mut added_items);

                        // A rejected entry leaves the stored plan untouched, so the
                        // caller repairs the payload instead of persisting a partial
                        // merge.
                        if errors.len() > errors_before {
                            continue;
                        }

                        plan.normalize_leaf_files();
                        plan.refresh_parent_statuses();
                        // The merged plan is validated as a whole before it reaches
                        // disk: a repeated identifier, a dependency that does not
                        // exist, or an unknown concept reference fails the mutation
                        // instead of being written.
                        if let Err(plan_errors) = plan.validate() {
                            errors.push(format!(
                                "Cannot apply update to plan '{}': {}",
                                plan.id,
                                plan_errors
                                    .iter()
                                    .map(|e| e.to_string())
                                    .collect::<Vec<_>>()
                                    .join("; ")
                            ));
                            continue;
                        }

                        plan.updated_at = now;
                        match Self::save_plan_unlocked(repo_root, &plan) {
                            Ok(()) => {
                                applied.push(plan);
                            }
                            Err(err) => {
                                errors.push(format!(
                                    "Failed to save updated plan '{}': {:#}",
                                    plan.id, err
                                ));
                            }
                        }
                    } else {
                        let available: Vec<String> =
                            all_plans.iter().map(|p| p.id.clone()).collect();
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
                    let target_id =
                        crate::plan::matcher::resolve_plan_id_exact(&tag.id, &all_plans)
                            .map(|p| p.id.as_str())
                            .unwrap_or(&tag.id);
                    match Self::delete_plan_unlocked(repo_root, target_id) {
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
                    errors.push(format!(
                        "Unsupported plan action '{other}' (expected save, update, or delete)"
                    ));
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
        Self::apply_discussion_plan_update_with_mode(
            repo_root,
            update,
            PlanMutationMode::Discussion,
        )
    }

    /// Applies a structured `DiscussionPlanUpdate` directly to local plan storage under a specific [`PlanMutationMode`].
    pub fn apply_discussion_plan_update_with_mode(
        repo_root: &Path,
        update: &DiscussionPlanUpdate,
        mode: PlanMutationMode,
    ) -> PlanApplicationOutcome {
        // Resolution and application share one lock scope, and that whole scope
        // runs off the async executor thread: the read-modify-write cycle waits
        // neither on the executor thread nor for a stalled disk.
        crate::blocking::run_blocking(move || {
            // Resolve the target plan and the effective action under the same lock
            // that guards the follow-up mutation, so a concurrent task cannot create
            // or swap the active plan between resolution and application.
            let _guard = Self::lock();

            let all_plans = Self::load_all_unlocked(repo_root).unwrap_or_default();
            let active_id = Self::load_active_id_unlocked(repo_root).ok().flatten();
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
                && Self::load_plan_unlocked(repo_root, &plan_id)
                    .ok()
                    .flatten()
                    .is_none()
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

            let parsed_tag = ParsedPlanTag {
                action,
                id: plan_id,
                title: update.title.clone(),
                description: update.description.clone(),
                concept: update.concept.clone(),
                items: update.items.clone().into_iter().map(Into::into).collect(),
            };
            // The structured discussion command is the explicit architect path: its
            // typed, deliberate item list (including an explicit empty list) fully
            // replaces the stored plan.
            Self::apply_parsed_plans_unlocked(
                repo_root,
                vec![parsed_tag],
                Vec::new(),
                SaveItemsReplacement::Replace,
                mode,
            )
        })
    }

    /// Applies plan tags (`<plan>...</plan>`) from model output to local storage under a specific [`PlanMutationMode`].
    pub fn apply_plan_tags_with_mode(
        repo_root: &Path,
        text: &str,
        mode: PlanMutationMode,
    ) -> PlanApplicationOutcome {
        let (parsed, errors) =
            crate::edits::protocol::xml::tags::parse_plan_tags_with_diagnostics(text);
        Self::apply_parsed_plans_with_mode(repo_root, parsed, errors, mode)
    }

    /// Applies plan tags (`<plan>...</plan>`) from model output to local storage.
    pub fn apply_plan_tags(repo_root: &Path, text: &str) -> PlanApplicationOutcome {
        Self::apply_plan_tags_with_mode(repo_root, text, PlanMutationMode::Discussion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::PlanItemStatus;
    use tempfile::tempdir;

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
        assert_eq!(
            PlanStorage::load_active_id(root).unwrap(),
            Some("jwt-auth".to_string())
        );

        let update_text = r#"
Step 1 is done:
<plan action="update" id="jwt-auth">
  <item id="1" status="done" />
</plan>
"#;
        let affected_update = PlanStorage::apply_plan_tags(root, update_text);
        assert_eq!(affected_update.applied.len(), 1);
        assert!(affected_update.errors.is_empty());
        assert_eq!(
            affected_update.applied[0].items[0].status,
            PlanItemStatus::Done
        );
        assert_eq!(
            affected_update.applied[0].items[1].status,
            PlanItemStatus::Todo
        );
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
        let update_missing =
            "<plan action=\"update\" id=\"non-existent\"><item id=\"1\" status=\"done\" /></plan>";
        let res_missing = PlanStorage::apply_plan_tags(root, update_missing);
        assert!(res_missing.applied.is_empty());
        assert_eq!(res_missing.errors.len(), 1);
        assert!(res_missing.errors[0].contains("Cannot update plan 'non-existent'"));

        // 3. Non-existent item on update
        let valid_plan = Plan {
            id: "my-plan".to_string(),
            last_replanned_step: None,
            title: "P".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            architect: None,
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

        let update_item_missing =
            "<plan action=\"update\" id=\"my-plan\"><item id=\"99\" status=\"done\" /></plan>";
        let res_item = PlanStorage::apply_plan_tags(root, update_item_missing);
        assert_eq!(res_item.applied.len(), 1);
        assert!(res_item.errors.is_empty(), "Errors: {:?}", res_item.errors);
        let reloaded = PlanStorage::load_plan(root, "my-plan").unwrap().unwrap();
        let added = reloaded
            .find_item("99")
            .expect("an unknown step must be added by update");
        assert_eq!(added.status, PlanItemStatus::Done);
    }

    #[test]
    fn test_update_patches_existing_step_and_adds_new_leaf() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let initial = r#"
<plan action="save" id="upsert-plan" title="Upsert">
  <concept>
    <concept_item id="c-1" title="Invariant" />
  </concept>
  <item id="1" title="Step 1" status="in_progress" />
</plan>
"#;
        let seeded = PlanStorage::apply_plan_tags(root, initial);
        assert!(seeded.errors.is_empty(), "Errors: {:?}", seeded.errors);
        assert_eq!(seeded.applied.len(), 1);

        let update = r#"
<plan action="update" id="upsert-plan">
  <item id="1" title="Step 1 refined" concept="c-1" />
  <item id="2" title="Step 2" status="todo" deps="1" />
</plan>
"#;
        let applied = PlanStorage::apply_plan_tags(root, update);
        assert!(applied.errors.is_empty(), "Errors: {:?}", applied.errors);
        assert_eq!(applied.applied.len(), 1);

        let reloaded = PlanStorage::load_plan(root, "upsert-plan")
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.items.len(), 2);

        let step1 = reloaded.find_item("1").expect("step 1 must exist");
        assert_eq!(step1.title, "Step 1 refined");
        assert_eq!(step1.concept_refs, vec!["c-1"]);
        // A partial patch keeps the status the step already carried.
        assert_eq!(step1.status, PlanItemStatus::InProgress);

        let step2 = reloaded.find_item("2").expect("step 2 must be added");
        assert_eq!(step2.depends_on, vec!["1"]);
    }

    #[test]
    fn test_discussion_update_adds_new_leaf_step() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let seed = DiscussionPlanUpdate {
            action: DiscussionPlanAction::Save,
            id: Some("structured-upsert".to_string()),
            title: Some("Structured Upsert".to_string()),
            description: None,
            concept: vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Invariant".to_string(),
                details: None,
                children: vec![],
            }],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                status: PlanItemStatus::Todo,
                ..Default::default()
            }],
        };
        let seeded = PlanStorage::apply_discussion_plan_update(root, &seed);
        assert!(seeded.errors.is_empty(), "Errors: {:?}", seeded.errors);

        let update = DiscussionPlanUpdate {
            action: DiscussionPlanAction::Update,
            id: Some("structured-upsert".to_string()),
            title: None,
            description: None,
            concept: vec![],
            items: vec![PlanItem {
                id: "2".to_string(),
                title: "Step 2".to_string(),
                status: PlanItemStatus::Todo,
                depends_on: vec!["1".to_string()],
                ..Default::default()
            }],
        };
        let applied = PlanStorage::apply_discussion_plan_update(root, &update);
        assert!(applied.errors.is_empty(), "Errors: {:?}", applied.errors);
        assert_eq!(applied.applied.len(), 1);
        assert_eq!(applied.applied[0].items.len(), 2);

        let reloaded = PlanStorage::load_plan(root, "structured-upsert")
            .unwrap()
            .unwrap();
        assert!(reloaded.find_item("2").is_some());
    }

    #[test]
    fn test_update_rejects_invalid_new_steps() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let initial = r#"
<plan action="save" id="validation-plan" title="Validation">
  <concept>
    <concept_item id="c-1" title="Invariant" />
  </concept>
  <item id="1" title="Step 1" status="todo" />
</plan>
"#;
        let seeded = PlanStorage::apply_plan_tags(root, initial);
        assert!(seeded.errors.is_empty(), "Errors: {:?}", seeded.errors);

        // Duplicate identifiers inside the added batch.
        let duplicates = r#"
<plan action="update" id="validation-plan">
  <item id="2" title="First" status="todo" />
  <item id="2" title="Second" status="todo" />
</plan>
"#;
        let dup_res = PlanStorage::apply_plan_tags(root, duplicates);
        assert!(dup_res.applied.is_empty());
        assert_eq!(dup_res.errors.len(), 1);
        assert!(dup_res.errors[0].contains("Cannot apply update to plan 'validation-plan'"));
        assert!(dup_res.errors[0].contains("Duplicate step ID: '2'"));

        // A new step naming a dependency that does not exist.
        let dangling = r#"
<plan action="update" id="validation-plan">
  <item id="3" title="Step 3" status="todo" deps="99" />
</plan>
"#;
        let dangling_res = PlanStorage::apply_plan_tags(root, dangling);
        assert!(dangling_res.applied.is_empty());
        assert_eq!(dangling_res.errors.len(), 1);
        assert!(dangling_res.errors[0].contains("Cannot apply update to plan 'validation-plan'"));
        assert!(dangling_res.errors[0].contains("references non-existent dependency '99'"));

        // Restructuring the tree is not a delta update operation.
        let container = r#"
<plan action="update" id="validation-plan">
  <item id="4" title="Container">
    <item id="4.1" title="Child" status="todo" />
  </item>
</plan>
"#;
        let container_res = PlanStorage::apply_plan_tags(root, container);
        assert!(container_res.applied.is_empty());
        assert_eq!(container_res.errors.len(), 1);
        assert!(container_res.errors[0].contains("update may add leaf steps only"));

        // Nothing reached disk.
        let reloaded = PlanStorage::load_plan(root, "validation-plan")
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.items.len(), 1);
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
        assert_eq!(
            PlanStorage::load_active_id(root).unwrap(),
            Some("structured-auth".to_string())
        );

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
        assert_eq!(
            outcome2.applied[0].items[0].details.as_deref(),
            Some("Updated details")
        );
    }

    #[test]
    fn test_save_plan_clears_items_when_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan {
            id: "test-plan".to_string(),
            last_replanned_step: None,
            title: "Test".to_string(),
            description: Some("Desc".to_string()),
            created_at: 100,
            updated_at: 100,
            architect: None,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                ..Default::default()
            }],
        };
        PlanStorage::save_plan(root, &plan).unwrap();
        assert_eq!(
            PlanStorage::load_plan(root, "test-plan")
                .unwrap()
                .unwrap()
                .items
                .len(),
            1
        );

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
        assert_eq!(
            reloaded.description.as_deref(),
            Some("Updated Desc without items")
        );
    }

    #[test]
    fn test_update_plan_ambiguous_reports_error_without_mutating() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let p1 = Plan {
            id: "feature-alpha".to_string(),
            title: "Feature Alpha".to_string(),
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                status: PlanItemStatus::Todo,
                ..Default::default()
            }],
            ..Default::default()
        };
        let p2 = Plan {
            id: "feature-beta".to_string(),
            title: "Feature Beta".to_string(),
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                status: PlanItemStatus::Todo,
                ..Default::default()
            }],
            ..Default::default()
        };
        PlanStorage::save_plan(root, &p1).unwrap();
        PlanStorage::save_plan(root, &p2).unwrap();

        // "feature" matches both plans as a prefix
        let update_text = r#"
<plan action="update" id="feature">
  <item id="1" status="done" />
</plan>
"#;
        let outcome = PlanStorage::apply_plan_tags(root, update_text);
        assert!(outcome.applied.is_empty());
        assert_eq!(outcome.errors.len(), 1);
        assert!(outcome.errors[0].contains("ambiguous match"));
        assert!(outcome.errors[0].contains("'feature-alpha'"));
        assert!(outcome.errors[0].contains("'feature-beta'"));

        // Neither plan was modified
        let p1_reloaded = PlanStorage::load_plan(root, "feature-alpha")
            .unwrap()
            .unwrap();
        let p2_reloaded = PlanStorage::load_plan(root, "feature-beta")
            .unwrap()
            .unwrap();
        assert_eq!(p1_reloaded.items[0].status, PlanItemStatus::Todo);
        assert_eq!(p2_reloaded.items[0].status, PlanItemStatus::Todo);
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
        assert!(
            refreshed.errors.is_empty(),
            "Errors: {:?}",
            refreshed.errors
        );
        assert_eq!(refreshed.applied.len(), 1);
        assert_eq!(refreshed.applied[0].items.len(), 2);
        assert_eq!(
            refreshed.applied[0].items[0].status,
            PlanItemStatus::InProgress
        );

        let reloaded = PlanStorage::load_plan(root, "guarded-save")
            .unwrap()
            .unwrap();
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

    #[test]
    fn test_plan_mutation_mode_enforcement() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let initial_plan = r#"
<plan action="save" id="plan-mode-test" title="Initial">
  <concept>
    <concept_item id="c-1" title="Core Invariant" />
  </concept>
  <item id="1" title="Step 1" status="todo" />
</plan>
"#;

        // 1. IsolatedStep mode forbids any plan actions (including save)
        let res_isolated = PlanStorage::apply_plan_tags_with_mode(
            root,
            initial_plan,
            PlanMutationMode::IsolatedStep,
        );
        assert!(res_isolated.applied.is_empty());
        assert_eq!(res_isolated.errors.len(), 1);
        assert!(res_isolated.errors[0].contains("isolated step execution"));

        // 2. Develop mode permits saving a new plan
        let res_develop_save =
            PlanStorage::apply_plan_tags_with_mode(root, initial_plan, PlanMutationMode::Develop);
        assert_eq!(res_develop_save.applied.len(), 1);
        assert!(res_develop_save.errors.is_empty());

        // 3. Develop mode forbids updating an existing plan
        let update_text = r#"
<plan action="update" id="plan-mode-test">
  <item id="1" status="done" />
</plan>
"#;
        let res_develop_update =
            PlanStorage::apply_plan_tags_with_mode(root, update_text, PlanMutationMode::Develop);
        assert!(res_develop_update.applied.is_empty());
        assert_eq!(res_develop_update.errors.len(), 1);
        assert!(res_develop_update.errors[0].contains("in Develop mode: only 'save' is permitted"));

        // 4. Develop mode forbids deleting a plan
        let delete_text = r#"<plan action="delete" id="plan-mode-test" />"#;
        let res_develop_delete =
            PlanStorage::apply_plan_tags_with_mode(root, delete_text, PlanMutationMode::Develop);
        assert!(res_develop_delete.applied.is_empty());
        assert_eq!(res_develop_delete.errors.len(), 1);
        assert!(res_develop_delete.errors[0].contains("in Develop mode: only 'save' is permitted"));

        // 5. Develop mode forbids modifying an existing plan even with action="save"
        let overwrite_text = r#"
<plan action="save" id="plan-mode-test" title="Overwritten">
  <concept>
    <concept_item id="c-2" title="New Invariant" />
  </concept>
</plan>
"#;
        let res_develop_overwrite =
            PlanStorage::apply_plan_tags_with_mode(root, overwrite_text, PlanMutationMode::Develop);
        assert!(res_develop_overwrite.applied.is_empty());
        assert_eq!(res_develop_overwrite.errors.len(), 1);
        assert!(res_develop_overwrite.errors[0]
            .contains("Cannot modify existing plan 'plan-mode-test' in Develop mode"));

        // 6. Discussion mode permits update
        let res_disc_update =
            PlanStorage::apply_plan_tags_with_mode(root, update_text, PlanMutationMode::Discussion);
        assert_eq!(res_disc_update.applied.len(), 1);
        assert!(res_disc_update.errors.is_empty());
        assert_eq!(
            res_disc_update.applied[0].items[0].status,
            PlanItemStatus::Done
        );
    }

    #[test]
    fn test_update_plan_item_without_status_preserves_existing_status() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let initial_plan = Plan {
            id: "status-preserve".to_string(),
            title: "Status Preserve".to_string(),
            concept: vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Concept".to_string(),
                details: None,
                children: vec![],
            }],
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Step 1".to_string(),
                    status: PlanItemStatus::InProgress,
                    readonly_files: vec!["crates/core/src/lib.rs".to_string()],
                    depends_on: vec!["2".to_string()],
                    ..Default::default()
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Step 2".to_string(),
                    status: PlanItemStatus::Blocked,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        PlanStorage::save_plan(root, &initial_plan).unwrap();

        // 1. Partial update without status attribute must NOT regress InProgress or Blocked to Todo
        let partial_update = r#"
<plan action="update" id="status-preserve">
  <item id="1" title="Step 1 Updated" />
</plan>
"#;
        let outcome = PlanStorage::apply_plan_tags(root, partial_update);
        assert!(outcome.errors.is_empty(), "Errors: {:?}", outcome.errors);
        assert_eq!(outcome.applied.len(), 1);

        let plan = PlanStorage::load_plan(root, "status-preserve")
            .unwrap()
            .unwrap();
        assert_eq!(plan.items[0].title, "Step 1 Updated");
        assert_eq!(plan.items[0].status, PlanItemStatus::InProgress);
        assert_eq!(plan.items[0].readonly_files, vec!["crates/core/src/lib.rs"]);
        assert_eq!(plan.items[0].depends_on, vec!["2"]);
        assert_eq!(plan.items[1].status, PlanItemStatus::Blocked);

        // 2. Update with explicit readonly_files and depends_on
        let update_with_deps = r#"
<plan action="update" id="status-preserve">
  <item id="1" readonly="crates/protocol/src/lib.rs" deps="2" />
</plan>
"#;
        let outcome2 = PlanStorage::apply_plan_tags(root, update_with_deps);
        assert!(outcome2.errors.is_empty(), "Errors: {:?}", outcome2.errors);

        let plan2 = PlanStorage::load_plan(root, "status-preserve")
            .unwrap()
            .unwrap();
        assert_eq!(plan2.items[0].status, PlanItemStatus::InProgress);
        assert_eq!(
            plan2.items[0].readonly_files,
            vec!["crates/protocol/src/lib.rs"]
        );
        assert_eq!(plan2.items[0].depends_on, vec!["2"]);
    }
}
