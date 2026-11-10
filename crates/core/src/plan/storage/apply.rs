use crate::edits::protocol::xml::tags::{ParsedPlanItem, ParsedPlanTag};
use std::path::Path;
use std::time::SystemTime;
use tauqe_protocol::{
    DiscussionPlanAction, DiscussionPlanUpdate, Plan, PlanItem, PlanItemStatus, PlanRejection,
};

use super::{PlanApplicationOutcome, PlanBatchSnapshot, PlanMutationMode, PlanStorage};

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

/// Contract of one applied mutation of a plan batch.
///
/// The two contracts differ only in where the result of the shared barrier goes:
/// [`MutationCommit::Persist`] writes the validated document, while
/// [`MutationCommit::Probe`] keeps it in the snapshot of the batch and discards
/// it. Because both run the very same preparation and validation against that
/// snapshot, the verdict of a probe is the verdict of the application it
/// predicts.
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
    ///
    /// A persisted plan also becomes the active plan when the repository has none,
    /// so a client always has a marker to render; the probe writes neither the
    /// document nor the marker. Both advance the snapshot, because every later
    /// mutation of the batch must resolve against the plan this one produced.
    fn commit(
        self,
        repo_root: &Path,
        snapshot: &mut PlanBatchSnapshot,
        plan: &Plan,
    ) -> anyhow::Result<Plan> {
        match self {
            Self::Persist => PlanStorage::save_plan_in_snapshot_unlocked(repo_root, snapshot, plan),
            Self::Probe => {
                // The barrier runs and nothing reaches the disk, so the validated
                // document replaces the stored one in the snapshot alone.
                let prepared = PlanStorage::prepare_plan_unlocked(repo_root, plan, Some(snapshot))?;
                snapshot.remember(prepared.plan.clone(), prepared.target_file);
                Ok(prepared.plan)
            }
        }
    }

    /// Removes one plan under this contract and reports whether it was removed.
    ///
    /// The probe decides over the snapshot, which was read from storage under the
    /// same lock, so it reaches the verdict the removal reaches without unlinking
    /// a file.
    fn delete(
        self,
        repo_root: &Path,
        snapshot: &mut PlanBatchSnapshot,
        id: &str,
    ) -> anyhow::Result<bool> {
        match self {
            Self::Persist => PlanStorage::delete_plan_in_snapshot_unlocked(repo_root, snapshot, id),
            Self::Probe => Ok(snapshot.remove(id).is_some()),
        }
    }
}

/// Reports whether a model mutation may create a step in `status`.
///
/// A step enters existence as a draft (`Discussion`) or as pending work (`Todo`).
/// `InProgress`, `Blocked`, and `Cancelled` belong to the execution pipeline and
/// to the developer's manual control, while `Done` is protected by the
/// immutability guard, so none of those statuses can be asserted while a step is
/// authored.
fn is_creatable_status(status: PlanItemStatus) -> bool {
    matches!(status, PlanItemStatus::Todo | PlanItemStatus::Discussion)
}

/// Status lifecycle barrier of one plan mutation.
///
/// The model authors plans, but it never runs them: a step is created only as
/// `Todo` or `Discussion`, and a mutation never starts an established step. The
/// barrier repairs a payload that asserts a status the mutation cannot assign
/// instead of rejecting the whole payload, and records every repair once, in
/// mutation order, so a caller can surface what was changed.
struct StatusBarrier<'a> {
    /// Identifier of the targeted plan, quoted in every repair report.
    plan_id: String,
    /// Repairs applied while the payload was normalized.
    warnings: &'a mut Vec<String>,
}

impl<'a> StatusBarrier<'a> {
    /// Creates the barrier of a mutation targeting `plan_id`.
    fn new(plan_id: &str, warnings: &'a mut Vec<String>) -> Self {
        Self {
            plan_id: plan_id.to_string(),
            warnings,
        }
    }

    /// Resolves the status an established step carries after the mutation.
    ///
    /// `Done` immutability is enforced separately by the storage barrier, so this
    /// method owns the one transition a model may never perform: starting a step.
    /// An assignment that repeats the stored status is not a transition at all,
    /// so it passes untouched.
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

    /// Repairs the status of a step the mutation creates.
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

