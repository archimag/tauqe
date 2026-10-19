use anyhow::Result;
use tauqe_protocol::{Plan, PlanItem, PlanItemStatus};

use super::PlanStorage;

impl PlanStorage {
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

            let old_details = done_item
                .details
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let new_details = updated_item
                .details
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{DiscussionPlanAction, DiscussionPlanUpdate};
    use tempfile::tempdir;

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
            architect: None,
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
            architect: None,
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
            architect: None,
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
        assert!(err1
            .to_string()
            .contains("Cannot regress status of completed step '1'"));

        // 2. Title mutation
        let mut title_mutated = plan.clone();
        title_mutated.items[0].title = "Altered Title".to_string();
        let err2 = PlanStorage::save_plan(root, &title_mutated).unwrap_err();
        assert!(err2
            .to_string()
            .contains("Cannot modify title of completed step '1'"));

        // 3. Details mutation
        let mut details_mutated = plan.clone();
        details_mutated.items[0].details = Some("Altered Details".to_string());
        let err3 = PlanStorage::save_plan(root, &details_mutated).unwrap_err();
        assert!(err3
            .to_string()
            .contains("Cannot modify details of completed step '1'"));

        // 4. Target files mutation
        let mut files_mutated = plan.clone();
        files_mutated.items[0].files = vec!["src/other.rs".to_string()];
        let err4 = PlanStorage::save_plan(root, &files_mutated).unwrap_err();
        assert!(err4
            .to_string()
            .contains("Cannot modify target files of completed step '1'"));

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
}
