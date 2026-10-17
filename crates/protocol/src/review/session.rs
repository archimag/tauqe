use serde::{Deserialize, Serialize};

use crate::model::{deserialize_model_ref_lenient, ModelRef};

use super::item::{ReviewIssue, ReviewItem, ReviewItemStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewSession {
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub created_at: u64,
    #[serde(default, deserialize_with = "deserialize_model_ref_lenient")]
    pub model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_files: Vec<String>,
    #[serde(default, alias = "issues", skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ReviewItem>,
    #[serde(default)]
    pub raw_markdown: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: u64,
    #[serde(default, deserialize_with = "deserialize_model_ref_lenient")]
    pub model: ModelRef,
    /// Actionable findings, excluding the terminal `Skipped` and `Rejected`
    /// outcomes (see [`ReviewSession::stats`]).
    pub total_items: usize,
    /// Findings resolved as `Fixed`.
    pub fixed_items: usize,
}

impl ReviewSession {
    pub fn summary(&self) -> ReviewSummary {
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

    /// Counts actionable findings and how many of them are resolved as `Fixed`,
    /// returned as `(total, fixed)`.
    ///
    /// A `Skipped` or `Rejected` finding is a terminal, non-actionable outcome:
    /// it was already addressed, is not reproducible, or was dismissed as a
    /// false positive or an accepted trade-off. Both statuses are excluded from
    /// `total`, mirroring how [`crate::plan::count_leaf_stats`] drops
    /// `Cancelled` leaves from a plan's progress metric. A session whose
    /// remaining findings are all resolved therefore reports `fixed == total`
    /// (or `0/0` when nothing is actionable) instead of a fraction that can
    /// never reach one, so every client renders the same, reachable progress.
    pub fn stats(&self) -> (usize, usize) {
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

    pub fn find_item(&self, item_id: u32) -> Option<&ReviewItem> {
        self.items.iter().find(|i| i.id == item_id)
    }

    pub fn find_item_mut(&mut self, item_id: u32) -> Option<&mut ReviewItem> {
        self.items.iter_mut().find(|i| i.id == item_id)
    }

    /// Access session findings as a collection of domain [`ReviewIssue`] instances.
    pub fn issues(&self) -> Vec<ReviewIssue> {
        self.items.iter().cloned().map(ReviewIssue::from).collect()
    }
}
