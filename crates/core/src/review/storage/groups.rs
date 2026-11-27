use crate::models::ArchitectTier;
use crate::plan::{PlanImplementation, PlanItem, PlanItemStatus, PlanScopeEvaluation};
use crate::review::{
    IssueStatus, ReviewGroup, ReviewGroupKind, ReviewIssue, ReviewItem, ReviewSession,
    ReviewUpdateProposal, DEFAULT_REVIEW_GROUP_ID,
};
use anyhow::Result;
use std::path::Path;

use super::items::{is_open_finding, record_resolution};
use super::ReviewStorage;

#[derive(Debug, PartialEq, Eq)]
pub enum GroupCompletion {
    Missing,
    Incomplete,
    Closed(usize),
}

/// Result of an initial horizon write. An existing horizon is never replaced.
#[derive(Debug, PartialEq, Eq)]
pub enum InitialGroupHorizon {
    Written(ReviewGroup),
    Existing(ReviewGroup),
}

/// A rejected initial synthesis is distinct from an I/O failure.
#[derive(Debug, thiserror::Error)]
pub enum InitialGroupHorizonError {
    #[error("Review group '{group_id}' of session '{review_id}' does not exist")]
    MissingTarget { review_id: String, group_id: String },
    #[error("Review group '{0}' is not architectural")]
    NotArchitectural(String),
    #[error("The synthesized group horizon is empty")]
    EmptyHorizon,
    #[error("Invalid synthesized group horizon: {0}")]
    InvalidTree(#[source] anyhow::Error),
    #[error("Failed to store the synthesized group horizon: {0}")]
    Storage(#[source] anyhow::Error),
}

/// A refused refinement leaves the stored session unchanged.
#[derive(Debug, thiserror::Error)]
pub enum GroupHorizonReplacementError {
    #[error("Review group '{group_id}' of session '{review_id}' does not exist")]
    MissingTarget { review_id: String, group_id: String },
    #[error("Review group '{0}' is not architectural")]
    NotArchitectural(String),
    #[error("The refined group horizon is empty")]
    EmptyHorizon,
    #[error("Invalid refined group horizon: {0}")]
    InvalidTree(#[source] anyhow::Error),
    #[error("Review group '{0}' changed since refinement began")]
    StaleSnapshot(String),
    #[error("Failed to store the refined group horizon: {0}")]
    Storage(#[source] anyhow::Error),
}

/// A refused manual step update leaves the stored review unchanged.
#[derive(Debug, thiserror::Error)]
pub enum GroupStepStatusError {
    #[error("Review session '{0}' does not exist")]
    MissingSession(String),
    #[error("Review group '{group_id}' of session '{review_id}' does not exist")]
    MissingGroup { review_id: String, group_id: String },
    #[error("Review group '{0}' is not architectural")]
    NotArchitectural(String),
    #[error("Step '{0}' does not exist in the review group")]
    MissingStep(String),
    #[error("Invalid step status transition: {0}")]
    InvalidTransition(#[source] anyhow::Error),
    #[error("Invalid resulting group horizon: {0}")]
    InvalidTree(#[source] anyhow::Error),
    #[error("Failed to store the group step status: {0}")]
    Storage(#[source] anyhow::Error),
}

impl ReviewStorage {
    /// Updates one leaf of an existing architectural group under the storage lock.
    /// An unchanged status is a complete no-op, including its lifecycle fields.
    pub fn update_group_step_status(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        step_id: &str,
        status: PlanItemStatus,
    ) -> std::result::Result<ReviewGroup, GroupStepStatusError> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            let (path, mut session) = Self::resolve_session_file_unlocked(repo_root, review_id)
                .map_err(GroupStepStatusError::Storage)?
                .ok_or_else(|| GroupStepStatusError::MissingSession(review_id.into()))?;
            let group = find_group_mut(&mut session, group_id).ok_or_else(|| {
                GroupStepStatusError::MissingGroup {
                    review_id: review_id.into(),
                    group_id: group_id.into(),
                }
            })?;
            if group.kind != ReviewGroupKind::Architectural {
                return Err(GroupStepStatusError::NotArchitectural(group_id.into()));
            }
            let step = group
                .implementation
                .find_item(step_id)
                .ok_or_else(|| GroupStepStatusError::MissingStep(step_id.into()))?;
            if !step.is_leaf() {
                return Err(GroupStepStatusError::InvalidTransition(anyhow::anyhow!(
                    "Step '{}' is a container",
                    step.id
                )));
            }
            if step.status == status {
                return Ok(group.clone());
            }
            group
                .implementation
                .transition_leaf(step_id, status, None, None)
                .map_err(|error| GroupStepStatusError::InvalidTransition(error.into()))?;
            group
                .implementation
                .validate_structure()
                .map_err(|errors| {
                    GroupStepStatusError::InvalidTree(anyhow::anyhow!(
                        "{}",
                        errors
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("; ")
                    ))
                })?;
            let updated = group.clone();
            Self::write_session_unlocked(repo_root, &path, &session)
                .map_err(GroupStepStatusError::Storage)?;
            Ok(updated)
        })
    }

    /// Commits an initial architectural horizon and clears its stale blocker in
    /// the same atomic session write. A competing writer's horizon wins.
    pub fn write_initial_group_horizon(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        items: Vec<PlanItem>,
        architect: ArchitectTier,
    ) -> std::result::Result<InitialGroupHorizon, InitialGroupHorizonError> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            let (path, mut session) = Self::resolve_session_file_unlocked(repo_root, review_id)
                .map_err(InitialGroupHorizonError::Storage)?
                .ok_or_else(|| InitialGroupHorizonError::MissingTarget {
                    review_id: review_id.into(),
                    group_id: group_id.into(),
                })?;
            let group = find_group_mut(&mut session, group_id).ok_or_else(|| {
                InitialGroupHorizonError::MissingTarget {
                    review_id: review_id.into(),
                    group_id: group_id.into(),
                }
            })?;
            if group.kind != ReviewGroupKind::Architectural {
                return Err(InitialGroupHorizonError::NotArchitectural(group_id.into()));
            }
            if !group.implementation.items.is_empty() {
                return Ok(InitialGroupHorizon::Existing(group.clone()));
            }
            if items.is_empty() {
                return Err(InitialGroupHorizonError::EmptyHorizon);
            }
            let merged = crate::plan::storage::merge_subplan(&group.implementation.items, items)
                .map_err(InitialGroupHorizonError::InvalidTree)?;
            if merged.is_empty() {
                return Err(InitialGroupHorizonError::EmptyHorizon);
            }
            group.implementation.items = merged;
            group.implementation.architect = Some(architect);
            group.implementation.last_replanned_step = None;
            group.blocked_reason = None;
            let updated = group.clone();
            Self::write_session_unlocked(repo_root, &path, &session)
                .map_err(InitialGroupHorizonError::Storage)?;
            Ok(InitialGroupHorizon::Written(updated))
        })
    }
    /// Applies a leaf transition to the current stored group horizon.
    ///
    /// Lookup, domain validation, and persistence share one storage lock.
    /// A refusal leaves the document untouched; an idempotent transition
    /// returns the current horizon without rewriting the file.
    pub fn transition_group_leaf(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        step_id: &str,
        status: PlanItemStatus,
        blocked_reason: Option<&str>,
        commit: Option<&str>,
    ) -> Result<PlanImplementation> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            let (path, mut session) = Self::resolve_session_file_unlocked(repo_root, review_id)?
                .ok_or_else(|| anyhow::anyhow!("Review session '{}' is not stored", review_id))?;
            let group = find_group_mut(&mut session, group_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "Review group '{}' of session '{}' is not stored",
                    group_id,
                    review_id
                )
            })?;
            let changed = group
                .implementation
                .transition_leaf(step_id, status, blocked_reason, commit)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Review group '{}' of session '{}' does not store step '{}'",
                        group_id,
                        review_id,
                        step_id
                    )
                })?;
            let implementation = group.implementation.clone();
            if changed {
                Self::write_session_unlocked(repo_root, &path, &session)?;
            }
            Ok(implementation)
        })
    }

    /// Replaces only the unfinished horizon of an architectural group while
    /// preserving the immutable `Done` foundation.
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
    /// The expected group and its member findings are the synthesis snapshot.
    /// Changes elsewhere in the session do not invalidate that snapshot.
    /// Refusals never write; a successful manual refinement clears the group
    /// blocker and the previous replanned-step marker in the same write.
    pub fn replace_uncompleted_group_subplan(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        expected_group: &ReviewGroup,
        expected_findings: &[ReviewItem],
        new_subplan: Vec<PlanItem>,
        architect: ArchitectTier,
    ) -> std::result::Result<ReviewGroup, GroupHorizonReplacementError> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::replace_uncompleted_group_subplan_unlocked(
                repo_root,
                review_id,
                group_id,
                expected_group,
                expected_findings,
                new_subplan,
                architect,
            )
        })
    }

    /// Lock-free core of [`Self::replace_uncompleted_group_subplan`]; the
    /// caller must hold the storage lock.
    fn replace_uncompleted_group_subplan_unlocked(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        expected_group: &ReviewGroup,
        expected_findings: &[ReviewItem],
        new_subplan: Vec<PlanItem>,
        architect: ArchitectTier,
    ) -> std::result::Result<ReviewGroup, GroupHorizonReplacementError> {
        let (path, mut session) = Self::resolve_session_file_unlocked(repo_root, review_id)
            .map_err(GroupHorizonReplacementError::Storage)?
            .ok_or_else(|| GroupHorizonReplacementError::MissingTarget {
                review_id: review_id.into(),
                group_id: group_id.into(),
            })?;
        let group_index = session
            .groups
            .iter()
            .position(|group| group.id == group_id)
            .ok_or_else(|| GroupHorizonReplacementError::MissingTarget {
                review_id: review_id.into(),
                group_id: group_id.into(),
            })?;
        let group = &session.groups[group_index];
        if group.kind != ReviewGroupKind::Architectural {
            return Err(GroupHorizonReplacementError::NotArchitectural(
                group_id.into(),
            ));
        }
        if group != expected_group
            || !session
                .items
                .iter()
                .filter(|item| finding_in_group(item, group_id))
                .eq(expected_findings.iter())
        {
            return Err(GroupHorizonReplacementError::StaleSnapshot(group_id.into()));
        }
        if new_subplan.is_empty() {
            return Err(GroupHorizonReplacementError::EmptyHorizon);
        }
        let merged = crate::plan::storage::merge_subplan(&group.implementation.items, new_subplan)
            .map_err(GroupHorizonReplacementError::InvalidTree)?;
        let implementation = PlanImplementation {
            items: merged,
            ..Default::default()
        };
        if !matches!(
            implementation.evaluate_scope(None),
            PlanScopeEvaluation::Ready(_)
        ) {
            return Err(GroupHorizonReplacementError::InvalidTree(anyhow::anyhow!(
                "Refined group horizon has no executable remaining work"
            )));
        }
        let group = &mut session.groups[group_index];
        group.implementation.items = implementation.items;
        group.implementation.architect = Some(architect);
        group.implementation.last_replanned_step = None;
        group.blocked_reason = None;
        let updated = group.clone();
        Self::write_session_unlocked(repo_root, &path, &session)
            .map_err(GroupHorizonReplacementError::Storage)?;
        Ok(updated)
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
    #[cfg(test)]
    pub(crate) fn update_session_group_findings<F>(
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
    #[cfg(test)]
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
        let Some((path, mut session)) = Self::resolve_session_file_unlocked(repo_root, review_id)?
        else {
            return Ok(None);
        };

        let Some(group) = find_group_mut(&mut session, group_id) else {
            return Ok(None);
        };
        group.blocked_reason = reason
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(str::to_string);
        let updated = group.clone();

        Self::write_session_unlocked(repo_root, &path, &session)?;
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
    /// A missing target or an empty or incomplete horizon leaves the session
    /// untouched. Completion returns the number of newly closed findings and
    /// writes only if a finding or the group blocker changed.
    pub fn close_group_implementation(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        note: &str,
    ) -> Result<GroupCompletion> {
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
    ) -> Result<GroupCompletion> {
        let Some((path, mut session)) = Self::resolve_session_file_unlocked(repo_root, review_id)?
        else {
            return Ok(GroupCompletion::Missing);
        };

        let Some(group) = find_group_mut(&mut session, group_id) else {
            return Ok(GroupCompletion::Missing);
        };
        if group.implementation.items.is_empty()
            || group.implementation.status() != PlanItemStatus::Done
        {
            return Ok(GroupCompletion::Incomplete);
        }
        let cleared_blocker = group.blocked_reason.take().is_some();

        let mut closed = 0usize;
        for item in &mut session.items {
            if !finding_in_group(item, group_id) || !is_open_finding(item.status) {
                continue;
            }

            // Mutate the finding through the canonical `ReviewIssue` model, so
            // the persisted record and the analytical view can never disagree
            // about the fields they share.
            let mut issue = ReviewIssue::from(item.clone());
            record_resolution(&mut issue, IssueStatus::Fixed, note);
            *item = ReviewItem::from(issue);
            closed += 1;
        }

        if closed > 0 || cleared_blocker {
            Self::write_session_unlocked(repo_root, &path, &session)?;
        }

        Ok(GroupCompletion::Closed(closed))
    }
}

/// Applies an admitted group decision, returning false if the group is absent.
pub(super) fn apply_group_decision(
    session: &mut ReviewSession,
    group_id: &str,
    decision: &ReviewUpdateProposal,
) -> bool {
    let Some(group) = find_group_mut(session, group_id) else {
        return false;
    };
    group.blocked_reason = None;
    group.resolution_notes = Some(decision.note.trim().to_string());
    for item in &mut session.items {
        if finding_in_group(item, group_id) {
            let mut issue = ReviewIssue::from(item.clone());
            issue.apply_decision(decision);
            *item = ReviewItem::from(issue);
        }
    }
    true
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
pub(super) fn finding_in_group(item: &ReviewItem, group_id: &str) -> bool {
    match item.group.as_deref() {
        Some(group) => group == group_id,
        None => group_id == DEFAULT_REVIEW_GROUP_ID,
    }
}