    /// Applies the barrier to every node a `save` writes against `stored`.
    ///
    /// The stored plan separates the two cases: a node whose identifier it
    /// already carries is an established step, while every other node —
    /// including the children of a container the save introduces — is being
    /// created.
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
        errors: Vec<String>,
        replacement: SaveItemsReplacement,
        mode: PlanMutationMode,
    ) -> PlanApplicationOutcome {
        let mut snapshot = PlanBatchSnapshot::load_unlocked(repo_root).unwrap_or_default();
        Self::apply_tags_with_snapshot_unlocked(
            repo_root,
            &mut snapshot,
            parsed,
            errors,
            replacement,
            mode,
            MutationCommit::Persist,
        )
    }

    /// Applies plan tags against the snapshot of the batch under an explicit
    /// commit contract.
    ///
    /// The snapshot is the single repository read of the batch. Both contracts
    /// advance it in memory — the persist contract with the document it wrote and
    /// the probe with the document it validated — so a later tag of the same batch
    /// resolves against the plan the batch itself produced without re-reading a
    /// stored document, and the verdict of the probe stays the verdict of the
    /// application.
    fn apply_tags_with_snapshot_unlocked(
        repo_root: &Path,
        snapshot: &mut PlanBatchSnapshot,
        parsed: Vec<ParsedPlanTag>,
        errors: Vec<String>,
        replacement: SaveItemsReplacement,
        mode: PlanMutationMode,
        commit: MutationCommit,
    ) -> PlanApplicationOutcome {
        if parsed.is_empty() && errors.is_empty() {
            return PlanApplicationOutcome::default();
        }

        // A diagnostic the parser already reported describes a payload the
        // planning contract refused, so the batch enters typed: every entry is
        // classified here, where the refusal was produced, and no consumer ever
        // derives a category from the wording of a reason.
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

        // The batch resolves every tag against the view it was handed and
        // advances it after each applied mutation. One turn accumulates plan
        // payloads from several rounds and settles them as a single batch, so an
        // update of a plan that an earlier tag of the same batch saved must be
        // resolved against the plan the batch itself produced.

        for tag in parsed {
            let action = tag.action.to_lowercase();
            // Repairs are staged per mutation: a rejected mutation created nothing,
            // so its notes are discarded together with it.
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
                            "Cannot save plan '{}': plan must contain a <concept> section defining architectural intent and invariants",
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
                        .map(ParsedPlanItem::into_plan_item)
                        .collect();
                    // The stored plan is the reference of the barrier: it separates
                    // an established step from one this save creates.
                    let mut barrier = StatusBarrier::new(&plan.id, &mut tag_warnings);
                    for item in &mut items {
                        barrier.saved(&plan, item);
                    }
                    match replacement {
                        // An explicit replacement always wins, even when it clears
                        // the stored steps.
                        SaveItemsReplacement::Replace => plan.implementation.items = items,
                        // A guarded save must never erase established work: the
                        // incoming items replace the plan only when they are
                        // non-empty.
                        SaveItemsReplacement::Guarded => {
                            if !items.is_empty() {
                                plan.implementation.items = items;
                            }
                        }
                    }
                    plan.implementation.refresh_parent_statuses();
                    plan.updated_at = now;
                    match commit.commit(repo_root, snapshot, &plan) {
                        Ok(stored) => {
                            // A probe applies nothing, so only a persisted batch
                            // reports the plan as its applied mutation.
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

                        fn update_single(
                            barrier: &mut StatusBarrier<'_>,
                            items: &mut [PlanItem],
                            update: &ParsedPlanItem,
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
                                    // A leaf names the external documents its implementation
                                    // obeys, so a delta update binds them the same way it
                                    // binds `files` and `readonly_files`.
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

                        // A delta `update` mirrors the concept-node upsert: an
                        // identifier that is not found is appended to the plan root
                        // instead of failing the whole mutation, so one `update`
                        // both patches established steps and adds new ones.
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
                            // Only executable leaves are added this way: creating or
                            // moving containers restructures the plan and stays the
                            // sole province of `save`.
                            if !update_item.children.is_empty() {
                                errors.push(PlanRejection::payload_defect(format!(
                                    "Cannot add step '{}' to plan '{}': update may add leaf steps only, use save to restructure the plan",
                                    update_item.id, plan.id
                                )));
                                continue;
                            }
                            // A step the stored plan does not contain is being
                            // created by this mutation, so its status is repaired
                            // to the only states a created step may carry.
                            let mut added = update_item.clone().into_plan_item();
                            barrier.created(&mut added);
                            added_items.push(added);
                        }
                        plan.implementation.items.append(&mut added_items);

                        // A rejected entry leaves the stored plan untouched, so the
                        // caller repairs the payload instead of persisting a partial
                        // merge.
                        if errors.len() > errors_before {
                            continue;
                        }

                        plan.implementation.normalize_leaf_files();
                        plan.implementation.refresh_parent_statuses();
                        // The structural verdict belongs to the storage barrier:
                        // `save_plan_unlocked` validates the whole plan before a
                        // byte reaches disk, so this branch never restates the
                        // rules the single write path already enforces.

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
                "delete" => {
                    // The view carries the plans this batch already saved, so an
                    // id introduced moments ago resolves to its own stored artifact
                    // instead of addressing a stale alias.
                    let target_id =
                        crate::plan::matcher::resolve_plan_id_exact(&tag.id, snapshot.plans())
                            .map(|p| p.id.clone())
                            .unwrap_or_else(|| tag.id.clone());
                    match commit.delete(repo_root, snapshot, &target_id) {
                        Ok(true) => {}
                        Ok(false) => {
                            errors.push(PlanRejection::payload_defect(format!(
                                "Cannot delete plan '{}': plan not found",
                                tag.id
                            )));
                        }
                        Err(err) => {
                            errors.push(PlanRejection::payload_defect(format!(
                                "Failed to delete plan '{}': {:#}",
                                tag.id, err
                            )));
                        }
                    }
                }
                other => {
                    errors.push(PlanRejection::payload_defect(format!(
                        "Unsupported plan action '{other}' (expected save, update, or delete)"
                    )));
                }
            }
        }

        // Every repair is reported twice: in the outcome, so a caller can surface
        // it, and in the journal, because a normalized status is a real event in
        // the lifecycle of the plan rather than a silent rewrite.
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
    ///
    /// The whole batch runs under one lock with one repository read: every update
    /// resolves against the plan the batch already produced, and the snapshot
    /// advances in memory instead of being re-read from disk after each mutation.
    /// A turn carrying `k` mutations over `n` stored plans therefore reads each
    /// stored plan once instead of `O(k·n)` times.
    ///
    /// The outcomes are aligned with `updates`, so one mutation reports exactly the
    /// refusals, the repairs, and the document the single-update entry point
    /// reports for it.
    pub fn apply_discussion_plan_updates_with_mode(
        repo_root: &Path,
        updates: &[DiscussionPlanUpdate],
        mode: PlanMutationMode,
    ) -> Vec<PlanApplicationOutcome> {
        // The lock wait, the repository read, and every atomic write of the batch
        // run off the async executor thread, so a slow disk never stalls the
        // bounded turn, provider streaming, or RPC handling.
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::apply_discussion_plan_updates_unlocked(repo_root, updates, mode)
        })
    }

    /// Lock-free core of [`Self::apply_discussion_plan_updates_with_mode`]; the
    /// caller must hold the storage lock.
    fn apply_discussion_plan_updates_unlocked(
        repo_root: &Path,
        updates: &[DiscussionPlanUpdate],
        mode: PlanMutationMode,
    ) -> Vec<PlanApplicationOutcome> {
        if updates.is_empty() {
            return Vec::new();
        }
        // One repository read serves the whole batch: the snapshot records the
        // document and the file of every stored plan, and it advances in memory
        // because every mutation writes through it under the same lock.
        let mut snapshot = PlanBatchSnapshot::load_unlocked(repo_root).unwrap_or_default();
        let mut outcomes = Vec::with_capacity(updates.len());
        let mut last_applied: Option<Plan> = None;
        for update in updates {
            // The identity decision is shared with the probe, so a refusal of the
            // resolution itself keeps the classification it was born with.
            let resolved =
                Self::resolve_discussion_update_unlocked(repo_root, update, snapshot.plans());
            let parsed_tag = match resolved {
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
            // The structured discussion command is the explicit architect path: its
            // typed, deliberate item list (including an explicit empty list) fully
            // replaces the stored plan.
            let outcome = Self::apply_tags_with_snapshot_unlocked(
                repo_root,
                &mut snapshot,
                vec![parsed_tag],
                Vec::new(),
                SaveItemsReplacement::Replace,
                mode,
                MutationCommit::Persist,
            );
            if let Some(plan) = outcome.applied.last() {
                last_applied = Some(plan.clone());
            }
            outcomes.push(outcome);
        }
        // The last applied mutation leaves its plan active, so a batch settles the
        // active-plan marker exactly as applying its mutations one after another
        // did.
        if let Some(plan) = last_applied {
            let _ = Self::save_active_id_unlocked(repo_root, Some(&plan.id));
        }
        outcomes
    }

    /// Applies a structured `DiscussionPlanUpdate` directly to local plan storage
    /// under a specific [`PlanMutationMode`].
    pub fn apply_discussion_plan_update_with_mode(
        repo_root: &Path,
        update: &DiscussionPlanUpdate,
        mode: PlanMutationMode,
    ) -> PlanApplicationOutcome {
        Self::apply_discussion_plan_updates_with_mode(repo_root, std::slice::from_ref(update), mode)
            .pop()
            .unwrap_or_default()
    }

    /// Validates a batch of structured discussion updates without persisting anything.
    ///
    /// The probe runs the very mutation cycle the application runs — identity
    /// resolution, the status barrier, and the structural barrier of the write path
    /// — against an in-memory snapshot of the stored plans, under the same storage
    /// lock and off the async executor thread. Nothing reaches the disk, so the
    /// returned outcome carries the refusals and the repairs of the batch with an
    /// empty `applied` list, and its verdict is the verdict the later application of
    /// the same batch reaches.
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

    /// Lock-free core of [`Self::validate_discussion_plan_updates`]; the caller must
    /// hold the storage lock.
    fn probe_discussion_plan_updates_unlocked(
        repo_root: &Path,
        updates: &[DiscussionPlanUpdate],
        mode: PlanMutationMode,
    ) -> PlanApplicationOutcome {
        if updates.is_empty() {
            return PlanApplicationOutcome::default();
        }
        // The probe advances the snapshot itself, because nothing is written: a
        // later mutation of the batch resolves against the plan the probe already
        // validated, exactly as the application resolves it against the plan it
        // stored.
        let mut snapshot = PlanBatchSnapshot::load_unlocked(repo_root).unwrap_or_default();
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        for update in updates {
            // The identity decision is shared with the application, so a refusal of
            // the resolution itself keeps the classification it was born with.
            let parsed_tag =
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
                vec![parsed_tag],
                Vec::new(),
                SaveItemsReplacement::Replace,
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

    /// Resolves the plan identity and the effective action a structured discussion
    /// update names.
    ///
    /// The resolution is shared by the application and the probe, so both observe
    /// one identity decision under one lock: a concurrent task cannot create or swap
    /// the active plan between a verdict and the write that follows it. The refusal
    /// of a `save` that carries no concept tree belongs here as well, because it is
    /// decided before any mutation is applied.
    fn resolve_discussion_update_unlocked(
        repo_root: &Path,
        update: &DiscussionPlanUpdate,
        all_plans: &[Plan],
    ) -> std::result::Result<ParsedPlanTag, PlanRejection> {
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

        // A first discussion of an empty repository creates the plan it names: an
        // update of a plan that does not exist yet is the creation of it.
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

        Ok(ParsedPlanTag {
            action,
            id: plan_id,
            title: update.title.clone(),
            description: update.description.clone(),
            concept: update.concept.clone(),
            items: update.items.clone().into_iter().map(Into::into).collect(),
        })
    }

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
    use tauqe_protocol::{PlanItemStatus, PlanRejectionKind};
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
  <item id="1" title="Model" status="todo" files="src/model.rs" />
  <item id="2" title="Handler" status="todo" files="src/handler.rs" />
</plan>
"#;

        let affected = PlanStorage::apply_plan_tags(root, model_text);
        assert_eq!(affected.applied.len(), 1);
        assert!(affected.errors.is_empty());
        assert_eq!(affected.applied[0].id, "jwt-auth");
        assert_eq!(affected.applied[0].concept.len(), 1);
        assert_eq!(affected.applied[0].implementation.items.len(), 2);
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
            affected_update.applied[0].implementation.items[0].status,
            PlanItemStatus::Done
        );
        assert_eq!(
            affected_update.applied[0].implementation.items[1].status,
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
        assert_eq!(
            res.errors[0].kind,
            PlanRejectionKind::PayloadDefect,
            "a parser diagnostic describes a defect of the payload"
        );
        assert!(res.errors[0].message.contains("missing closing tag"));

        // 2. Non-existent plan on update
        let update_missing =
            "<plan action=\"update\" id=\"non-existent\"><item id=\"1\" status=\"done\" /></plan>";
        let res_missing = PlanStorage::apply_plan_tags(root, update_missing);
        assert!(res_missing.applied.is_empty());
        assert_eq!(res_missing.errors.len(), 1);
        assert_eq!(res_missing.errors[0].kind, PlanRejectionKind::PayloadDefect);
        assert!(res_missing.errors[0]
            .message
            .contains("Cannot update plan 'non-existent'"));

        // 3. Non-existent item on update
        let valid_plan = Plan::from_parts(
            "my-plan".to_string(),
            "P".to_string(),
            None,
            1,
            1,
            vec![],
            vec![PlanItem {
                id: "1".to_string(),
                title: "T".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                blocked_reason: None,
                commit: None,
                files: vec!["src/t.rs".to_string()],
                readonly_files: vec![],
                outer_docs: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        );
        PlanStorage::save_plan(root, &valid_plan).unwrap();

        let update_item_missing =
            "<plan action=\"update\" id=\"my-plan\"><item id=\"99\" status=\"done\" files=\"src/extra.rs\" /></plan>";
        let res_item = PlanStorage::apply_plan_tags(root, update_item_missing);
        assert_eq!(res_item.applied.len(), 1);
        assert!(res_item.errors.is_empty(), "Errors: {:?}", res_item.errors);
        let reloaded = PlanStorage::load_plan(root, "my-plan").unwrap().unwrap();
        let added = reloaded
            .implementation
            .find_item("99")
            .expect("an unknown step must be added by update");
        // The step is created by the mutation, so the execution status it asked
        // for is repaired to the only creatable state and reported.
        assert_eq!(added.status, PlanItemStatus::Todo);
        assert_eq!(res_item.warnings.len(), 1, "{:?}", res_item.warnings);
        assert!(res_item.warnings[0].contains("new step '99'"));
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
  <item id="1" title="Step 1" status="in_progress" files="src/step1.rs" />
</plan>
"#;
        let seeded = PlanStorage::apply_plan_tags(root, initial);
        assert!(seeded.errors.is_empty(), "Errors: {:?}", seeded.errors);
        assert_eq!(seeded.applied.len(), 1);
        // A step enters existence as TODO: the pipeline, not the model, starts it.
        assert_eq!(
            seeded.applied[0]
                .implementation
                .find_item("1")
                .unwrap()
                .status,
            PlanItemStatus::Todo
        );
        PlanStorage::update_item(
            root,
            "upsert-plan",
            "1",
            Some(PlanItemStatus::InProgress),
            None,
            None,
        )
        .unwrap()
        .expect("the seeded step must exist");

        let update = r#"
<plan action="update" id="upsert-plan">
  <item id="1" title="Step 1 refined" concept="c-1" />
  <item id="2" title="Step 2" status="todo" deps="1" files="src/step2.rs" />
</plan>
"#;
        let applied = PlanStorage::apply_plan_tags(root, update);
        assert!(applied.errors.is_empty(), "Errors: {:?}", applied.errors);
        assert_eq!(applied.applied.len(), 1);

        let reloaded = PlanStorage::load_plan(root, "upsert-plan")
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.implementation.items.len(), 2);

        let step1 = reloaded
            .implementation
            .find_item("1")
            .expect("step 1 must exist");
        assert_eq!(step1.title, "Step 1 refined");
        assert_eq!(step1.concept_refs, vec!["c-1"]);
        // A partial patch keeps the status the step already carried.
        assert_eq!(step1.status, PlanItemStatus::InProgress);

        let step2 = reloaded
            .implementation
            .find_item("2")
            .expect("step 2 must be added");
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
    fn test_update_rejects_invalid_new_steps() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let initial = r#"
<plan action="save" id="validation-plan" title="Validation">
  <concept>
    <concept_item id="c-1" title="Invariant" />
  </concept>
  <item id="1" title="Step 1" status="todo" files="src/step1.rs" />
</plan>
"#;
        let seeded = PlanStorage::apply_plan_tags(root, initial);
        assert!(seeded.errors.is_empty(), "Errors: {:?}", seeded.errors);

        // Duplicate identifiers inside the added batch.
        let duplicates = r#"
<plan action="update" id="validation-plan">
  <item id="2" title="First" status="todo" files="src/first.rs" />
  <item id="2" title="Second" status="todo" files="src/second.rs" />
</plan>
"#;
        let dup_res = PlanStorage::apply_plan_tags(root, duplicates);
        assert!(dup_res.applied.is_empty());
        assert_eq!(dup_res.errors.len(), 1);
        assert_eq!(dup_res.errors[0].kind, PlanRejectionKind::PayloadDefect);
        assert!(dup_res.errors[0]
            .message
            .contains("Cannot save plan 'validation-plan'"));
        assert!(dup_res.errors[0].message.contains("Duplicate step ID: '2'"));

        // A new step naming a dependency that does not exist.
        let dangling = r#"
<plan action="update" id="validation-plan">
  <item id="3" title="Step 3" status="todo" deps="99" files="src/step3.rs" />
</plan>
"#;
        let dangling_res = PlanStorage::apply_plan_tags(root, dangling);
        assert!(dangling_res.applied.is_empty());
        assert_eq!(dangling_res.errors.len(), 1);
        assert_eq!(
            dangling_res.errors[0].kind,
            PlanRejectionKind::PayloadDefect
        );
        assert!(dangling_res.errors[0]
            .message
            .contains("Cannot save plan 'validation-plan'"));
        assert!(dangling_res.errors[0]
            .message
            .contains("non-existent dependency '99'"));

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
        assert_eq!(
            container_res.errors[0].kind,
            PlanRejectionKind::PayloadDefect
        );
        assert!(container_res.errors[0]
            .message
            .contains("update may add leaf steps only"));

        // Nothing reached disk.
        let reloaded = PlanStorage::load_plan(root, "validation-plan")
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.implementation.items.len(), 1);
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
    fn test_update_plan_ambiguous_reports_error_without_mutating() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let p1 = Plan::from_parts(
            "feature-alpha".to_string(),
            "Feature Alpha".to_string(),
            None,
            0,
            0,
            vec![],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                status: PlanItemStatus::Todo,
                files: vec!["src/step-alpha.rs".to_string()],
                ..Default::default()
            }],
        );
        let p2 = Plan::from_parts(
            "feature-beta".to_string(),
            "Feature Beta".to_string(),
            None,
            0,
            0,
            vec![],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                status: PlanItemStatus::Todo,
                files: vec!["src/step-beta.rs".to_string()],
                ..Default::default()
            }],
        );
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
        assert_eq!(outcome.errors[0].kind, PlanRejectionKind::PayloadDefect);
        assert!(outcome.errors[0].message.contains("ambiguous match"));
        assert!(outcome.errors[0].message.contains("'feature-alpha'"));
        assert!(outcome.errors[0].message.contains("'feature-beta'"));

        // Neither plan was modified
        let p1_reloaded = PlanStorage::load_plan(root, "feature-alpha")
            .unwrap()
            .unwrap();
        let p2_reloaded = PlanStorage::load_plan(root, "feature-beta")
            .unwrap()
            .unwrap();
        assert_eq!(
            p1_reloaded.implementation.items[0].status,
            PlanItemStatus::Todo
        );
        assert_eq!(
            p2_reloaded.implementation.items[0].status,
            PlanItemStatus::Todo
        );
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
  <item id="1" title="Step 1" status="in_progress" files="src/step1.rs" />
  <item id="2" title="Step 2" status="todo" files="src/step2.rs" />
