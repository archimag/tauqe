use anyhow::Result;
use std::path::Path;

use crate::discussion::DiscussionTarget;
use crate::review::ReviewUpdateProposal;
use crate::review::{IssueStatus, ReviewIssue, ReviewItem, ReviewSession};

use super::groups::{apply_group_decision, finding_in_group};
use super::{PreparedReviewDecisions, ReviewBatchCommitError, ReviewStorage, SiblingBatch};
use crate::execution::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDecisionOutcome {
    MissingSession,
    MissingFinding,
    MissingGroup,
    TargetMismatch,
    EmptyGroupNote,
    Saved,
}

impl ReviewStorage {
    /// Commit the prepared session and its marker as a reversible file batch.
    pub fn commit_prepared_decisions(
        repo_root: &Path,
        prepared: PreparedReviewDecisions,
        cancel: &CancellationToken,
    ) -> Result<ReviewSession, ReviewBatchCommitError> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            let current = std::fs::read(&prepared.path)?;
            if current != prepared.original {
                return Err(ReviewBatchCommitError::Conflict(prepared.path));
            }
            let marker = Self::reviews_dir(repo_root).join("active");
            let active = match std::fs::read(&marker) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if active != prepared.active_before {
                return Err(ReviewBatchCommitError::Conflict(marker));
            }
            let mut files = SiblingBatch::default();
            files.replace(
                prepared.path,
                Some(serde_json::to_vec_pretty(&prepared.session)?),
            );
            if active.is_none() {
                files.replace(marker, Some(prepared.session.id.as_bytes().to_vec()));
            }
            files.apply(|| cancel.is_cancelled(), || !cancel.is_cancelled())?;
            Ok(prepared.session)
        })
    }

    /// Apply a branch's decisions to one owned session without persisting it.
    pub fn prepare_decisions(
        repo_root: &Path,
        focus: &DiscussionTarget,
        decisions: &[ReviewUpdateProposal],
    ) -> Result<(Option<PreparedReviewDecisions>, Vec<ReviewDecisionOutcome>)> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            let review_id = match focus {
                DiscussionTarget::Review { id } => id,
                DiscussionTarget::ReviewGroup { session_id, .. } => session_id,
                DiscussionTarget::Plan { .. } => {
                    return Ok((
                        None,
                        vec![ReviewDecisionOutcome::TargetMismatch; decisions.len()],
                    ));
                }
            };
            let Some((path, session)) = Self::resolve_session_file_unlocked(repo_root, review_id)?
            else {
                return Ok((
                    None,
                    vec![ReviewDecisionOutcome::MissingSession; decisions.len()],
                ));
            };
            let original = std::fs::read(&path)?;
            let active = Self::reviews_dir(repo_root).join("active");
            let active_before = match std::fs::read(active) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            let mut prepared = PreparedReviewDecisions {
                path,
                original,
                session,
                active_before,
            };
            let outcomes = decisions
                .iter()
                .map(|decision| {
                    Self::apply_decision_to_session(&mut prepared.session, focus, decision)
                })
                .collect();
            Ok((Some(prepared), outcomes))
        })
    }

    /// Updates a review item by id within the explicitly named session.
    ///
    /// The session id is mandatory: review mutations are strictly scoped to the
    /// session the caller names, so the process-global active marker is never
    /// consulted as an implicit fallback.
    pub fn update_session_item<F>(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        updater: F,
    ) -> Result<Option<ReviewIssue>>
    where
        F: FnOnce(&mut ReviewIssue),
    {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            Self::update_session_item_unlocked(repo_root, review_id, item_id, updater)
        })
    }

    /// Lock-free core of [`Self::update_session_item`]; caller must hold storage lock.
    fn update_session_item_unlocked<F>(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        updater: F,
    ) -> Result<Option<ReviewIssue>>
    where
        F: FnOnce(&mut ReviewIssue),
    {
        let mut session = match Self::load_review_unlocked(repo_root, review_id)? {
            Some(s) => s,
            None => return Ok(None),
        };

        let updated_issue = update_item(&mut session, item_id, updater);

        if updated_issue.is_some() {
            Self::save_session_unlocked(repo_root, &session)?;
        }

        Ok(updated_issue)
    }

    /// Updates status of a review item within the explicitly named review session.
    pub fn update_session_item_status(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        status: IssueStatus,
    ) -> Result<Option<ReviewIssue>> {
        Self::update_session_item(repo_root, review_id, item_id, |issue| {
            issue.status = status;
        })
    }

    /// Completes one finding without replacing its recorded commit or developer note.
    pub fn complete_finding(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        note: &str,
    ) -> Result<Option<()>> {
        Self::settle_finding(repo_root, review_id, item_id, IssueStatus::Fixed, note)
    }

    /// Records why execution skipped one finding, preserving any existing verdict or note.
    pub fn skip_finding(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        reason: &str,
    ) -> Result<Option<()>> {
        let note = format!("The implementation run could not address this finding: {reason}");
        Self::settle_finding(repo_root, review_id, item_id, IssueStatus::Skipped, &note)
    }

    fn settle_finding(
        repo_root: &Path,
        review_id: &str,
        item_id: u32,
        status: IssueStatus,
        note: &str,
    ) -> Result<Option<()>> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            let Some((path, mut session)) =
                Self::resolve_session_file_unlocked(repo_root, review_id)?
            else {
                return Ok(None);
            };
            let Some(item) = session.items.iter_mut().find(|item| item.id == item_id) else {
                return Ok(None);
            };
            if is_open_finding(item.status) {
                let mut issue = ReviewIssue::from(item.clone());
                record_resolution(&mut issue, status, note);
                *item = ReviewItem::from(issue);
                Self::write_session_unlocked(repo_root, &path, &session)?;
            }
            Ok(Some(()))
        })
    }

    /// Applies a dialogue decision within its frozen review focus.
    /// A group focus scopes even a decision that addresses only one finding.
    pub fn apply_decision(
        repo_root: &Path,
        focus: &DiscussionTarget,
        decision: &ReviewUpdateProposal,
    ) -> Result<ReviewDecisionOutcome> {
        crate::blocking::run_blocking(|| {
            let _guard = Self::lock();
            let review_id = match focus {
                DiscussionTarget::Review { id } => (id, None),
                DiscussionTarget::ReviewGroup {
                    session_id,
                    group_id,
                } => (session_id, Some(group_id.as_str())),
                DiscussionTarget::Plan { .. } => return Ok(ReviewDecisionOutcome::TargetMismatch),
            }
            .0;
            let Some((path, mut session)) =
                Self::resolve_session_file_unlocked(repo_root, review_id)?
            else {
                return Ok(ReviewDecisionOutcome::MissingSession);
            };
            let result = Self::apply_decision_to_session(&mut session, focus, decision);
            if result == ReviewDecisionOutcome::Saved {
                Self::write_session_unlocked(repo_root, &path, &session)?;
            }
            Ok(result)
        })
    }

    fn apply_decision_to_session(
        session: &mut ReviewSession,
        focus: &DiscussionTarget,
        decision: &ReviewUpdateProposal,
    ) -> ReviewDecisionOutcome {
        let focused_group = match focus {
            DiscussionTarget::Review { .. } => None,
            DiscussionTarget::ReviewGroup { group_id, .. } => Some(group_id.as_str()),
            DiscussionTarget::Plan { .. } => return ReviewDecisionOutcome::TargetMismatch,
        };
        let proposed_group = decision.group_id.as_deref();
        if proposed_group.is_some() && proposed_group != focused_group {
            return ReviewDecisionOutcome::TargetMismatch;
        }
        if let Some(group_id) = focused_group {
            if !session.groups.iter().any(|group| group.id == group_id) {
                return ReviewDecisionOutcome::MissingGroup;
            }
            if !session
                .find_item(decision.item_id)
                .is_some_and(|item| finding_in_group(item, group_id))
            {
                return ReviewDecisionOutcome::MissingFinding;
            }
        }
        if let Some(group_id) = proposed_group {
            if decision.note.trim().is_empty() {
                return ReviewDecisionOutcome::EmptyGroupNote;
            }
            if !apply_group_decision(session, group_id, decision) {
                return ReviewDecisionOutcome::MissingGroup;
            }
        } else if update_item(session, decision.item_id, |issue| {
            issue.apply_decision(decision);
        })
        .is_none()
        {
            return ReviewDecisionOutcome::MissingFinding;
        }
        ReviewDecisionOutcome::Saved
    }
}

