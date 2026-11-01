use anyhow::Result;
use std::path::Path;

use tauqe_protocol::review::ReviewUpdateProposal;
use tauqe_protocol::{IssueStatus, ReviewIssue, ReviewItem};

use super::ReviewStorage;

impl ReviewStorage {
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

        let updated_issue = if let Some(item) = session.items.iter_mut().find(|it| it.id == item_id)
        {
            let mut issue = ReviewIssue::from(item.clone());
            updater(&mut issue);
            *item = ReviewItem::from(issue.clone());
            Some(issue)
        } else {
            None
        };

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

    /// Applies a decision recorded in a dialogue for one finding.
    ///
    /// The decision is the engineer's own resolution of the finding, so it
    /// carries the status the finding moves to, the reasoning behind it, and the
    /// remediation agreed during the discussion. The note is stored under the
    /// canonical `note` wire name shared with the review DTOs, and a
    /// recommendation the finding already carries is never duplicated. A finding
    /// the named session does not hold is reported as absent instead of being
    /// invented, so a decision can never fabricate a finding.
    pub fn apply_decision(
        repo_root: &Path,
        review_id: &str,
        decision: &ReviewUpdateProposal,
    ) -> Result<Option<ReviewIssue>> {
        Self::update_session_item(repo_root, review_id, decision.item_id, |issue| {
            issue.status = decision.status.into();
            let note = decision.note.trim();
            if !note.is_empty() {
                issue.note = Some(note.to_string());
            }
            for recommendation in &decision.recommendations {
                let trimmed = recommendation.trim();
                if !trimmed.is_empty()
                    && !issue
                        .recommendations
                        .iter()
                        .any(|existing| existing == trimmed)
                {
                    issue.recommendations.push(trimmed.to_string());
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use tauqe_protocol::review::{
        IssueStatus, IssueType, ReviewDecisionStatus, ReviewItem, ReviewSession, ReviewSeverity,
        ReviewUpdateProposal,
    };

    use super::ReviewStorage;

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

    /// Returns a scratch repository root that is unique to this test process.
    fn scratch_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "tauqe-review-decision-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch root");
        root
    }

    #[test]
    fn a_decision_updates_the_status_the_note_and_the_recommendations() {
        let root = scratch_root("apply");
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
        let updated = ReviewStorage::apply_decision(&root, "rev-decision", &decision)
            .expect("apply the decision")
            .expect("the finding is present");

        assert_eq!(updated.status, IssueStatus::Rejected);
        assert_eq!(
            updated.note.as_deref(),
            Some("The guard is released after the await")
        );
        assert_eq!(
            updated.recommendations,
            vec![
                "Guard the await".to_string(),
                "Document the ordering".to_string()
            ]
        );

        let stored = ReviewStorage::load_review(&root, "rev-decision")
            .expect("load the session")
            .expect("the session is stored");
        let stored = stored.find_item(1).expect("the finding is stored");
        assert_eq!(stored.status, IssueStatus::Rejected);
        assert_eq!(
            stored.resolution_notes.as_deref(),
            Some("The guard is released after the await")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_decision_that_names_no_stored_finding_is_reported() {
        let root = scratch_root("absent");
        ReviewStorage::save_session(&root, &session_with_finding()).expect("save the session");

        let decision = ReviewUpdateProposal {
            item_id: 9,
            group_id: None,
            status: ReviewDecisionStatus::Todo,
            note: String::new(),
            recommendations: Vec::new(),
        };
        let applied = ReviewStorage::apply_decision(&root, "rev-decision", &decision)
            .expect("the write is attempted");

        assert!(
            applied.is_none(),
            "an absent finding is reported, never invented"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