</plan>
"#;
        let seeded = PlanStorage::apply_plan_tags(root, initial);
        assert!(seeded.errors.is_empty(), "Errors: {:?}", seeded.errors);
        assert_eq!(seeded.applied.len(), 1);
        assert_eq!(seeded.applied[0].implementation.items.len(), 2);
        // Steps are authored as TODO, so the IN_PROGRESS status this fixture needs
        // is assigned through the pipeline before the save is refreshed.
        PlanStorage::update_item(
            root,
            "guarded-save",
            "1",
            Some(PlanItemStatus::InProgress),
            None,
            None,
        )
        .unwrap()
        .expect("the seeded step must exist");

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
        assert_eq!(refreshed.applied[0].implementation.items.len(), 2);
        assert_eq!(
            refreshed.applied[0].implementation.items[0].status,
            PlanItemStatus::InProgress
        );

        let reloaded = PlanStorage::load_plan(root, "guarded-save")
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.implementation.items.len(), 2);
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
  <item id="1" title="Step 1" status="todo" files="src/step1.rs" />
  <item id="2" title="Step 2" status="todo" files="src/step2.rs" />
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
        assert_eq!(applied.applied[0].implementation.items.len(), 1);
        assert_eq!(
            applied.applied[0].implementation.items[0].status,
            PlanItemStatus::Done
        );
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
  <item id="1" title="Step 1" status="todo" files="src/step1.rs" />
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
        assert_eq!(
            res_isolated.errors[0].kind,
            PlanRejectionKind::ModeForbidden
        );
        assert!(res_isolated.errors[0]
            .message
            .contains("isolated step execution"));

        // 2. REPL mode permits saving a new plan
        let res_repl_save =
            PlanStorage::apply_plan_tags_with_mode(root, initial_plan, PlanMutationMode::Repl);
        assert_eq!(res_repl_save.applied.len(), 1);
        assert!(res_repl_save.errors.is_empty());

        // 3. REPL mode forbids updating an existing plan
        let update_text = r#"
