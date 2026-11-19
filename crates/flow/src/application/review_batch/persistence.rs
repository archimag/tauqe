//! Persistence of step progress for review batch members.

use std::path::Path;
use tauqe_core::control::{plan::step::StepPersistence, SystemError};
use tauqe_core::plan::{Plan, PlanItemStatus};
use tauqe_core::review::IssueStatus;
use tauqe_core::review::ReviewStorage;
use tauqe_core::workflow::implementation::ExecutionScope;

/// Persistence of the remediation horizon owned by one review group.
///
/// A review group fixes one root cause through one coherent change set, so its
/// findings share a single sequence of steps. That horizon is a plain
/// implementation of steps, which is why it runs through the same sequence loop
/// as a local plan: only the document that records the lifecycle differs, and it
/// is the review session that owns the group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewGroupPersistence {
    /// Identifier of the review session that owns the group.
    review_id: String,
    /// Identifier of the group whose horizon is executed.
    group_id: String,
}

impl ReviewGroupPersistence {
    /// Addresses the horizon of one review group.
    fn new(review_id: impl Into<String>, group_id: impl Into<String>) -> Self {
        Self {
            review_id: review_id.into(),
            group_id: group_id.into(),
        }
    }
}

impl StepPersistence for ReviewGroupPersistence {
    fn load(&self, repo_root: &Path) -> Result<Plan, SystemError> {
        let session = ReviewStorage::load_review(repo_root, &self.review_id)
            .map_err(|err| {
                SystemError::Other(format!(
                    "Failed to load review session '{}': {}",
                    self.review_id, err
                ))
            })?
            .ok_or_else(|| {
                SystemError::Other(format!("Review session '{}' not found", self.review_id))
            })?;
        let group = session
            .groups
            .iter()
            .find(|group| group.id == self.group_id)
            .ok_or_else(|| {
                SystemError::Other(format!(
                    "Review session '{}' declares no group '{}'",
                    self.review_id, self.group_id
                ))
            })?;

        // The step loop reads a document, and the horizon of a group is exactly
        // that: the steps it declares. The header the loop never reads carries
        // the identity of the group the steps belong to.
        Ok(Plan::from_parts(
            session.id.clone(),
            group.title.clone(),
            group.description.clone(),
            0,
            0,
            Vec::new(),
            group.implementation.items.clone(),
        ))
    }

    fn persist(&self, repo_root: &Path, plan: &Plan) -> Result<(), SystemError> {
        let updated = ReviewStorage::update_group_subplan(
            repo_root,
            &self.review_id,
            &self.group_id,
            plan.implementation.items.clone(),
        )
        .map_err(|err| {
            SystemError::Other(format!(
                "Failed to persist the horizon of review group '{}' in session '{}': {}",
                self.group_id, self.review_id, err
            ))
        })?;

        match updated {
            Some(_) => Ok(()),
            None => Err(SystemError::Other(format!(
                "Review session '{}' declares no group '{}'",
                self.review_id, self.group_id
            ))),
        }
    }
}

/// Persistence of the degenerate one-step horizon a single review finding owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFindingPersistence {
    /// Identifier of the review session that owns the finding.
    review_id: String,
    /// Audit ordinal of the executed finding.
    item_id: u32,
}

impl ReviewFindingPersistence {
    /// Addresses the degenerate horizon of one review finding.
    fn new(review_id: impl Into<String>, item_id: u32) -> Self {
        Self {
            review_id: review_id.into(),
            item_id,
        }
    }
}

impl StepPersistence for ReviewFindingPersistence {
    fn load(&self, repo_root: &Path) -> Result<Plan, SystemError> {
        let scope = ExecutionScope::ReviewFinding {
            review_id: self.review_id.clone(),
            item_id: self.item_id,
        };
        scope
            .resolve(repo_root)
            .map(|frame| frame.plan)
            .map_err(|failure| SystemError::Other(failure.into_reason()))
    }

    fn persist(&self, repo_root: &Path, plan: &Plan) -> Result<(), SystemError> {
        let step_id = self.item_id.to_string();
        let Some(step) = plan.implementation.find_item(&step_id) else {
            return Err(SystemError::Other(format!(
                "The degenerate horizon of review finding #{} does not carry the step '{}'",
                self.item_id, step_id
            )));
        };
        let commit = step.commit.clone();
        let persisted = match step.status {
            PlanItemStatus::InProgress => ReviewStorage::update_session_item_status(
                repo_root,
                &self.review_id,
                self.item_id,
                IssueStatus::InProgress,
            ),
            PlanItemStatus::Todo => ReviewStorage::update_session_item_status(
                repo_root,
                &self.review_id,
                self.item_id,
                IssueStatus::Todo,
            ),
            // A completed step records the commit of the change set that carried
            // it. The verdict of the finding is never written here: it is stated
            // exactly once, by the settlement of the run that addressed it.
            PlanItemStatus::Done => ReviewStorage::update_session_item(
                repo_root,
                &self.review_id,
                self.item_id,
                |issue| {
                    if let Some(commit) = commit
                        .as_deref()
                        .map(str::trim)
                        .filter(|hash| !hash.is_empty())
                    {
                        issue.commit = Some(commit.to_string());
                    }
                },
            ),
            PlanItemStatus::Blocked | PlanItemStatus::Discussion | PlanItemStatus::Cancelled => {
                return Ok(())
            }
        };
        persisted.map_err(|err| {
            SystemError::Other(format!(
                "Failed to persist the lifecycle of review finding #{}: {}",
                self.item_id, err
            ))
        })?;
        Ok(())
    }
}

/// Persistence of the horizon a review batch member executes.
///
/// A member addresses either the unified horizon of an architectural group or
/// the degenerate horizon of a single finding, so the adapter selects the
/// storage that owns the addressed entity and the step loop stays unaware of
/// which one it drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewMemberPersistence {
    /// The unified horizon of an architectural review group.
    Group(ReviewGroupPersistence),
    /// The degenerate one-step horizon of a single review finding.
    Finding(ReviewFindingPersistence),
}

impl ReviewMemberPersistence {
    /// Selects the persistence of the review entity a scope addresses.
    pub fn of(scope: &ExecutionScope) -> Option<Self> {
        match scope {
            ExecutionScope::ReviewGroup {
                review_id,
                group_id,
            } => Some(Self::Group(ReviewGroupPersistence::new(
                review_id.as_str(),
                group_id.as_str(),
            ))),
            ExecutionScope::ReviewFinding { review_id, item_id } => Some(Self::Finding(
                ReviewFindingPersistence::new(review_id.as_str(), *item_id),
            )),
            ExecutionScope::Plan { .. } => None,
        }
    }
}

impl StepPersistence for ReviewMemberPersistence {
    fn load(&self, repo_root: &Path) -> Result<Plan, SystemError> {
        match self {
            Self::Group(group) => group.load(repo_root),
            Self::Finding(finding) => finding.load(repo_root),
        }
    }

    fn persist(&self, repo_root: &Path, plan: &Plan) -> Result<(), SystemError> {
        match self {
            Self::Group(group) => group.persist(repo_root, plan),
            Self::Finding(finding) => finding.persist(repo_root, plan),
        }
    }
}
