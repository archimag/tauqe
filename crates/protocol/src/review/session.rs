use serde::{Deserialize, Serialize};

use crate::model::{deserialize_model_ref_lenient, ModelRef};

use super::group::{ReviewGroup, ReviewLaunchConfig, ReviewPostAnalysis};
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
    /// Groups this session audits, in launch order.
    ///
    /// Empty for a session written before grouping existed and for a
    /// single-audit run; [`Self::effective_groups`] resolves both cases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ReviewGroup>,
    /// Classification produced by the Architect role over the whole session.
    ///
    /// Replacing it discards only the previous classification: the findings it
    /// references are immutable and are never rescanned to rebuild this layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_analysis: Option<ReviewPostAnalysis>,
    /// Snapshot of the configuration that launched this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_config: Option<ReviewLaunchConfig>,
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
    /// The groups this session actually audits.
    ///
    /// A session created before grouping existed, or one launched as a single
    /// audit, carries no explicit groups: it is the degenerate case of one
    /// implicit `Default` group covering the session target files. Synthesizing
    /// it here keeps every consumer on one code path instead of branching on
    /// "legacy or grouped".
    pub fn effective_groups(&self) -> Vec<ReviewGroup> {
        if self.groups.is_empty() {
            return vec![ReviewGroup::single_default(self.target_files.clone())];
        }
        self.groups.clone()
    }

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
    ///
    /// The conversion is lossless: the stored semantic category is carried
    /// through unchanged, so `Bug`/`Security` and `Architecture`/`Performance`
    /// never collapse into their coarser severity classes when a session is
    /// read back.
    pub fn issues(&self) -> Vec<ReviewIssue> {
        self.items.iter().cloned().map(ReviewIssue::from).collect()
    }
}
