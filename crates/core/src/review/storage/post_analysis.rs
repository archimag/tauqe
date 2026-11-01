use super::ReviewStorage;
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use tauqe_protocol::{IssueStatus, ReviewGroup, ReviewIssue, ReviewItem, ReviewSession};

/// Placement of one finding in the layer a post-analysis produces.
///
/// Post-analysis owns the whole layer, so it decides both the group that now
/// owns a finding and the lifecycle status its resolution starts from. A status
/// the classification does not decide stays `None`, and the finding keeps the
/// one it already stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingAssignment {
    /// Identity of the finding, exactly as the session stores it.
    pub uuid: String,
    /// Group the finding now belongs to.
    pub group_id: String,
    /// Lifecycle status the finding is routed to, or `None` when the routing
    /// leaves the stored status unchanged.
    pub status: Option<IssueStatus>,
}
impl ReviewStorage {
    /// Regroups the explicitly named review session in place.
    ///
    /// Post-analysis owns no projection beside the session: it rewrites the
    /// session itself. The global verdict becomes the session description, the
    /// classification replaces `groups`, and every finding moves to the group and
    /// to the lifecycle status its routing decided. The findings keep their
    /// identity and their content, and the audited code is never rescanned, so a
    /// repeated analysis substitutes only the layer of the previous one.
    ///
    /// `assignments` is keyed by the identity the session stores. A finding whose
    /// identity is not named keeps the group and the status it already has, so a
    /// partial classification never strands a finding outside the group list.
    pub fn regroup_session(
        repo_root: &Path,
        review_id: &str,
        verdict: String,
        groups: Vec<ReviewGroup>,
        assignments: Vec<FindingAssignment>,
    ) -> Result<Option<ReviewSession>> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::regroup_session_unlocked(repo_root, review_id, verdict, groups, assignments)
        })
    }

    /// Lock-free core of [`Self::regroup_session`]; the caller must hold the
    /// storage lock.
    fn regroup_session_unlocked(
        repo_root: &Path,
        review_id: &str,
        verdict: String,
        groups: Vec<ReviewGroup>,
        assignments: Vec<FindingAssignment>,
    ) -> Result<Option<ReviewSession>> {
        let mut session = match Self::load_review_unlocked(repo_root, review_id)? {
            Some(session) => session,
            None => return Ok(None),
        };

        let assigned: HashMap<String, FindingAssignment> = assignments
            .into_iter()
            .map(|assignment| (assignment.uuid.clone(), assignment))
            .collect();
        for item in &mut session.items {
            // The classification references the identity the session stores, so
            // the lookup needs no normalization here.
            let Some(uuid) = item.uuid.clone() else {
                continue;
            };
            let Some(assignment) = assigned.get(&uuid) else {
                continue;
            };
            item.group = Some(assignment.group_id.clone());
            // The group and the routing describe the same finding, so the single
            // write that persists the layer persists both.
            if let Some(status) = assignment.status {
                item.status = status;
            }
        }

        session.description = Some(verdict);
        session.groups = groups;
        Self::save_session_unlocked(repo_root, &session)?;

        Ok(Some(session))
    }

    /// Appends the findings one review group produced to the named session.
    ///
    /// Every appended finding is stamped with a freshly minted UUIDv7 identity
    /// and the identifier of the group that produced it: the identity is what
    /// the post-analysis layer references, and the provenance is what the
    /// grouped session view renders. Findings are appended in order and the
    /// items already stored are never modified, so a group that fails or is
    /// cancelled withholds only its own findings instead of invalidating the
    /// results the other groups already produced.
    pub fn append_group_findings(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        findings: Vec<ReviewIssue>,
    ) -> Result<Option<ReviewSession>> {
        crate::blocking::run_blocking(move || {
            let _guard = Self::lock();
            Self::append_group_findings_unlocked(repo_root, review_id, group_id, findings)
        })
    }

    /// Lock-free core of [`Self::append_group_findings`]; the caller must hold
    /// the storage lock.
    fn append_group_findings_unlocked(
        repo_root: &Path,
        review_id: &str,
        group_id: &str,
        findings: Vec<ReviewIssue>,
    ) -> Result<Option<ReviewSession>> {
        let mut session = match Self::load_review_unlocked(repo_root, review_id)? {
            Some(session) => session,
            None => return Ok(None),
        };

        for mut finding in findings {
            finding.uuid = Some(crate::review::uuid::generate_uuid_v7());
            finding.group = Some(group_id.to_string());
            session.items.push(ReviewItem::from(finding));
        }

        Self::save_session_unlocked(repo_root, &session)?;

        Ok(Some(session))
    }
}