<plan action="update" id="plan-mode-test">
  <item id="1" status="done" />
</plan>
"#;
        let res_repl_update =
            PlanStorage::apply_plan_tags_with_mode(root, update_text, PlanMutationMode::Repl);
        assert!(res_repl_update.applied.is_empty());
        assert_eq!(res_repl_update.errors.len(), 1);
        assert_eq!(
            res_repl_update.errors[0].kind,
            PlanRejectionKind::ModeForbidden
        );
        assert!(res_repl_update.errors[0].message.contains("in REPL mode"));

        // 4. REPL mode forbids deleting a plan
        let delete_text = r#"<plan action="delete" id="plan-mode-test" />"#;
        let res_repl_delete =
            PlanStorage::apply_plan_tags_with_mode(root, delete_text, PlanMutationMode::Repl);
        assert!(res_repl_delete.applied.is_empty());
        assert_eq!(res_repl_delete.errors.len(), 1);
        assert_eq!(
            res_repl_delete.errors[0].kind,
            PlanRejectionKind::ModeForbidden
        );
        assert!(res_repl_delete.errors[0].message.contains("in REPL mode"));

        // 5. REPL mode forbids modifying an existing plan even with action="save"
        let overwrite_text = r#"
<plan action="save" id="plan-mode-test" title="Overwritten">
  <concept>
    <concept_item id="c-2" title="New Invariant" />
  </concept>
