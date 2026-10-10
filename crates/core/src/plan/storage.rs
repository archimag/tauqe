use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tauqe_protocol::{
    DiscussionPlanAction, DiscussionPlanUpdate, Plan, PlanItem, PlanItemStatus, PlanSummary,
};

pub struct PlanStorage;

impl PlanStorage {
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
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(plan) = serde_json::from_str::<Plan>(&content) {
                        summaries.push(plan.summary());
                    }
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
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(plan) = serde_json::from_str::<Plan>(&content) {
                        plans.push(plan);
                    }
                }
            }
        }

        plans.sort_by_key(|p| std::cmp::Reverse(p.updated_at));
        Ok(plans)
    }

    /// Saves a plan into `.tauqe/plans/<id>.json` atomically.
    pub fn save_plan(repo_root: &Path, plan: &Plan) -> Result<()> {
        let dir = Self::plans_dir(repo_root);
        std::fs::create_dir_all(&dir).with_context(|| {
            format!("Failed to create plan storage directory at {}", dir.display())
        })?;

        let safe_id = Self::sanitize_id(&plan.id);
        let target_file = dir.join(format!("{safe_id}.json"));
        let temp_file = dir.join(format!("{safe_id}.json.tmp"));

        let json = serde_json::to_string_pretty(plan)
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
    pub fn delete_plan(repo_root: &Path, id: &str) -> Result<bool> {
        let dir = Self::plans_dir(repo_root);
        let safe_id = Self::sanitize_id(id);
        let target_file = dir.join(format!("{safe_id}.json"));

        if target_file.is_file() {
            std::fs::remove_file(&target_file)
                .with_context(|| format!("Failed to remove plan file at {}", target_file.display()))?;

            // If the deleted plan was active, clear active marker
            if let Ok(Some(act)) = Self::load_active_id(repo_root) {
                if act == id || act == safe_id {
                    let _ = Self::save_active_id(repo_root, None);
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
    pub fn save_active_id(repo_root: &Path, id: Option<&str>) -> Result<()> {
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

    /// Updates status of a plan item recursively.
    pub fn update_item(
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
            Self::save_plan(repo_root, &plan)?;
            Ok(Some(plan))
        } else {
            Ok(None)
        }
    }

    /// Applies pre-parsed plan tags and diagnostics to local storage.
    pub fn apply_parsed_plans(
        repo_root: &Path,
        parsed: Vec<crate::edits::protocol::xml::tags::ParsedPlanTag>,
        mut errors: Vec<String>,
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
                    let mut plan = matched_plan.unwrap_or_else(|| Plan {
                        id: tag.id.clone(),
                        title: tag.title.clone().unwrap_or_else(|| tag.id.clone()),
                        description: tag.description.clone(),
                        created_at: now,
                        updated_at: now,
                        items: Vec::new(),
                    });
                    if let Some(t) = tag.title {
                        plan.title = t;
                    }
                    if tag.description.is_some() {
                        plan.description = tag.description;
                    }
                    plan.items = tag.items;
                    plan.refresh_parent_statuses();
                    plan.updated_at = now;
                    match Self::save_plan(repo_root, &plan) {
                        Ok(()) => {
                            if let Ok(None) = Self::load_active_id(repo_root) {
                                let _ = Self::save_active_id(repo_root, Some(&plan.id));
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
                        match Self::save_plan(repo_root, &plan) {
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
                    match Self::delete_plan(repo_root, &tag.id) {
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

        let parsed_tag = crate::edits::protocol::xml::tags::ParsedPlanTag {
            action,
            id: plan_id,
            title: update.title.clone(),
            description: update.description.clone(),
            items: update.items.clone(),
        };
        Self::apply_parsed_plans(repo_root, vec![parsed_tag], Vec::new())
    }

    /// Applies plan tags (`<plan>...</plan>`) from model output to local storage.
    pub fn apply_plan_tags(repo_root: &Path, text: &str) -> PlanApplicationOutcome {
let (parsed, errors) = crate::edits::protocol::xml::tags::parse_plan_tags_with_diagnostics(text);
Self::apply_parsed_plans(repo_root, parsed, errors)
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
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Models".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    children: vec![PlanItem {
                        id: "1.1".to_string(),
                        title: "Token struct".to_string(),
                        details: None,
                        status: PlanItemStatus::Todo,
                        children: vec![],
                    }],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Endpoint".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
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
        assert_eq!(list[0].total_items, 3);

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
  <item id="1" title="Model" status="todo" />
  <item id="2" title="Handler" status="todo" />
</plan>
"#;

        let affected = PlanStorage::apply_plan_tags(root, model_text);
        assert_eq!(affected.applied.len(), 1);
        assert!(affected.errors.is_empty());
        assert_eq!(affected.applied[0].id, "jwt-auth");
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
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "T".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
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
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Protocol types".to_string(),
                details: Some("Schemas and filters".to_string()),
                status: PlanItemStatus::Done,
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
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Protocol types".to_string(),
                details: Some("Updated details".to_string()),
                status: PlanItemStatus::Done,
                children: vec![],
            }],
        };

        let outcome2 = PlanStorage::apply_discussion_plan_update(root, &update2);
        assert!(outcome2.errors.is_empty());
        assert_eq!(outcome2.applied[0].items[0].details.as_deref(), Some("Updated details"));
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
}
