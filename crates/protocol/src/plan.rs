use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanItemStatus {
    #[default]
    Todo,
    InProgress,
    Done,
    Cancelled,
}

impl PlanItemStatus {
    pub fn next(&self) -> Self {
        match self {
            Self::Todo => Self::InProgress,
            Self::InProgress => Self::Done,
            Self::Done => Self::Cancelled,
            Self::Cancelled => Self::Todo,
        }
    }
}

impl std::fmt::Display for PlanItemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Todo => write!(f, "TODO"),
            Self::InProgress => write!(f, "IN_PROGRESS"),
            Self::Done => write!(f, "DONE"),
            Self::Cancelled => write!(f, "CANCELLED"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanItem {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(default)]
    pub status: PlanItemStatus,
    #[serde(default)]
    pub checked: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<PlanItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Plan {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PlanItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub total_items: usize,
    pub completed_items: usize,
    pub checked_items: usize,
}

impl Plan {
    pub fn summary(&self) -> PlanSummary {
        let stats = self.stats();
        PlanSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            total_items: stats.total,
            completed_items: stats.completed,
            checked_items: stats.checked,
        }
    }

    pub fn stats(&self) -> PlanStats {
        let (total, completed, checked) = self.count_stats();
        PlanStats {
            total,
            completed,
            checked,
        }
    }

    pub fn count_stats(&self) -> (usize, usize, usize) {
        fn walk(items: &[PlanItem], total: &mut usize, completed: &mut usize, checked: &mut usize) {
            for item in items {
                *total += 1;
                if item.status == PlanItemStatus::Done {
                    *completed += 1;
                }
                if item.checked {
                    *checked += 1;
                }
                walk(&item.children, total, completed, checked);
            }
        }
        let mut total = 0;
        let mut completed = 0;
        let mut checked = 0;
        walk(&self.items, &mut total, &mut completed, &mut checked);
        (total, completed, checked)
    }

    pub fn toggle_item_checked(&mut self, item_id: &str) -> bool {
        fn walk(items: &mut [PlanItem], item_id: &str) -> Option<bool> {
            for item in items {
                if item.id == item_id {
                    item.checked = !item.checked;
                    return Some(item.checked);
                }
                if let Some(res) = walk(&mut item.children, item_id) {
                    return Some(res);
                }
            }
            None
        }
        walk(&mut self.items, item_id).unwrap_or(false)
    }

    pub fn update_item_status(&mut self, item_id: &str, status: PlanItemStatus) -> bool {
        fn walk(items: &mut [PlanItem], item_id: &str, status: PlanItemStatus) -> bool {
            for item in items {
                if item.id == item_id {
                    item.status = status;
                    return true;
                }
                if walk(&mut item.children, item_id, status) {
                    return true;
                }
            }
            false
        }
        walk(&mut self.items, item_id, status)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanStats {
    pub total: usize,
    pub completed: usize,
    pub checked: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanGetParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanGetResult {
    pub plan: Option<Plan>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanListResult {
    pub plans: Vec<PlanSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSaveParams {
    pub plan: Plan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanUpdateItemParams {
    pub plan_id: String,
    pub item_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<PlanItemStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanUpdateItemResult {
    pub plan: Plan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDeleteParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanSetActiveParams {
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanSetActiveResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanUpdatedEvent {
    pub plan: Plan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanListChangedEvent {
    pub plans: Vec<PlanSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_item_status_lifecycle() {
        let status = PlanItemStatus::Todo;
        assert_eq!(status.next(), PlanItemStatus::InProgress);
        assert_eq!(status.next().next(), PlanItemStatus::Done);
        assert_eq!(status.next().next().next(), PlanItemStatus::Cancelled);
        assert_eq!(status.next().next().next().next(), PlanItemStatus::Todo);
        assert_eq!(status.to_string(), "TODO");
    }

    #[test]
    fn test_plan_item_serde() {
        let item = PlanItem {
            id: "1.1".to_string(),
            title: "Define claims".to_string(),
            details: Some("Claims struct with exp and sub".to_string()),
            status: PlanItemStatus::InProgress,
            checked: true,
            children: vec![],
        };

        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"in_progress\""));
        assert!(json.contains("\"checked\":true"));

        let de: PlanItem = serde_json::from_str(&json).unwrap();
        assert_eq!(de, item);
    }

    #[test]
    fn test_plan_stats_and_summary() {
        let plan = Plan {
            id: "jwt-auth".to_string(),
            title: "JWT Authentication".to_string(),
            description: Some("Replace session cookies".to_string()),
            created_at: 1700000000,
            updated_at: 1700000010,
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Models".to_string(),
                    details: None,
                    status: PlanItemStatus::Done,
                    checked: false,
                    children: vec![
                        PlanItem {
                            id: "1.1".to_string(),
                            title: "Claims".to_string(),
                            details: None,
                            status: PlanItemStatus::Done,
                            checked: true,
                            children: vec![],
                        },
                    ],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Endpoint".to_string(),
                    details: None,
                    status: PlanItemStatus::InProgress,
                    checked: true,
                    children: vec![],
                },
            ],
        };

        let (total, completed, checked) = plan.count_stats();
        assert_eq!(total, 3);
        assert_eq!(completed, 2);
        assert_eq!(checked, 2);

        let summary = plan.summary();
        assert_eq!(summary.total_items, 3);
        assert_eq!(summary.completed_items, 2);
        assert_eq!(summary.checked_items, 2);
        assert_eq!(summary.id, "jwt-auth");
    }
}