</plan>
"#;
        let res_repl_overwrite =
            PlanStorage::apply_plan_tags_with_mode(root, overwrite_text, PlanMutationMode::Repl);
        assert!(res_repl_overwrite.applied.is_empty());
        assert_eq!(res_repl_overwrite.errors.len(), 1);
        assert_eq!(
            res_repl_overwrite.errors[0].kind,
            PlanRejectionKind::ModeForbidden
        );
        assert!(res_repl_overwrite.errors[0]
            .message
            .contains("in REPL mode"));

        // 6. Discussion mode permits update
        let res_disc_update =
            PlanStorage::apply_plan_tags_with_mode(root, update_text, PlanMutationMode::Discussion);
        assert_eq!(res_disc_update.applied.len(), 1);
        assert!(res_disc_update.errors.is_empty());
        assert_eq!(
            res_disc_update.applied[0].implementation.items[0].status,
            PlanItemStatus::Done
        );
    }

    #[test]
    fn test_update_plan_item_without_status_preserves_existing_status() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let initial_plan = Plan::from_parts(
            "status-preserve".to_string(),
            "Status Preserve".to_string(),
            None,
            0,
            0,
            vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Concept".to_string(),
                details: None,
                children: vec![],
            }],
            vec![
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
                    files: vec!["src/step2.rs".to_string()],
                    ..Default::default()
                },
            ],
        );
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
        assert_eq!(plan.implementation.items[0].title, "Step 1 Updated");
        assert_eq!(
            plan.implementation.items[0].status,
            PlanItemStatus::InProgress
        );
        assert_eq!(
            plan.implementation.items[0].readonly_files,
            vec!["crates/core/src/lib.rs"]
        );
        assert_eq!(plan.implementation.items[0].depends_on, vec!["2"]);
        assert_eq!(plan.implementation.items[1].status, PlanItemStatus::Blocked);

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
        assert_eq!(
            plan2.implementation.items[0].status,
            PlanItemStatus::InProgress
        );
        assert_eq!(
            plan2.implementation.items[0].readonly_files,
            vec!["crates/protocol/src/lib.rs"]
        );
        assert_eq!(plan2.implementation.items[0].depends_on, vec!["2"]);
    }

    #[test]
    fn test_new_steps_are_created_only_as_todo_or_discussion() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let saved = r#"
