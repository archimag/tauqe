use std::path::Path;
use std::time::SystemTime;
use tauqe_protocol::{
    DiscussionPlanAction, DiscussionPlanUpdate, Plan, PlanItem, PlanItemStatus, PlanRejection,
};

use super::{PlanApplicationOutcome, PlanBatchSnapshot, PlanMutationMode, PlanStorage};

/// Contract of one applied mutation of a plan batch.
#[derive(Debug, Clone, Copy)]
enum MutationCommit {
    /// The mutation is written to storage.
    Persist,
    /// The mutation is validated and recorded in the batch snapshot only.
    Probe,
}

impl MutationCommit {
    /// Reports whether this contract writes the mutation to storage.
    fn persists(self) -> bool {
        matches!(self, Self::Persist)
    }

    /// Applies one validated plan under this contract and returns the document
    /// the batch now carries.
    fn commit(
        self,
        repo_root: &Path,
        snapshot: &mut PlanBatchSnapshot,
        plan: &Plan,
    ) -> anyhow::Result<Plan> {
        match self {
            Self::Persist => PlanStorage::save_plan_in_snapshot_unlocked(repo_root, snapshot, plan),
            Self::Probe => {
                let prepared = PlanStorage::prepare_plan_unlocked(repo_root, plan, Some(snapshot))?;
                snapshot.remember(prepared.plan.clone(), prepared.target_file);
                Ok(prepared.plan)
            }
        }
    }
}

/// Reports whether a model mutation may create a step in `status`.
fn is_creatable_status(status: PlanItemStatus) -> bool {
    matches!(status, PlanItemStatus::Todo | PlanItemStatus::Discussion)
}

/// Status lifecycle barrier of one plan mutation.
struct StatusBarrier<'a> {
    plan_id: String,
    warnings: &'a mut Vec<String>,
}

impl<'a> StatusBarrier<'a> {
    fn new(plan_id: &str, warnings: &'a mut Vec<String>) -> Self {
        Self {
            plan_id: plan_id.to_string(),
            warnings,
        }
    }

    fn existing(
        &mut self,
        item_id: &str,
        stored: PlanItemStatus,
        requested: PlanItemStatus,
    ) -> PlanItemStatus {
        if requested != PlanItemStatus::InProgress || stored == PlanItemStatus::InProgress {
            return requested;
        }
        self.warnings.push(format!(
            "Plan '{}': step '{}' cannot be moved to {} by a plan mutation; it keeps {}",
            self.plan_id, item_id, requested, stored
        ));
        stored
    }

    fn created(&mut self, item: &mut PlanItem) {
        if is_creatable_status(item.status) {
            return;
        }
        let requested = item.status;
        item.status = PlanItemStatus::Todo;
        self.warnings.push(format!(
            "Plan '{}': new step '{}' requested status {}; created as {} (a new step may only be {} or {})",
            self.plan_id,
            item.id,
            requested,
            item.status,
            PlanItemStatus::Todo,
            PlanItemStatus::Discussion
        ));
    }

    fn saved(&mut self, stored: &Plan, item: &mut PlanItem) {
        match stored.implementation.find_item(&item.id) {
            Some(established) => {
                item.status = self.existing(&item.id, established.status, item.status);
            }
            None => self.created(item),
        }
        for child in &mut item.children {
            self.saved(stored, child);
        }
    }
}

#[derive(Debug, Clone)]
struct PlanMutationItem {
    id: String,
    title: String,
    details: Option<String>,
    status: Option<PlanItemStatus>,
    files: Vec<String>,
    readonly_files: Vec<String>,
    outer_docs: Vec<String>,
    concept_refs: Vec<String>,
    depends_on: Vec<String>,
    children: Vec<PlanMutationItem>,
}

