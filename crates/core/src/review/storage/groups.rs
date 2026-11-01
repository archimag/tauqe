use anyhow::Result;
use std::path::Path;
use tauqe_protocol::{
    ArchitectTier, IssueStatus, PlanImplementation, PlanItem, ReviewGroup, ReviewIssue, ReviewItem,
    ReviewSession, DEFAULT_REVIEW_GROUP_ID,
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
        group.implementation.items =
            crate::plan::storage::merge_subplan(&group.implementation.items, new_subplan)?;
        group.implementation.architect = Some(architect);
        group.implementation.last_replanned_step = last_replanned_step;
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
    /// their leaves, and [`PlanImplementation::refresh_parent_statuses`]
    /// recomputes that derived value, so a stored container can never disagree
    /// with the children it aggregates. The whole read-modify-write cycle runs
    /// inside a single
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
        let mut implementation = PlanImplementation {
            items: subplan,
            ..Default::default()
        };
        implementation.refresh_parent_statuses();
        group.implementation.items = implementation.items;
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
            if !finding_in_group(item, group_id) {
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

    /// Records or clears the blocker that currently stops the named review
    /// group.
    ///
    /// A group that cannot be decomposed, or whose remediation stalled on an
    /// invariant conflict, states the diagnosis on itself: the blocker belongs
    /// to the group that has to resolve it, not to the notes of the findings it
    /// manifests. A non-blank reason is stored trimmed; `None` or a blank
    /// reason clears the blocker and returns the group to an unblocked state.
    ///
    /// Returns `None` when the named session does not exist or declares no
    /// group with that identifier, so an absent target is reported instead of
    /// being invented.
    pub fn set_group_blocked_reason(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        reason: Option<&str>,
    ) -> Result<Option<ReviewGroup>> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::set_group_blocked_reason_unlocked(repo_root, review_id, group_id, reason)
        })
    }

    /// Lock-free core of [`Self::set_group_blocked_reason`]; the caller must
    /// hold the storage lock.
    fn set_group_blocked_reason_unlocked(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        reason: Option<&str>,
    ) -> Result<Option<ReviewGroup>> {
        let mut session = match Self::load_review_unlocked(repo_root, review_id)? {
            Some(session) => session,
            None => return Ok(None),
        };

        let Some(group) = find_group_mut(&mut session, group_id) else {
            return Ok(None);
        };
        group.blocked_reason = reason
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(str::to_string);
        let updated = group.clone();

        Self::save_session_unlocked(repo_root, &session)?;
        Ok(Some(updated))
    }

    /// Records the decision that resolves the architectural fork of one review
    /// group and publishes it to the findings the group collects.
    ///
    /// A decision ends the blocker: the group stores the resolution once and
    /// clears `blocked_reason`, and every finding of the group receives the
    /// same note, because the findings of an architectural group are symptoms
    /// of the one cause the decision settled. The whole transition is a single
    /// read-modify-write cycle under the process-global storage lock, so the
    /// group and its findings can never disagree about the decision, and a
    /// repeated decomposition reads the recorded resolution through
    /// `resolution_notes` instead of reopening the fork.
    ///
    /// The decision must carry its own reasoning: a blank note is rejected,
    /// because recording that something was decided without saying what would
    /// leave the next decomposition with a blocker it cannot interpret.
    /// Findings of other groups are never touched, and a group the session does
    /// not declare resolves to `None` without any write.
    pub fn apply_group_decision(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        note: &str,
    ) -> Result<Option<ReviewGroup>> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::apply_group_decision_unlocked(repo_root, review_id, group_id, note)
        })
    }

    /// Lock-free core of [`Self::apply_group_decision`]; the caller must hold
    /// the storage lock.
    fn apply_group_decision_unlocked(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        note: &str,
    ) -> Result<Option<ReviewGroup>> {
        let resolution = note.trim();
        if resolution.is_empty() {
            anyhow::bail!(
                "Cannot record a decision for review group '{group_id}': the resolution note is empty"
            );
        }

        let mut session = match Self::load_review_unlocked(repo_root, review_id)? {
            Some(session) => session,
            None => return Ok(None),
        };

        let updated = {
            let Some(group) = find_group_mut(&mut session, group_id) else {
                return Ok(None);
            };
            group.blocked_reason = None;
            group.resolution_notes = Some(resolution.to_string());
            group.clone()
        };

        for item in &mut session.items {
            if !finding_in_group(item, group_id) {
                continue;
            }

            // Mutate the finding through the canonical `ReviewIssue` model, so
            // the persisted record and the analytical view can never disagree
            // about the fields they share.
            let mut issue = ReviewIssue::from(item.clone());
            issue.note = Some(resolution.to_string());
            *item = ReviewItem::from(issue);
        }

        Self::save_session_unlocked(repo_root, &session)?;
        Ok(Some(updated))
    }
    /// Closes the implementation horizon of one review group.
    ///
    /// A group whose horizon reached the aggregate `Done` status has completed
    /// its remediation, so every finding the group still holds open — in
    /// `Discussion`, `Todo`, or `InProgress` — is closed as `Fixed`. Findings of
    /// other groups are never observed, and `Skipped`, `Rejected`, and `Fixed`
    /// findings are left exactly as they are: those are verdicts the developer
    /// already stated, and a completion is forbidden to revoke them.
    ///
    /// The whole transition is a single read-modify-write cycle under the
    /// process-global storage lock, so the findings and the group are published as
    /// one edit of the session file and the session is never observably
    /// half-closed. The same write clears `blocked_reason`: a group whose horizon
    /// finished has nothing left to report. `note` records the completion on the
    /// findings that carry no note of their own, and a note a finding already
    /// carries is never rewritten.
    ///
    /// Returns the number of findings the completion closed. `None` when the
    /// named session does not exist or declares no group with that identifier; a
    /// declared group with no open finding resolves to `Some(0)` and performs no
    /// write.
    pub fn close_group_implementation(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        note: &str,
    ) -> Result<Option<usize>> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::close_group_implementation_unlocked(repo_root, review_id, group_id, note)
        })
    }

    /// Lock-free core of [`Self::close_group_implementation`]; the caller must
    /// hold the storage lock.
    fn close_group_implementation_unlocked(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        note: &str,
    ) -> Result<Option<usize>> {
        let mut session = match Self::load_review_unlocked(repo_root, review_id)? {
            Some(session) => session,
            None => return Ok(None),
        };

        // A group the session does not declare has no members to close, so the
        // completion reports that it changed nothing instead of inferring an
        // implicit membership the session never stated.
        if !session.groups.iter().any(|group| group.id == group_id) {
            return Ok(None);
        }

        let completion = note.trim();
        let mut closed = 0usize;
        for item in &mut session.items {
            if !finding_in_group(item, group_id) || !is_open_finding(item.status) {
                continue;
            }

            // Mutate the finding through the canonical `ReviewIssue` model, so
            // the persisted record and the analytical view can never disagree
            // about the fields they share.
            let mut issue = ReviewIssue::from(item.clone());
            issue.status = IssueStatus::Fixed;
            if !completion.is_empty() && !carries_note(issue.note.as_deref()) {
                issue.note = Some(completion.to_string());
            }
            *item = ReviewItem::from(issue);
            closed += 1;
        }

        // The blocker belongs to the group that reported it, so the same write
        // clears it: the finding verdicts and the group state are never published
        // apart.
        let mut writes = closed > 0;
        if let Some(group) = find_group_mut(&mut session, group_id) {
            writes |= group.blocked_reason.take().is_some();
        }

        if writes {
            Self::save_session_unlocked(repo_root, &session)?;
        }

        Ok(Some(closed))
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

/// Whether a stored finding belongs to the named review group.
///
/// Membership follows the effective group of a finding: a finding that stores
/// no group belongs to the implicit [`DEFAULT_REVIEW_GROUP_ID`] group, exactly
/// as the grouped session view and the post-analysis layer resolve it, so every
/// group-scoped primitive observes one membership rule instead of re-deriving
/// it.
fn finding_in_group(item: &ReviewItem, group_id: &str) -> bool {
    match item.group.as_deref() {
        Some(group) => group == group_id,
        None => group_id == DEFAULT_REVIEW_GROUP_ID,
    }
}

/// Whether a finding still awaits the completion of its group.
///
/// Only a finding the remediation has not concluded is open: a verdict the
/// developer stated (`Skipped`, `Rejected`) and a finding already `Fixed` are
/// terminal outcomes the completion never rewrites.
fn is_open_finding(status: IssueStatus) -> bool {
    matches!(
        status,
        IssueStatus::Discussion | IssueStatus::Todo | IssueStatus::InProgress
    )
}

/// Whether a finding already carries a resolution note.
///
/// A note the developer stated is never rewritten by a completion: the note of
/// the completion is recorded only on a finding that carries none.
fn carries_note(note: Option<&str>) -> bool {
    note.map(str::trim).is_some_and(|text| !text.is_empty())
}