<plan action="save" id="lifecycle-plan" title="Lifecycle">
  <concept>
    <concept_item id="c-1" title="Invariant" />
  </concept>
  <item id="1" title="Pending" status="todo" files="src/step1.rs" />
  <item id="2" title="Draft" status="discussion" files="src/step2.rs" />
  <item id="3" title="Running" status="in_progress" files="src/step3.rs" />
  <item id="4" title="Finished" status="done" files="src/step4.rs" />
  <item id="5" title="Stalled" status="blocked" files="src/step5.rs" />
  <item id="6" title="Dropped" status="cancelled" files="src/step6.rs" />
</plan>
"#;
        let outcome = PlanStorage::apply_plan_tags(root, saved);
        assert!(outcome.errors.is_empty(), "Errors: {:?}", outcome.errors);
        assert_eq!(outcome.applied.len(), 1);

        // A created step keeps only the two authoring statuses; every execution
        // status the payload asked for is repaired and reported.
        assert_eq!(outcome.warnings.len(), 4, "{:?}", outcome.warnings);

        let plan = PlanStorage::load_plan(root, "lifecycle-plan")
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.implementation.find_item("1").unwrap().status,
            PlanItemStatus::Todo
        );
        assert_eq!(
            plan.implementation.find_item("2").unwrap().status,
            PlanItemStatus::Discussion
        );
        for id in ["3", "4", "5", "6"] {
            assert_eq!(
                plan.implementation.find_item(id).unwrap().status,
                PlanItemStatus::Todo,
                "step {id}"
            );
            assert!(
                outcome
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(&format!("new step '{id}'"))),
                "step {id}: {:?}",
                outcome.warnings
            );
        }
    }

    #[test]
    fn test_model_cannot_move_an_existing_step_into_progress() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let saved = r#"
