use anyhow::Result;
use std::path::Path;
use tauqe_protocol::{
    ArchitectTier, Plan, PlanItem, ReviewGroup, ReviewIssue, ReviewItem, ReviewSession,
    DEFAULT_REVIEW_GROUP_ID,
};

use super::ReviewStorage;

impl ReviewStorage {
    /// Replaces the unified remediation subplan of one review group's
    /// architectural root cause while preserving the immutable `Done`
    /// foundation.
    ///
    /// A group is the operational unit of remediation: one root cause is fixed
    /// by one coherent change set, so its horizon is synthesized exactly like a
    /// plan's. The Architect returns only the new steps for the remaining work
    /// and the harness guarantees the atomic concatenation
    /// `[...done_steps, ...new_horizon_steps]`. Completed steps are never
    /// regenerated, leaf-only file declarations are normalized, derived
    /// container statuses are recomputed, and a horizon that would close a
    /// dependency cycle or leave no ready step is rejected before anything
    /// reaches disk.
    ///
    /// `architect` is the configuration that produced the horizon and
    /// `last_replanned_step` names the sub-step whose horizon this wave
    /// reprojects. Both are recorded on the group, so a later wave reads them
    /// from disk instead of in-memory supervisor state. An initial synthesis
    /// passes `None`.
    ///
    /// Returns `None` when the named session does not exist or declares no
    /// group with that identifier.
    pub fn replace_uncompleted_group_subplan(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        new_subplan: Vec<PlanItem>,
        architect: ArchitectTier,
        last_replanned_step: Option<String>,
    ) -> Result<Option<ReviewGroup>> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::replace_uncompleted_group_subplan_unlocked(
                repo_root,
                review_id,
                group_id,
                new_subplan,
                architect,
                last_replanned_step,
            )
        })
    }

    /// Lock-free core of [`Self::replace_uncompleted_group_subplan`]; the
    /// caller must hold the storage lock.
    fn replace_uncompleted_group_subplan_unlocked(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        new_subplan: Vec<PlanItem>,
        architect: ArchitectTier,
        last_replanned_step: Option<String>,
    ) -> Result<Option<ReviewGroup>> {
        let mut session = match Self::load_review_unlocked(repo_root, review_id)? {
            Some(session) => session,
            None => return Ok(None),
        };

        // A group horizon lives on the group itself, so a session that does not
        // declare the group has nothing to reproject and reports that it
        // changed nothing instead of inventing one.
        let Some(group) = find_group_mut(&mut session, group_id) else {
            return Ok(None);
        };
        group.subplan = crate::plan::storage::merge_subplan(&group.subplan, new_subplan)?;
        group.subplan_architect = Some(architect);
        group.subplan_last_replanned_step = last_replanned_step;
        let updated = group.clone();

        Self::save_session_unlocked(repo_root, &session)?;
        Ok(Some(updated))
    }

    /// Replaces the unified remediation subplan of one review group without
    /// preserving the previous horizon.
    ///
    /// The supplied subplan is the authoritative remediation horizon of the
    /// group, so it is normalized before it is persisted. Container steps carry
    /// no authoritative status of their own: their status is a pure function of
    /// their leaves, and [`Plan::refresh_parent_statuses`] recomputes that
    /// derived value, so a stored container can never disagree with the children
    /// it aggregates. The whole read-modify-write cycle runs inside a single
    /// acquisition of the process-global storage lock, so a concurrent update
    /// cannot interleave between the load and the save.
    ///
    /// Returns `None` when the named session does not exist or declares no group
    /// with that identifier.
    pub fn update_group_subplan(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        subplan: Vec<PlanItem>,
    ) -> Result<Option<ReviewGroup>> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::update_group_subplan_unlocked(repo_root, review_id, group_id, subplan)
        })
    }

    /// Lock-free core of [`Self::update_group_subplan`]; the caller must hold
    /// the storage lock.
    fn update_group_subplan_unlocked(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        subplan: Vec<PlanItem>,
    ) -> Result<Option<ReviewGroup>> {
        let mut session = match Self::load_review_unlocked(repo_root, review_id)? {
            Some(session) => session,
            None => return Ok(None),
        };

        let Some(group) = find_group_mut(&mut session, group_id) else {
            return Ok(None);
        };
        let mut plan = Plan {
            items: subplan,
            ..Default::default()
        };
        plan.refresh_parent_statuses();
        group.subplan = plan.items;
        let updated = group.clone();

        Self::save_session_unlocked(repo_root, &session)?;
        Ok(Some(updated))
    }
    /// Applies one in-place update to every finding of the named review group
    /// and persists the whole result in a single atomic write.
    ///
    /// A group's remediation is one coherent change set whose steps share a
    /// single lifecycle, so its findings move through their statuses together:
    /// they all become `InProgress` while the horizon runs and all become
    /// `Fixed` once its last step verifies. Persisting such a transition one
    /// finding at a time would leave the session observably half-updated
    /// between the writes and would pay a full read-modify-write cycle per
    /// finding. This primitive therefore loads the session once under the
    /// process-global storage lock, applies `updater` to every member of the
    /// group, and saves once, so the whole transition is published as a single
    /// edit of the session file.
    ///
    /// Membership follows the effective group of a finding: a finding that
    /// stores no group belongs to the implicit [`DEFAULT_REVIEW_GROUP_ID`]
    /// group, exactly as the grouped session view and the post-analysis layer
    /// resolve it. The group itself must be declared by the session, so an
    /// unknown group or a missing session resolves to `None` and leaves the
    /// stored session untouched, while a declared group with no member finding
    /// resolves to `Some(0)` and performs no write. Findings of every other
    /// group are never observed by `updater`.
    ///
    /// Returns the number of findings the update reached.
    pub fn update_session_group_findings<F>(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        updater: F,
    ) -> Result<Option<usize>>
    where
        F: FnMut(&mut ReviewIssue),
    {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::update_session_group_findings_unlocked(repo_root, review_id, group_id, updater)
        })
    }

    /// Lock-free core of [`Self::update_session_group_findings`]; the caller
    /// must hold the storage lock.
    fn update_session_group_findings_unlocked<F>(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        mut updater: F,
    ) -> Result<Option<usize>>
    where
        F: FnMut(&mut ReviewIssue),
    {
        let mut session = match Self::load_review_unlocked(repo_root, review_id)? {
            Some(session) => session,
            None => return Ok(None),
        };

        // A group the session does not declare has no members to transition, so
        // the update reports that it changed nothing instead of inferring an
        // implicit membership the session never stated.
        if !session.groups.iter().any(|group| group.id == group_id) {
            return Ok(None);
        }

        let mut affected = 0usize;
        for item in &mut session.items {
            let belongs_to_group = match item.group.as_deref() {
                Some(group) => group == group_id,
                None => group_id == DEFAULT_REVIEW_GROUP_ID,
            };
            if !belongs_to_group {
                continue;
            }

            // Mutate the finding through the canonical `ReviewIssue` model, so
            // the persisted record and the analytical view can never disagree
            // about the fields they share.
            let mut issue = ReviewIssue::from(item.clone());
            updater(&mut issue);
            *item = ReviewItem::from(issue);
            affected += 1;
        }

        if affected > 0 {
            Self::save_session_unlocked(repo_root, &session)?;
        }

        Ok(Some(affected))
    }
}

/// Resolves the review group of a session by id.
///
/// A group horizon lives on the group itself, so a session that does not
/// declare the group has nothing to update; both cases resolve to `None` and
/// leave the stored session untouched.
fn find_group_mut<'a>(
    session: &'a mut ReviewSession,
    group_id: &str,
) -> Option<&'a mut ReviewGroup> {
    session.groups.iter_mut().find(|group| group.id == group_id)
}
