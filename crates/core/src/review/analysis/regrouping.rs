//! Validated replacement of a review's classification and routing layer.

use super::{validate_post_analysis, PostAnalysisOutput};
use crate::review::storage::FindingAssignment;
use crate::review::{uuid::generate_uuid_v7, ReviewStorage};
use crate::review::{IssueStatus, IssueType, ReviewSession};
use crate::review::{
    ReviewGroup, ReviewGroupKind, LOCAL_REVIEW_GROUP_ID, LOCAL_REVIEW_GROUP_TITLE,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Classification refusal, independent of RPC error codes.
#[derive(Debug, thiserror::Error)]
pub enum PostAnalysisRegroupingError {
    #[error("Review post-analysis returned no summary; the global verdict is required by the classification schema")]
    MissingSummary,
    #[error("{0}")]
    InvalidReference(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegroupingStatistics {
    pub independent_issues: usize,
    pub architectural_groups: usize,
    pub routed_todo: usize,
    pub routed_discussion: usize,
}

/// A validated classification bound to the session it was prepared for.
/// Groups and assignments stay private so persistence cannot mix two sessions.
pub struct PostAnalysisRegrouping {
    review_id: String,
    verdict: String,
    groups: Vec<ReviewGroup>,
    assignments: Vec<FindingAssignment>,
    statistics: RegroupingStatistics,
    history_response: String,
}

impl PostAnalysisRegrouping {
    /// Validate references and compile deterministic groups and lifecycle routing.
    /// The caller heals identities before presenting findings to the model.
    pub fn prepare(
        session: &ReviewSession,
        output: &PostAnalysisOutput,
    ) -> Result<Self, PostAnalysisRegroupingError> {
        if output.summary.trim().is_empty() {
            return Err(PostAnalysisRegroupingError::MissingSummary);
        }
        let known_uuids = finding_identities(session);
        validate_post_analysis(output, &known_uuids)
            .map_err(PostAnalysisRegroupingError::InvalidReference)?;
        let verdict = output.summary.trim().to_string();
        let redirects = arch_redirect_groups(output, session, &known_uuids);
        let groups = regrouped_groups(output, &redirects);
        let triage = triage_statuses(output, &known_uuids);
        let assignments = group_assignments(output, &known_uuids, session, &triage, &redirects);
        let statistics = RegroupingStatistics {
            independent_issues: output.independent_issues.len(),
            architectural_groups: output.architectural_groups.len(),
            routed_todo: assignments
                .iter()
                .filter(|a| a.status == Some(IssueStatus::Todo))
                .count(),
            routed_discussion: assignments
                .iter()
                .filter(|a| a.status == Some(IssueStatus::Discussion))
                .count(),
        };
        let history_response = format!(
            "Review session [{}] post-analysis regrouped {} finding(s) into {} independent issue(s) and {} architectural group(s), routing {} to todo and {} to discussion.\n\n{}",
            session.id, session.items.len(), statistics.independent_issues,
            statistics.architectural_groups, statistics.routed_todo,
            statistics.routed_discussion, verdict,
        );
        Ok(Self {
            review_id: session.id.clone(),
            verdict,
            groups,
            assignments,
            statistics,
            history_response,
        })
    }

    pub fn is_for_review(&self, id: &str) -> bool {
        self.review_id == id
    }

    pub fn statistics(&self) -> RegroupingStatistics {
        self.statistics
    }

    pub fn history_response(&self) -> &str {
        &self.history_response
    }

    /// Replace only the classification layer using one storage mutation.
    /// The caller retains its operation lease and checks cancellation before saving.
    pub fn persist(self, repo_root: &Path) -> anyhow::Result<Option<ReviewSession>> {
        ReviewStorage::regroup_session(
            repo_root,
            &self.review_id,
            self.verdict,
            self.groups,
            self.assignments,
        )
    }
}

/// Collects the finding identities the session holds, in stored order.
///
/// Identity is what the classification references. Healing runs before this
/// collection, so every finding of a classified session contributes one; the
/// filter stays as a defensive guard against an identity that is present but
/// blank.
fn finding_identities(session: &ReviewSession) -> Vec<String> {
    session
        .items
        .iter()
        .filter_map(|item| item.uuid.as_deref())
        .map(str::trim)
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_string)
        .collect()
}

/// Assigns a UUIDv7 identity to every finding that carries none.
///
/// Returns the number of identities minted. A finding stored before identities
/// existed cannot be referenced by the classification, so the session is healed
/// before the model reads it instead of being rendered as an unclassifiable
/// element. An identity a finding already carries is preserved, so healing never
/// renames a finding an earlier round already referenced.
pub fn ensure_finding_identities(session: &mut ReviewSession) -> usize {
    let mut minted = 0;
    for item in &mut session.items {
        let present = item
            .uuid
            .as_deref()
            .map(str::trim)
            .is_some_and(|uuid| !uuid.is_empty());
        if !present {
            item.uuid = Some(generate_uuid_v7());
            minted += 1;
        }
    }
    minted
}

/// Resolves one emitted identifier to the exact spelling the session stores.
///
/// Comparison ignores ASCII case, so a model that re-spells an identity still
/// addresses the finding it means, while the persisted layer keeps the
/// authoritative spelling every later projection matches exactly.
fn resolve_identity(identifier: &str, known_uuids: &[String]) -> Option<String> {
    let trimmed = identifier.trim();
    known_uuids
        .iter()
        .find(|known| known.eq_ignore_ascii_case(trimmed))
        .cloned()
}

/// Resolves the lifecycle status each finding is routed to, keyed by identity.
///
/// The routing is the decision about where a finding starts, so it is keyed by
/// the authoritative spelling the session stores. An entry naming an identity the
/// session does not hold is ignored here: the payload was already validated
/// against the session by name, so a stray entry is not a reason to abandon the
/// layer.
fn triage_statuses(
    output: &PostAnalysisOutput,
    known_uuids: &[String],
) -> HashMap<String, IssueStatus> {
    let mut statuses = HashMap::new();
    for entry in &output.triage {
        if let Some(known) = resolve_identity(&entry.uuid, known_uuids) {
            statuses.insert(known, entry.status);
        }
    }
    statuses
}

/// Statuses the post-analysis may still route a finding out of.
///
/// A finding that is already being executed, or that already reached a terminal
/// outcome, keeps the lifecycle state it earned: re-running the analysis rewrites
/// the routing of the findings that are still open and never reopens a decision
/// that was already taken.
fn triageable(status: IssueStatus) -> bool {
    matches!(status, IssueStatus::Discussion | IssueStatus::Todo)
}

/// Resolves the status one finding is routed to, if it may still be routed.
fn routed_status(
    uuid: &str,
    session: &ReviewSession,
    triage: &HashMap<String, IssueStatus>,
) -> Option<IssueStatus> {
    let item = session
        .items
        .iter()
        .find(|item| item.uuid.as_deref() == Some(uuid))?;
    if !triageable(item.status) {
        return None;
    }
    triage.get(uuid).copied()
}

/// Identifier of the architectural cluster emitted at one payload position.
///
/// A cluster has no identity outside the classification that produced it, so its
/// identifier is a deterministic position within that classification. A random
/// identifier would make two identical analyses produce two different sessions.
fn architectural_group_id(index: usize) -> String {
    format!("arch-{}", index + 1)
}

/// Materializes the groups that replace the session groups.
///
/// The classification is the session, so the payload is the group list: one group
/// per architectural cluster of the findings, one single-element group per finding
/// whose type requires planning before execution, plus the group collecting the
/// findings that need no architectural remediation. That group is materialized
/// even when it turns out empty, so a classified session keeps one uniform shape:
/// every finding belongs to exactly one rendered group.
///
/// The class of each group is declared here rather than derived from its
/// identifier: an architectural cluster and a single-element architectural group
/// become [`ReviewGroupKind::Architectural`], while the group collecting the
/// findings that share no root cause becomes [`ReviewGroupKind::Simple`], so
/// execution never guesses a class from a name.
fn regrouped_groups(
    output: &PostAnalysisOutput,
    redirects: &[(String, ReviewGroup)],
) -> Vec<ReviewGroup> {
    let mut groups: Vec<ReviewGroup> = output
        .architectural_groups
        .iter()
        .enumerate()
        .map(|(index, group)| ReviewGroup {
            id: architectural_group_id(index),
            title: group.title.trim().to_string(),
            description: Some(group.description.trim().to_string()),
            // A cluster is remediated by one shared horizon that removes the root
            // cause, never finding by finding.
            kind: ReviewGroupKind::Architectural,
            ..Default::default()
        })
        .collect();
    // A finding whose type requires planning before execution owns a
    // single-element group of its own, rendered after the clusters so the layer
    // keeps one deterministic numbering.
    for (_, group) in redirects {
        groups.push(group.clone());
    }
    groups.push(ReviewGroup {
        id: LOCAL_REVIEW_GROUP_ID.to_string(),
        title: LOCAL_REVIEW_GROUP_TITLE.to_string(),
        description: Some(
            "Findings that stand on their own: each one is resolved by its own finding subplan, with no shared group horizon."
                .to_string(),
        ),
        // The findings that share no root cause stay independent, so the group
        // collecting them is executed finding by finding.
        kind: ReviewGroupKind::Simple,
        ..Default::default()
    });
    groups
}

/// Resolves the findings whose type requires planning before execution, paired
/// with the architectural group that owns each of them.
///
/// `IssueType::Architecture` is a deterministic requirement, not a label: such a
/// finding is remediated by a plan, and the only sanctioned carrier of a plan is
/// an architectural group. The group of independent findings is a simple group,
/// so it may hold no architecture finding at all. A classification that names one
/// independent, or that does not name it at all, therefore does not decide where
/// the finding lands; the group is derived here instead, titled after the finding
/// because the finding is the whole content of its root cause.
///
/// A finding the classification placed in a cluster is already owned by an
/// architectural group and keeps it, so no group of the layer is ever empty. The
/// numbering continues after the clusters and follows the stored order of the
/// findings, so a repeated analysis of the same payload mints the same
/// identifiers.
fn arch_redirect_groups(
    output: &PostAnalysisOutput,
    session: &ReviewSession,
    known_uuids: &[String],
) -> Vec<(String, ReviewGroup)> {
    let clustered: HashSet<String> = output
        .architectural_groups
        .iter()
        .flat_map(|group| group.issue_ids.iter())
        .filter_map(|identifier| resolve_identity(identifier, known_uuids))
        .collect();
    let mut redirects = Vec::new();
    let mut index = output.architectural_groups.len();
    for item in &session.items {
        if item.issue_type != IssueType::Architecture {
            continue;
        }
        let uuid = item.uuid.as_deref().map(str::trim).unwrap_or_default();
        if uuid.is_empty() || clustered.contains(uuid) {
            continue;
        }
        redirects.push((
            uuid.to_string(),
            ReviewGroup {
                id: architectural_group_id(index),
                title: item.title.trim().to_string(),
                description: Some(
                    "The finding requires a plan before it is executed: the group synthesizes one coherent remediation of its root cause."
                        .to_string(),
                ),
                kind: ReviewGroupKind::Architectural,
                ..Default::default()
            },
        ));
        index += 1;
    }
    redirects
}

/// Resolves the group that owns every finding the session holds.
///
/// A finding the payload names belongs to the group it named. A finding the payload
/// does not name would otherwise keep pointing at a group the new layer no longer
/// declares, so it lands in the group of independent findings. An architecture
/// finding never lands there: the single-element architectural group minted for it
/// replaces the independent assignment, because the group of independent findings
/// holds no finding that must be planned before it is executed. Every assignment
/// therefore names a group of [`regrouped_groups`], and no finding is left outside
/// the rendered tree.
fn group_assignments(
    output: &PostAnalysisOutput,
    known_uuids: &[String],
    session: &ReviewSession,
    triage: &HashMap<String, IssueStatus>,
    redirects: &[(String, ReviewGroup)],
) -> Vec<FindingAssignment> {
    let mut assigned: HashMap<String, String> = HashMap::new();
    for identifier in &output.independent_issues {
        if let Some(known) = resolve_identity(identifier, known_uuids) {
            assigned.insert(known, LOCAL_REVIEW_GROUP_ID.to_string());
        }
    }
    for (index, group) in output.architectural_groups.iter().enumerate() {
        let group_id = architectural_group_id(index);
        for identifier in &group.issue_ids {
            if let Some(known) = resolve_identity(identifier, known_uuids) {
                assigned.insert(known, group_id.clone());
            }
        }
    }
    // The redirects are applied last, so an architecture finding keeps its own
    // architectural group even when the payload called it independent.
    for (uuid, group) in redirects {
        assigned.insert(uuid.clone(), group.id.clone());
    }
    known_uuids
        .iter()
        .map(|uuid| {
            let group_id = assigned
                .get(uuid)
                .cloned()
                .unwrap_or_else(|| LOCAL_REVIEW_GROUP_ID.to_string());
            FindingAssignment {
                uuid: uuid.clone(),
                group_id,
                status: routed_status(uuid, session, triage),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::analysis::{PostAnalysisGroupOutput, PostAnalysisTriageOutput};
    use crate::review::{ReviewItem, ReviewSeverity};

    const FIRST: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000001";
    const SECOND: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000002";
    const THIRD: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000003";

    fn finding(id: u32, uuid: &str, issue_type: IssueType, status: IssueStatus) -> ReviewItem {
        ReviewItem {
            id,
            title: format!("Finding {id}"),
            uuid: Some(uuid.into()),
            commit: None,
            group: Some("old-group".into()),
            model: None,
            issue_type,
            severity: ReviewSeverity::from(issue_type),
            status,
            recommendations: vec!["Keep this recommendation".into()],
            files: vec!["src/lib.rs".into()],
            readonly_files: vec![],
            resolution_notes: None,
            body: "Keep this finding body".into(),
        }
    }

    fn session() -> ReviewSession {
        ReviewSession {
            id: "regrouping-review".into(),
            items: vec![
                finding(1, FIRST, IssueType::Architecture, IssueStatus::Todo),
                finding(2, SECOND, IssueType::Architecture, IssueStatus::Discussion),
                finding(3, THIRD, IssueType::Architecture, IssueStatus::Fixed),
            ],
            ..ReviewSession::default()
        }
    }

    fn output() -> PostAnalysisOutput {
        PostAnalysisOutput {
            summary: "  Shared root cause  ".into(),
            independent_issues: vec![SECOND.to_uppercase(), THIRD.into()],
            architectural_groups: vec![PostAnalysisGroupOutput {
                title: " Cluster ".into(),
                description: " Root cause ".into(),
                issue_ids: vec![FIRST.to_uppercase()],
            }],
            triage: vec![
                PostAnalysisTriageOutput {
                    uuid: FIRST.to_uppercase(),
                    status: IssueStatus::Discussion,
                    discussion_topic: Some("Which boundary?".into()),
                },
                PostAnalysisTriageOutput {
                    uuid: SECOND.into(),
                    status: IssueStatus::Todo,
                    discussion_topic: None,
                },
                PostAnalysisTriageOutput {
                    uuid: THIRD.into(),
                    status: IssueStatus::Todo,
                    discussion_topic: None,
                },
            ],
        }
    }

    #[test]
    fn heals_only_missing_or_blank_identities_and_keeps_them_stable() {
        let mut review = session();
        review.items[0].uuid = None;
        review.items[1].uuid = Some("  ".into());
        assert_eq!(ensure_finding_identities(&mut review), 2);
        let identities = finding_identities(&review);
        assert_eq!(identities.len(), 3);
        assert!(identities
            .iter()
            .all(|id| crate::review::is_valid_uuid_v7(id)));
        assert_eq!(identities[2], THIRD);
        assert_eq!(ensure_finding_identities(&mut review), 0);
        assert_eq!(finding_identities(&review), identities);
    }

    #[test]
    fn deterministic_regrouping_preserves_findings_and_finished_statuses() {
        let review = session();
        let output = output();
        let prepared = PostAnalysisRegrouping::prepare(&review, &output).unwrap();
        let repeated = PostAnalysisRegrouping::prepare(&review, &output).unwrap();
        assert_eq!(prepared.groups, repeated.groups);
        assert_eq!(prepared.assignments, repeated.assignments);
        assert_eq!(
            prepared.statistics(),
            RegroupingStatistics {
                independent_issues: 2,
                architectural_groups: 1,
                routed_todo: 1,
                routed_discussion: 1,
            }
        );
        assert_eq!(
            prepared
                .groups
                .iter()
                .map(|g| g.id.as_str())
                .collect::<Vec<_>>(),
            ["arch-1", "arch-2", "arch-3", LOCAL_REVIEW_GROUP_ID]
        );
        assert_eq!(prepared.assignments[0].uuid, FIRST);
        assert_eq!(prepared.assignments[2].status, None);
        assert!(prepared.groups[..3]
            .iter()
            .all(|g| g.kind == ReviewGroupKind::Architectural));

        let root = tempfile::tempdir().unwrap();
        ReviewStorage::save_session(root.path(), &review).unwrap();
        let stored = prepared.persist(root.path()).unwrap().unwrap();
        assert_eq!(stored.description.as_deref(), Some("Shared root cause"));
        assert_eq!(stored.items[0].status, IssueStatus::Discussion);
        assert_eq!(stored.items[1].status, IssueStatus::Todo);
        assert_eq!(stored.items[2].status, IssueStatus::Fixed);
        for (before, after) in review.items.iter().zip(&stored.items) {
            assert_eq!(before.uuid, after.uuid);
            assert_eq!(before.body, after.body);
            assert_eq!(before.recommendations, after.recommendations);
            assert_eq!(before.files, after.files);
        }
    }

    #[test]
    fn omitted_findings_get_complete_assignments_and_typed_validation_refuses_foreign_ids() {
        let mut review = session();
        review.items[2].issue_type = IssueType::Security;
        let mut output = output();
        output.independent_issues.clear();
        output.triage.clear();
        let prepared = PostAnalysisRegrouping::prepare(&review, &output).unwrap();
        assert_eq!(prepared.assignments.len(), review.items.len());
        assert_eq!(prepared.assignments[2].group_id, LOCAL_REVIEW_GROUP_ID);
        assert!(prepared.assignments.iter().all(|a| a.status.is_none()));
        assert!(prepared
            .assignments
            .iter()
            .all(|a| prepared.groups.iter().any(|g| g.id == a.group_id)));
        output.independent_issues.push("foreign-identity".into());
        assert!(matches!(
            PostAnalysisRegrouping::prepare(&review, &output),
            Err(PostAnalysisRegroupingError::InvalidReference(_))
        ));
        output.summary = " ".into();
        assert!(matches!(
            PostAnalysisRegrouping::prepare(&review, &output),
            Err(PostAnalysisRegroupingError::MissingSummary)
        ));
    }
}