<plan action="save" id="lifecycle-repair" title="Lifecycle Repair">
  <concept>
    <concept_item id="c-1" title="Invariant" />
  </concept>
  <item id="1" title="Step 1" status="todo" files="src/step1.rs" />
</plan>
"#;
        let seeded = PlanStorage::apply_plan_tags(root, saved);
        assert!(seeded.errors.is_empty(), "Errors: {:?}", seeded.errors);

        let update = r#"
<plan action="update" id="lifecycle-repair">
  <item id="1" title="Step 1 refined" status="in_progress" />
</plan>
"#;
        let outcome = PlanStorage::apply_plan_tags(root, update);
        assert!(outcome.errors.is_empty(), "Errors: {:?}", outcome.errors);
        assert_eq!(outcome.applied.len(), 1);
        assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
        assert!(outcome.warnings[0].contains("cannot be moved to IN_PROGRESS"));

        let plan = PlanStorage::load_plan(root, "lifecycle-repair")
            .unwrap()
            .unwrap();
        let step = plan
            .implementation
            .find_item("1")
            .expect("step 1 must exist");
        // The rest of the patch still applies: only the lifecycle assignment is
        // refused, and the step keeps the status it carried.
        assert_eq!(step.title, "Step 1 refined");
        assert_eq!(step.status, PlanItemStatus::Todo);

        // Starting a step belongs to the execution pipeline. A mutation that
        // repeats the status the step already carries requests no transition, so
        // it passes untouched.
        PlanStorage::update_item(
            root,
            "lifecycle-repair",
            "1",
            Some(PlanItemStatus::InProgress),
            None,
            None,
        )
        .unwrap()
        .expect("the seeded step must exist");
        let repeated = PlanStorage::apply_plan_tags(root, update);
        assert!(repeated.errors.is_empty(), "Errors: {:?}", repeated.errors);
        assert!(repeated.warnings.is_empty(), "{:?}", repeated.warnings);
        assert_eq!(
            repeated.applied[0]
                .implementation
                .find_item("1")
                .unwrap()
                .status,
            PlanItemStatus::InProgress
        );
    }
    /// A batch settles every tag it accumulated in one pass, so the view its tags
    /// resolve against must advance with the batch's own writes. Two stored plans
    /// share the prefix of the id the batch introduces: a stale view answers the
    /// update with an ambiguous match, while the plan it names is an exact
    /// on-disk match by the time the update is resolved.
    #[test]
    fn test_batch_resolves_mutations_against_its_own_writes() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        for (id, title) in [
            ("feature-alpha", "Feature Alpha"),
            ("feature-beta", "Feature Beta"),
        ] {
            let plan = Plan::from_parts(
                id.to_string(),
                title.to_string(),
                None,
                0,
                0,
                vec![],
                vec![PlanItem {
                    id: "1".to_string(),
                    title: "Existing step".to_string(),
                    status: PlanItemStatus::Todo,
                    files: vec!["src/existing.rs".to_string()],
                    ..Default::default()
                }],
            );
            PlanStorage::save_plan(root, &plan).unwrap();
        }

        let batch = r#"