impl From<PlanItem> for PlanMutationItem {
    fn from(item: PlanItem) -> Self {
        Self {
            id: item.id,
            title: item.title,
            details: item.details,
            status: Some(item.status),
            files: item.files,
            readonly_files: item.readonly_files,
            outer_docs: item.outer_docs,
            concept_refs: item.concept_refs,
            depends_on: item.depends_on,
            children: item.children.into_iter().map(Into::into).collect(),
        }
    }
}

impl PlanMutationItem {
    fn into_plan_item(self) -> PlanItem {
        PlanItem {
            id: self.id,
            title: self.title,
            details: self.details,
            status: self.status.unwrap_or(PlanItemStatus::Todo),
            blocked_reason: None,
            commit: None,
            files: self.files,
            readonly_files: self.readonly_files,
            outer_docs: self.outer_docs,
            concept_refs: self.concept_refs,
            depends_on: self.depends_on,
            children: self
                .children
                .into_iter()
                .map(Self::into_plan_item)
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
struct PlanMutationTag {
    action: String,
    id: String,
    title: Option<String>,
    description: Option<String>,
    concept: Vec<tauqe_protocol::ConceptItem>,
    items: Vec<PlanMutationItem>,
}

impl PlanStorage {
    fn apply_tags_with_snapshot_unlocked(
        repo_root: &Path,
        snapshot: &mut PlanBatchSnapshot,
        parsed: Vec<PlanMutationTag>,
        errors: Vec<String>,
        mode: PlanMutationMode,
        commit: MutationCommit,
    ) -> PlanApplicationOutcome {
        if parsed.is_empty() && errors.is_empty() {
            return PlanApplicationOutcome::default();
        }

        let mut errors: Vec<PlanRejection> = errors
            .into_iter()
            .map(PlanRejection::payload_defect)
            .collect();
        let mut applied = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        for tag in parsed {
            let action = tag.action.to_lowercase();
            let mut tag_warnings: Vec<String> = Vec::new();

            match mode {
                PlanMutationMode::IsolatedStep => {
                    errors.push(PlanRejection::mode_forbidden(format!(
                        "Cannot perform plan action '{}' during isolated step execution: plan mutations are strictly prohibited during step execution",
                        action
                    )));
                    continue;
                }
                PlanMutationMode::Repl => {
                    if action != "save" {
                        errors.push(PlanRejection::mode_forbidden(format!(
                            "Cannot perform plan action '{}' in REPL mode: only 'save' is permitted (plan modifications and deletions belong exclusively in Discussion mode)",
                            action
                        )));
                        continue;
                    }
                    let already_exists =
                        crate::plan::matcher::resolve_plan_id_exact(&tag.id, snapshot.plans())
                            .is_some()
                            || applied
                                .iter()
                                .any(|p: &Plan| p.id.eq_ignore_ascii_case(&tag.id));
                    if already_exists {
                        errors.push(PlanRejection::mode_forbidden(format!(
                            "Cannot modify existing plan '{}' in REPL mode: modifying existing plans belongs exclusively in Discussion mode",
                            tag.id
                        )));
                        continue;
                    }
                }
                PlanMutationMode::Discussion => {}
            }

            match action.as_str() {
                "save" => {
                    if tag.concept.is_empty() {
                        errors.push(PlanRejection::payload_defect(format!(
                            "Cannot save plan '{}': plan must contain a concept tree defining architectural intent and invariants",
                            tag.id
                        )));
                        continue;
                    }
                    let matched_plan =
                        crate::plan::matcher::resolve_plan_id_exact(&tag.id, snapshot.plans())
                            .cloned();
                    let mut plan = matched_plan.unwrap_or_else(|| {
                        Plan::from_parts(
                            tag.id.clone(),
                            tag.title.clone().unwrap_or_else(|| tag.id.clone()),
                            tag.description.clone(),
                            now,
                            now,
                            Vec::new(),
                            Vec::new(),
                        )
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
                    let mut items: Vec<PlanItem> = tag
                        .items
                        .into_iter()
                        .map(PlanMutationItem::into_plan_item)
                        .collect();
                    let mut barrier = StatusBarrier::new(&plan.id, &mut tag_warnings);
                    for item in &mut items {
                        barrier.saved(&plan, item);
                    }
                    plan.implementation.items = items;
                    plan.implementation.refresh_parent_statuses();
                    plan.updated_at = now;
                    match commit.commit(repo_root, snapshot, &plan) {
                        Ok(stored) => {
                            if commit.persists() {
                                applied.push(stored);
                            }
                            warnings.append(&mut tag_warnings);
                        }
                        Err(err) => {
                            errors.push(PlanRejection::payload_defect(format!(
                                "Failed to save plan '{}': {:#}",
                                plan.id, err
                            )));
                        }
                    }
                }
                "update" => {
                    let matched_plan =
                        match crate::plan::matcher::resolve_plan(&tag.id, snapshot.plans(), None) {
                            crate::plan::matcher::PlanResolution::Found(p) => Some(p.clone()),
                            crate::plan::matcher::PlanResolution::Ambiguous(candidates) => {
                                let candidate_ids: Vec<String> =
                                    candidates.iter().map(|p| format!("'{}'", p.id)).collect();
                                errors.push(PlanRejection::payload_defect(format!(
                                    "Cannot update plan '{}': ambiguous match, candidates: {}",
                                    tag.id,
                                    candidate_ids.join(", ")
                                )));
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

                        fn update_single(
                            barrier: &mut StatusBarrier<'_>,
                            items: &mut [PlanItem],
                            update: &PlanMutationItem,
                        ) -> bool {
                            for item in items.iter_mut() {
                                if item.id.eq_ignore_ascii_case(&update.id) {
                                    if let Some(s) = update.status {
                                        item.status = barrier.existing(&item.id, item.status, s);
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
                                    if !update.outer_docs.is_empty() {
                                        item.outer_docs = update.outer_docs.clone();
                                    }
                                    if !update.concept_refs.is_empty() {
                                        item.concept_refs = update.concept_refs.clone();
                                    }
                                    if !update.depends_on.is_empty() {
                                        item.depends_on = update.depends_on.clone();
                                    }
                                    return true;
                                }
                                if update_single(barrier, &mut item.children, update) {
                                    return true;
                                }
                            }
                            false
                        }

                        let errors_before = errors.len();
                        let mut added_items: Vec<PlanItem> = Vec::new();
                        let mut barrier = StatusBarrier::new(&plan.id, &mut tag_warnings);
                        for update_item in &tag.items {
                            if update_single(
                                &mut barrier,
                                &mut plan.implementation.items,
                                update_item,
                            ) {
                                continue;
                            }
                            if !update_item.children.is_empty() {
                                errors.push(PlanRejection::payload_defect(format!(
                                    "Cannot add step '{}' to plan '{}': update may add leaf steps only, use save to restructure the plan",
                                    update_item.id, plan.id
                                )));
                                continue;
                            }
                            let mut added = update_item.clone().into_plan_item();
                            barrier.created(&mut added);
                            added_items.push(added);
                        }
                        plan.implementation.items.append(&mut added_items);

                        if errors.len() > errors_before {
                            continue;
                        }

                        plan.implementation.normalize_leaf_files();
                        plan.implementation.refresh_parent_statuses();

                        plan.updated_at = now;
                        match commit.commit(repo_root, snapshot, &plan) {
                            Ok(stored) => {
                                if commit.persists() {
                                    applied.push(stored);
                                }
                                warnings.append(&mut tag_warnings);
                            }
                            Err(err) => {
                                errors.push(PlanRejection::payload_defect(format!(
                                    "Failed to save updated plan '{}': {:#}",
                                    plan.id, err
                                )));
                            }
                        }
                    } else {
                        let available: Vec<String> =
                            snapshot.plans().iter().map(|p| p.id.clone()).collect();
                        let hint = if available.is_empty() {
                            "no plans exist in storage".to_string()
                        } else {
                            format!("available plans: {}", available.join(", "))
                        };
                        errors.push(PlanRejection::payload_defect(format!(
                            "Cannot update plan '{}': plan not found ({hint})",
                            tag.id
                        )));
                    }
                }
                other => {
                    errors.push(PlanRejection::payload_defect(format!(
                        "Unsupported plan action '{other}' (expected save or update)"
                    )));
                }
            }
        }

        for warning in &warnings {
            tracing::warn!("{}", warning);
        }
        PlanApplicationOutcome {
            applied,
            errors,
            warnings,
        }
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

    /// Applies a batch of structured discussion updates as one storage transaction.
    pub fn apply_discussion_plan_updates_with_mode(
        repo_root: &Path,
        updates: &[DiscussionPlanUpdate],
        mode: PlanMutationMode,
    ) -> Vec<PlanApplicationOutcome> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::apply_discussion_plan_updates_unlocked(repo_root, updates, mode)
        })
    }

    fn apply_discussion_plan_updates_unlocked(
        repo_root: &Path,
        updates: &[DiscussionPlanUpdate],
        mode: PlanMutationMode,
    ) -> Vec<PlanApplicationOutcome> {
        if updates.is_empty() {
            return Vec::new();
        }
        let mut snapshot = PlanBatchSnapshot::load_unlocked(repo_root).unwrap_or_default();
        let mut outcomes = Vec::with_capacity(updates.len());
        let mut last_applied: Option<Plan> = None;
        for update in updates {
            let resolved =
                Self::resolve_discussion_update_unlocked(repo_root, update, snapshot.plans());
            let mutation_tag = match resolved {
                Ok(tag) => tag,
                Err(rejection) => {
                    outcomes.push(PlanApplicationOutcome {
                        applied: Vec::new(),
                        errors: vec![rejection],
                        warnings: Vec::new(),
                    });
                    continue;
                }
            };
            let outcome = Self::apply_tags_with_snapshot_unlocked(
                repo_root,
                &mut snapshot,
                vec![mutation_tag],
                Vec::new(),
                mode,
                MutationCommit::Persist,
            );
            if let Some(plan) = outcome.applied.last() {
                last_applied = Some(plan.clone());
            }
            outcomes.push(outcome);
        }
        if let Some(plan) = last_applied {
            let _ = Self::save_active_id_unlocked(repo_root, Some(&plan.id));
        }
        outcomes
    }

    pub fn apply_discussion_plan_update_with_mode(
        repo_root: &Path,
        update: &DiscussionPlanUpdate,
        mode: PlanMutationMode,
    ) -> PlanApplicationOutcome {
        Self::apply_discussion_plan_updates_with_mode(repo_root, std::slice::from_ref(update), mode)
            .pop()
            .unwrap_or_default()
    }

    pub fn validate_discussion_plan_updates(
        repo_root: &Path,
        updates: &[DiscussionPlanUpdate],
        mode: PlanMutationMode,
    ) -> PlanApplicationOutcome {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::probe_discussion_plan_updates_unlocked(repo_root, updates, mode)
        })
    }

    fn probe_discussion_plan_updates_unlocked(
        repo_root: &Path,
        updates: &[DiscussionPlanUpdate],
        mode: PlanMutationMode,
    ) -> PlanApplicationOutcome {
        if updates.is_empty() {
            return PlanApplicationOutcome::default();
        }
        let mut snapshot = PlanBatchSnapshot::load_unlocked(repo_root).unwrap_or_default();
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        for update in updates {
            let mutation_tag =
                match Self::resolve_discussion_update_unlocked(repo_root, update, snapshot.plans())
                {
                    Ok(tag) => tag,
                    Err(rejection) => {
                        errors.push(rejection);
                        continue;
                    }
                };
            let probe = Self::apply_tags_with_snapshot_unlocked(
                repo_root,
                &mut snapshot,
                vec![mutation_tag],
                Vec::new(),
                mode,
                MutationCommit::Probe,
            );
            errors.extend(probe.errors);
            warnings.extend(probe.warnings);
        }
        PlanApplicationOutcome {
            applied: Vec::new(),
            errors,
            warnings,
        }
    }

    fn resolve_discussion_update_unlocked(
        repo_root: &Path,
        update: &DiscussionPlanUpdate,
        all_plans: &[Plan],
    ) -> std::result::Result<PlanMutationTag, PlanRejection> {
        let active_id = Self::load_active_id_unlocked(repo_root).ok().flatten();
        let plan_id = update
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(String::from)
            .or_else(|| {
                crate::plan::matcher::resolve_plan_id("", all_plans, active_id.as_deref())
                    .map(|plan| plan.id.clone())
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
            return Err(PlanRejection::payload_defect(format!(
                "Cannot save plan '{}': plan must contain a concept tree defining architectural intent and invariants",
                plan_id
            )));
        }

        Ok(PlanMutationTag {
            action,
            id: plan_id,
            title: update.title.clone(),
            description: update.description.clone(),
            concept: update.concept.clone(),
            items: update.items.clone().into_iter().map(Into::into).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::PlanRejectionKind;
    use tempfile::tempdir;

    fn probe_leaf(id: &str, file: &str) -> PlanItem {
        PlanItem {
            id: id.to_string(),
            title: format!("Step {id}"),
            status: PlanItemStatus::Todo,
            files: vec![file.to_string()],
            ..Default::default()
        }
    }

    fn probe_payload(id: &str, items: Vec<PlanItem>) -> DiscussionPlanUpdate {
        DiscussionPlanUpdate {
            action: DiscussionPlanAction::Save,
            id: Some(id.to_string()),
            title: Some(format!("Plan {id}")),
            description: None,
            concept: vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Invariant".to_string(),
                details: None,
                children: Vec::new(),
            }],
            items,
        }
    }

    fn rejection_text(rejections: &[PlanRejection]) -> String {
        rejections
            .iter()
            .map(|rejection| rejection.message.as_str())
            .collect::<Vec<_>>()
            .join("\n")
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
                blocked_reason: None,
                commit: None,
                files: vec!["src/protocol_types.rs".to_string()],
                readonly_files: vec![],
                outer_docs: vec![],
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
                blocked_reason: None,
                commit: None,
                files: vec!["src/protocol_types.rs".to_string()],
                readonly_files: vec![],
                outer_docs: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        };

        let outcome2 = PlanStorage::apply_discussion_plan_update(root, &update2);
        assert!(outcome2.errors.is_empty());
        assert_eq!(
            outcome2.applied[0].implementation.items[0]
                .details
                .as_deref(),
            Some("Updated details")
        );
    }

    #[test]
    fn test_save_plan_clears_items_when_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let plan = Plan::from_parts(
            "test-plan".to_string(),
            "Test".to_string(),
            Some("Desc".to_string()),
            100,
            100,
            vec![],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                files: vec!["src/step1.rs".to_string()],
                ..Default::default()
            }],
        );
        PlanStorage::save_plan(root, &plan).unwrap();
        assert_eq!(
            PlanStorage::load_plan(root, "test-plan")
                .unwrap()
                .unwrap()
                .implementation
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
        assert_eq!(outcome.applied[0].implementation.items.len(), 0);

        let reloaded = PlanStorage::load_plan(root, "test-plan").unwrap().unwrap();
        assert_eq!(reloaded.implementation.items.len(), 0);
        assert_eq!(
            reloaded.description.as_deref(),
            Some("Updated Desc without items")
        );
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
                files: vec!["src/step1.rs".to_string()],
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
                files: vec!["src/step2.rs".to_string()],
                ..Default::default()
            }],
        };
        let applied = PlanStorage::apply_discussion_plan_update(root, &update);
        assert!(applied.errors.is_empty(), "Errors: {:?}", applied.errors);
        assert_eq!(applied.applied.len(), 1);
        assert_eq!(applied.applied[0].implementation.items.len(), 2);