pub(super) fn is_open_finding(status: IssueStatus) -> bool {
    matches!(
        status,
        IssueStatus::Discussion | IssueStatus::Todo | IssueStatus::InProgress
    )
}

pub(super) fn record_resolution(issue: &mut ReviewIssue, status: IssueStatus, note: &str) {
    issue.status = status;
    let note = note.trim();
    if !note.is_empty()
        && issue
            .note
            .as_deref()
            .is_none_or(|text| text.trim().is_empty())
    {
        issue.note = Some(note.to_string());
    }
}

fn update_item(
    session: &mut ReviewSession,
    item_id: u32,
    updater: impl FnOnce(&mut ReviewIssue),
) -> Option<ReviewIssue> {
    let item = session.items.iter_mut().find(|item| item.id == item_id)?;
    let mut issue = ReviewIssue::from(item.clone());
    updater(&mut issue);
    *item = ReviewItem::from(issue.clone());
    Some(issue)
}

#[cfg(test)]
mod tests {
    use crate::discussion::DiscussionTarget;
    use crate::review::{
        IssueStatus, IssueType, ReviewDecisionStatus, ReviewItem, ReviewSession, ReviewSeverity,
        ReviewUpdateProposal,
    };

    use super::{ReviewDecisionOutcome, ReviewStorage};

    /// Commit recorded on the fixture finding, standing in for the hash the
    /// executor records once it resolves a finding.
    const RESOLVED_COMMIT: &str = "9a8b7c6d5e4f30211203f4e5d6c7b8a99f0e1d2c";

    fn focus() -> DiscussionTarget {
        DiscussionTarget::Review {
            id: "rev-decision".to_string(),
        }
    }