<plan action="save" id="feature" title="Feature">
  <concept>
    <concept_item id="c-1" title="Invariant" />
  </concept>
  <item id="1" title="Step 1" status="todo" files="src/feature.rs" />
</plan>
<plan action="update" id="feature">
  <item id="1" title="Step 1 refined" />
</plan>
"#;

        let outcome = PlanStorage::apply_plan_tags(root, batch);
        assert!(outcome.errors.is_empty(), "Errors: {:?}", outcome.errors);
        assert_eq!(outcome.applied.len(), 2);

        let stored = PlanStorage::load_plan(root, "feature").unwrap().unwrap();
        assert_eq!(
            stored.implementation.find_item("1").unwrap().title,
            "Step 1 refined"
        );

        // The prefix neighbours keep their own steps untouched.
        for id in ["feature-alpha", "feature-beta"] {
            let neighbour = PlanStorage::load_plan(root, id).unwrap().unwrap();
            assert_eq!(
                neighbour.implementation.find_item("1").unwrap().title,
                "Existing step"
            );
        }
    }

    /// A repeated `save` of one id inside a batch must classify the steps the
    /// earlier save established as existing, exactly as it classifies the steps a
    /// plan that was already on disk carries. Only a step the batch has never seen
    /// is a created one.
    #[test]
    fn test_repeated_save_classifies_batch_established_steps_as_existing() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let batch = r#"
<plan action="save" id="repeat-save" title="Repeat">
  <concept>
    <concept_item id="c-1" title="Invariant" />
  </concept>
  <item id="1" title="Step 1" status="todo" files="src/step1.rs" />
</plan>
<plan action="save" id="repeat-save" title="Repeat">
  <concept>
    <concept_item id="c-1" title="Invariant" />
  </concept>
  <item id="1" title="Step 1 refined" status="blocked" files="src/step1.rs" />
  <item id="2" title="Step 2" status="done" files="src/step2.rs" />
</plan>
"#;

        let outcome = PlanStorage::apply_plan_tags(root, batch);
        assert!(outcome.errors.is_empty(), "Errors: {:?}", outcome.errors);
        assert_eq!(outcome.applied.len(), 2);

        // Exactly one repair: the step the repeated save actually introduced.
        assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
        assert!(
            outcome.warnings[0].contains("new step '2'"),
            "{:?}",
            outcome.warnings
        );

        let stored = PlanStorage::load_plan(root, "repeat-save")
            .unwrap()
            .unwrap();
        let established = stored.implementation.find_item("1").unwrap();
        assert_eq!(established.title, "Step 1 refined");
        assert_eq!(established.status, PlanItemStatus::Blocked);
        assert_eq!(
            stored.implementation.find_item("2").unwrap().status,
            PlanItemStatus::Todo
        );
    }
    /// Builds one executable leaf for the probe tests.
    fn probe_leaf(id: &str, file: &str) -> PlanItem {
        PlanItem {
            id: id.to_string(),
            title: format!("Step {id}"),
            status: PlanItemStatus::Todo,
            files: vec![file.to_string()],
            ..Default::default()
        }
    }

    /// Builds a `save` payload carrying the given steps and one concept node.
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

    /// Renders the reported wording of a batch of typed rejections.
    fn rejection_text(rejections: &[PlanRejection]) -> String {
        rejections
            .iter()
            .map(|rejection| rejection.message.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The probe applies nothing: a payload it accepts leaves storage untouched,
    /// and the application of the same payload then succeeds.
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

    /// The probe reaches the verdict the application reaches, and neither of them
    /// persists a refused payload.
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

        // A regime that forbids the mutation classifies it exactly as the
        // application does, so the probe never reports a refusal the application
        // would not produce.
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

    /// A batch resolves every mutation against the plan the probe already accepted,
    /// exactly as the application resolves it against the plan it stored.
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
    /// The batch entry point settles a whole payload as one storage transaction:
    /// every update resolves against the plan an earlier update produced, and the
    /// last applied mutation stays active.
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

        // The update resolved against the plan the same batch created.
        let stored = PlanStorage::load_plan(root, "batch-a").unwrap().unwrap();
        assert_eq!(
            stored.implementation.find_item("1").unwrap().title,
            "Renamed step"
        );
        assert!(PlanStorage::load_plan(root, "batch-b").unwrap().is_some());
        // The last applied mutation leaves the active-plan marker behind.
        assert_eq!(
            PlanStorage::load_active_id(root).unwrap(),
            Some("batch-b".to_string())
        );
    }
    /// A delta `update` patches every field of an established step, including the
    /// external documents its implementation must obey. The field is part of the
    /// leaf contract, so an update that names `outer_docs` must reach storage
    /// instead of being dropped by the merge.
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

        let update = r#"
<plan action="update" id="outer-docs-plan">
  <item id="1" outer_docs="protocol-spec, wire-format" />
</plan>
"#;
        let outcome = PlanStorage::apply_plan_tags(root, update);
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
        // The rest of the leaf contract survives the patch untouched.
        assert_eq!(step.files, vec!["src/step1.rs"]);
    }
}