        let reloaded = PlanStorage::load_plan(root, "structured-upsert")
            .unwrap()
            .unwrap();
        assert!(reloaded.implementation.find_item("2").is_some());
    }

    #[test]
    fn test_probe_accepts_a_valid_payload_without_writing() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let update = probe_payload("probe-valid", vec![probe_leaf("1", "src/step1.rs")]);

        let probe = PlanStorage::validate_discussion_plan_updates(
            root,
            std::slice::from_ref(&update),
            PlanMutationMode::Discussion,
        );
        assert!(probe.errors.is_empty(), "Errors: {:?}", probe.errors);
        assert!(probe.warnings.is_empty(), "Warnings: {:?}", probe.warnings);
        assert!(probe.applied.is_empty(), "a probe applies nothing");
        assert!(PlanStorage::load_plan(root, "probe-valid")
            .unwrap()
            .is_none());
        assert!(PlanStorage::load_active_id(root).unwrap().is_none());

        let applied = PlanStorage::apply_discussion_plan_update(root, &update);
        assert!(applied.errors.is_empty(), "Errors: {:?}", applied.errors);
        assert_eq!(applied.applied.len(), 1);
        assert!(applied.warnings.is_empty());
        assert!(PlanStorage::load_plan(root, "probe-valid")
            .unwrap()
            .is_some());
    }

    #[test]
    fn test_probe_reaches_the_applied_verdict() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let duplicate = probe_payload(
            "probe-duplicate",
            vec![
                probe_leaf("1", "src/first.rs"),
                probe_leaf("1", "src/second.rs"),
            ],
        );
        let mut cyclic_first = probe_leaf("a", "src/a.rs");
        cyclic_first.depends_on = vec!["b".to_string()];
        let mut cyclic_second = probe_leaf("b", "src/b.rs");
        cyclic_second.depends_on = vec!["a".to_string()];
        let cyclic = probe_payload("probe-cycle", vec![cyclic_first, cyclic_second]);

        for (update, plan_id) in [(&duplicate, "probe-duplicate"), (&cyclic, "probe-cycle")] {
            let probe = PlanStorage::validate_discussion_plan_updates(
                root,
                std::slice::from_ref(update),
                PlanMutationMode::Discussion,
            );
            assert!(probe.applied.is_empty(), "a probe applies nothing");
            assert_eq!(probe.errors.len(), 1, "{:?}", probe.errors);

            let applied = PlanStorage::apply_discussion_plan_update(root, update);
            assert!(applied.applied.is_empty());
            assert_eq!(
                rejection_text(&probe.errors),
                rejection_text(&applied.errors),
                "the probe must reach the verdict the application reaches"
            );
            assert!(PlanStorage::load_plan(root, plan_id).unwrap().is_none());
        }

        let valid = probe_payload("probe-forbidden", vec![probe_leaf("1", "src/step1.rs")]);
        let probe_forbidden = PlanStorage::validate_discussion_plan_updates(
            root,
            std::slice::from_ref(&valid),
            PlanMutationMode::IsolatedStep,
        );
        assert_eq!(probe_forbidden.errors.len(), 1);
        assert_eq!(
            probe_forbidden.errors[0].kind,
            PlanRejectionKind::ModeForbidden
        );
        let applied_forbidden = PlanStorage::apply_discussion_plan_update_with_mode(
            root,
            &valid,
            PlanMutationMode::IsolatedStep,
        );
        assert_eq!(
            rejection_text(&probe_forbidden.errors),
            rejection_text(&applied_forbidden.errors)
        );
        assert!(PlanStorage::load_plan(root, "probe-forbidden")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_probe_advances_its_own_view_across_a_batch() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let save = probe_payload("probe-batch", vec![probe_leaf("1", "src/step1.rs")]);
        let rename = DiscussionPlanUpdate {
            action: DiscussionPlanAction::Update,
            id: Some("probe-batch".to_string()),
            title: None,
            description: None,
            concept: Vec::new(),
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Renamed step".to_string(),
                ..Default::default()
            }],
        };

        let probe = PlanStorage::validate_discussion_plan_updates(
            root,
            &[save.clone(), rename.clone()],
            PlanMutationMode::Discussion,
        );
        assert!(probe.errors.is_empty(), "Errors: {:?}", probe.errors);
        assert!(probe.applied.is_empty(), "a probe applies nothing");
        assert!(PlanStorage::load_plan(root, "probe-batch")
            .unwrap()
            .is_none());

        let seeded = PlanStorage::apply_discussion_plan_update(root, &save);
        assert!(seeded.errors.is_empty(), "Errors: {:?}", seeded.errors);
        let renamed = PlanStorage::apply_discussion_plan_update(root, &rename);
        assert!(renamed.errors.is_empty(), "Errors: {:?}", renamed.errors);

        let stored = PlanStorage::load_plan(root, "probe-batch")
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.implementation.find_item("1").unwrap().title,
            "Renamed step"
        );
    }

    #[test]
    fn test_batch_entry_point_settles_a_payload_as_one_transaction() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let save_a = probe_payload("batch-a", vec![probe_leaf("1", "src/a1.rs")]);
        let rename_a = DiscussionPlanUpdate {
            action: DiscussionPlanAction::Update,
            id: Some("batch-a".to_string()),
            title: None,
            description: None,
            concept: Vec::new(),
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Renamed step".to_string(),
                ..Default::default()
            }],
        };
        let save_b = probe_payload("batch-b", vec![probe_leaf("1", "src/b1.rs")]);

        let outcomes = PlanStorage::apply_discussion_plan_updates_with_mode(
            root,
            &[save_a, rename_a, save_b],
            PlanMutationMode::Discussion,
        );
        assert_eq!(outcomes.len(), 3);
        for outcome in &outcomes {
            assert!(outcome.errors.is_empty(), "Errors: {:?}", outcome.errors);
            assert_eq!(outcome.applied.len(), 1);
        }

        let stored = PlanStorage::load_plan(root, "batch-a").unwrap().unwrap();
        assert_eq!(
            stored.implementation.find_item("1").unwrap().title,
            "Renamed step"
        );
        assert!(PlanStorage::load_plan(root, "batch-b").unwrap().is_some());
        assert_eq!(
            PlanStorage::load_active_id(root).unwrap(),
            Some("batch-b".to_string())
        );
    }

    #[test]
    fn test_update_patches_outer_docs_of_an_established_step() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let seeded = DiscussionPlanUpdate {
            action: DiscussionPlanAction::Save,
            id: Some("outer-docs-plan".to_string()),
            title: Some("Outer Docs Plan".to_string()),
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
                files: vec!["src/step1.rs".to_string()],
                ..Default::default()
            }],
        };
        let applied = PlanStorage::apply_discussion_plan_update(root, &seeded);
        assert!(applied.errors.is_empty(), "Errors: {:?}", applied.errors);

        let update = DiscussionPlanUpdate {
            action: DiscussionPlanAction::Update,
            id: Some("outer-docs-plan".to_string()),
            title: None,
            description: None,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                outer_docs: vec!["protocol-spec".to_string(), "wire-format".to_string()],
                ..Default::default()
            }],
        };
        let outcome = PlanStorage::apply_discussion_plan_update(root, &update);
        assert!(outcome.errors.is_empty(), "Errors: {:?}", outcome.errors);
        assert_eq!(outcome.applied.len(), 1);

        let stored = PlanStorage::load_plan(root, "outer-docs-plan")
            .unwrap()
            .unwrap();
        let step = stored
            .implementation
            .find_item("1")
            .expect("step 1 must exist");
        assert_eq!(step.outer_docs, vec!["protocol-spec", "wire-format"]);
        assert_eq!(step.files, vec!["src/step1.rs"]);
    }
}