    /// Builds a session whose single finding already carries a recommendation.
    fn session_with_finding() -> ReviewSession {
        ReviewSession {
            id: "rev-decision".to_string(),
            title: "Decision Audit".to_string(),
            created_at: 1,
            items: vec![ReviewItem {
                id: 1,
                title: "Finding".to_string(),
                uuid: None,
                commit: Some(RESOLVED_COMMIT.to_string()),
                group: None,
                model: None,
                issue_type: IssueType::Bug,
                severity: ReviewSeverity::Critical,
                status: IssueStatus::Discussion,
                recommendations: vec!["Guard the await".to_string()],
                files: Vec::new(),
                readonly_files: Vec::new(),
                resolution_notes: None,
                body: String::new(),
            }],
            ..ReviewSession::default()
        }
    }

    #[test]
    fn a_decision_updates_the_status_the_note_and_the_recommendations() {
        let root = crate::test_paths::scratch_dir("review-decision-apply");
        ReviewStorage::save_session(&root, &session_with_finding()).expect("save the session");

        let decision = ReviewUpdateProposal {
            item_id: 1,
            group_id: None,
            status: ReviewDecisionStatus::Rejected,
            note: "  The guard is released after the await  ".to_string(),
            recommendations: vec![
                "Guard the await".to_string(),
                "Document the ordering".to_string(),
            ],
        };
        assert_eq!(
            ReviewStorage::apply_decision(&root, &focus(), &decision).unwrap(),
            ReviewDecisionOutcome::Saved
        );

        let stored = ReviewStorage::load_review(&root, "rev-decision")
            .expect("load the session")
            .expect("the session is stored");
        let stored = stored.find_item(1).expect("the finding is stored");
        assert_eq!(stored.status, IssueStatus::Rejected);
        assert_eq!(
            stored.commit.as_deref(),
            Some(RESOLVED_COMMIT),
            "the recorded commit is persisted with the finding"
        );
        assert_eq!(
            stored.resolution_notes.as_deref(),
            Some("The guard is released after the await")
        );
        assert_eq!(
            stored.recommendations,
            ["Guard the await", "Document the ordering"]
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_decision_that_names_no_stored_finding_is_reported() {
        let root = crate::test_paths::scratch_dir("review-decision-absent");
        ReviewStorage::save_session(&root, &session_with_finding()).expect("save the session");

        let decision = ReviewUpdateProposal {
            item_id: 9,
            group_id: None,
            status: ReviewDecisionStatus::Todo,
            note: String::new(),
            recommendations: Vec::new(),
        };
        let applied =
            ReviewStorage::apply_decision(&root, &focus(), &decision).expect("apply the decision");

        assert_eq!(
            applied,
            ReviewDecisionOutcome::MissingFinding,
            "an absent finding is reported, never invented"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn a_decision_only_changes_the_explicitly_named_session() {
        let root = crate::test_paths::scratch_dir("review-decision-session-scope");
        let first = session_with_finding();
        let mut second = session_with_finding();
        second.id = "rev-other".to_string();
        ReviewStorage::save_session(&root, &first).unwrap();
        ReviewStorage::save_session(&root, &second).unwrap();

        let decision = ReviewUpdateProposal {
            item_id: 1,
            group_id: None,
            status: ReviewDecisionStatus::Rejected,
            note: "Not actionable".to_string(),
            recommendations: Vec::new(),
        };
        let applied = ReviewStorage::apply_decision(&root, &focus(), &decision).unwrap();
        assert_eq!(applied, ReviewDecisionOutcome::Saved);
        assert_eq!(
            ReviewStorage::load_review(&root, "rev-other")
                .unwrap()
                .unwrap()
                .find_item(1)
                .unwrap()
                .status,
            IssueStatus::Discussion
        );

        let _ = std::fs::remove_dir_all(&root);
    }
    /// The commit an executor records on a finding must survive a reload, so a
    /// finished finding reports the exact commit that resolved it.
    #[test]
    fn a_commit_recorded_by_the_executor_is_persisted_with_the_finding() {
        let root = crate::test_paths::scratch_dir("review-decision-commit");
        ReviewStorage::save_session(&root, &session_with_finding()).expect("save the session");

        let commit = "0123456789abcdef0123456789abcdef01234567";
        let updated = ReviewStorage::update_session_item(&root, "rev-decision", 1, |issue| {
            issue.commit = Some(commit.to_string());
        })
        .expect("record the commit")
        .expect("the finding is present");
        assert_eq!(updated.commit.as_deref(), Some(commit));

        // The commit reaches the session file, so a later read reports the exact
        // commit that resolved the finding instead of an anonymous fixed status.
        let session_file = ReviewStorage::reviews_dir(&root).join("rev-decision.json");
        let json = std::fs::read_to_string(&session_file).expect("read the session file");
        assert!(json.contains(commit), "the commit is stored: {json}");

        let stored = ReviewStorage::load_review(&root, "rev-decision")
            .expect("load the session")
            .expect("the session is stored");
        let stored = stored.find_item(1).expect("the finding is stored");
        assert_eq!(stored.commit.as_deref(), Some(commit));

        let _ = std::fs::remove_dir_all(&root);
    }
}
