//! Client-side review grouping, counters, and status cycling.
#[cfg(test)]
use tauqe_protocol::ModelRef;
use tauqe_protocol::{
    IssueStatus, PlanImplementation, ReviewGroup, ReviewGroupKind, ReviewIssue, ReviewItem,
    ReviewItemStatus, ReviewSession, DEFAULT_REVIEW_GROUP_ID,
};
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSummary {
    id: String,
    title: String,
    description: Option<String>,
    created_at: u64,
    pub model: ModelRef,
    /// Actionable findings, excluding the terminal `Skipped` and `Rejected`
    /// outcomes (see [`ReviewProjection::stats`]).
    pub total_items: usize,
    /// Findings resolved as `Fixed`.
    pub fixed_items: usize,
}

pub trait ReviewStatusProjection {
    fn next(&self) -> Self;
}
impl ReviewStatusProjection for IssueStatus {
    fn next(&self) -> Self {
        match self {
            Self::Discussion => Self::Todo,
            Self::Todo => Self::InProgress,
            Self::InProgress => Self::Fixed,
            Self::Fixed => Self::Skipped,
            Self::Skipped => Self::Rejected,
            Self::Rejected => Self::Discussion,
        }
    }
}
pub trait ReviewProjection {
    fn effective_groups(&self) -> Vec<ReviewGroup>;
    #[cfg(test)]
    fn summary(&self) -> ReviewSummary;
    fn stats(&self) -> (usize, usize);
    fn find_item(&self, item_id: u32) -> Option<&ReviewItem>;
    fn issues(&self) -> Vec<ReviewIssue>;
}
impl ReviewProjection for ReviewSession {
    fn effective_groups(&self) -> Vec<ReviewGroup> {
        if self.groups.is_empty() {
            return vec![ReviewGroup::single_default(self.target_files.clone())];
        }
        self.groups.clone()
    }
    #[cfg(test)]
    fn summary(&self) -> ReviewSummary {
        let (total, fixed) = self.stats();
        ReviewSummary {
            id: self.id.clone(),
            title: if self.title.trim().is_empty() {
                self.id.clone()
            } else {
                self.title.clone()
            },
            description: self.description.clone(),
            created_at: self.created_at,
            model: self.model.clone(),
            total_items: total,
            fixed_items: fixed,
        }
    }
    fn stats(&self) -> (usize, usize) {
        let total = self
            .items
            .iter()
            .filter(|item| {
                !matches!(
                    item.status,
                    ReviewItemStatus::Skipped | ReviewItemStatus::Rejected
                )
            })
            .count();
        let fixed = self
            .items
            .iter()
            .filter(|i| i.status == ReviewItemStatus::Fixed)
            .count();
        (total, fixed)
    }
    fn find_item(&self, item_id: u32) -> Option<&ReviewItem> {
        self.items.iter().find(|i| i.id == item_id)
    }
    fn issues(&self) -> Vec<ReviewIssue> {
        self.items.iter().cloned().map(ReviewIssue::from).collect()
    }
}
pub trait ReviewGroupProjection {
    fn single_default(paths: Vec<String>) -> Self;
}
impl ReviewGroupProjection for ReviewGroup {
    fn single_default(paths: Vec<String>) -> Self {
        Self {
            id: DEFAULT_REVIEW_GROUP_ID.to_string(),
            title: "Default".to_string(),
            paths,
            prompt: None,
            description: None,
            blocked_reason: None,
            resolution_notes: None,
            kind: ReviewGroupKind::Simple,
            implementation: PlanImplementation::default(),
        }
    }
}
