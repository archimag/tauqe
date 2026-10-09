use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanItemStatus {
    Discussion,
    #[default]
    Todo,
    InProgress,
    Done,
    Cancelled,
}

pub type PlanStatus = PlanItemStatus;

impl PlanItemStatus {
    pub fn next(&self) -> Self {
        match self {
            Self::Discussion => Self::Todo,
            Self::Todo => Self::InProgress,
            Self::InProgress => Self::Done,
            Self::Done => Self::Cancelled,
            Self::Cancelled => Self::Discussion,
        }
    }
}

impl std::fmt::Display for PlanItemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Discussion => write!(f, "DISCUSSION"),
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<PlanItem>,
}

impl PlanItem {
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
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
    #[serde(default)]
    pub status: PlanItemStatus,
}

/// Recursively computes the aggregated status of a collection of plan items.
pub fn compute_items_status(items: &[PlanItem]) -> PlanItemStatus {
    if items.is_empty() {
        return PlanItemStatus::Todo;
    }
    let statuses: Vec<PlanItemStatus> = items
        .iter()
        .map(|item| {
            if item.is_leaf() {
                item.status
            } else {
                compute_items_status(&item.children)
            }
        })
        .collect();

    if statuses.contains(&PlanItemStatus::Discussion) {
        return PlanItemStatus::Discussion;
    }
    if statuses.contains(&PlanItemStatus::InProgress) {
        return PlanItemStatus::InProgress;
    }
    let has_done = statuses.contains(&PlanItemStatus::Done);
    let has_todo = statuses.contains(&PlanItemStatus::Todo);
    if has_done && has_todo {
        return PlanItemStatus::InProgress;
    }
    if statuses.iter().all(|s| *s == PlanItemStatus::Done) {
        return PlanItemStatus::Done;
    }
    if statuses.iter().all(|s| *s == PlanItemStatus::Cancelled) {
        return PlanItemStatus::Cancelled;
    }
    if statuses.iter().all(|s| *s == PlanItemStatus::Todo) {
        return PlanItemStatus::Todo;
    }
    let active: Vec<_> = statuses.iter().filter(|s| **s != PlanItemStatus::Cancelled).collect();
    if !active.is_empty() && active.iter().all(|s| **s == PlanItemStatus::Done) {
        return PlanItemStatus::Done;
    }
    if !active.is_empty() && active.iter().all(|s| **s == PlanItemStatus::Todo) {
        return PlanItemStatus::Todo;
    }
    PlanItemStatus::InProgress
}

impl Plan {
    /// Computes the overall plan status deterministically based on its items.
    pub fn status(&self) -> PlanItemStatus {
        compute_items_status(&self.items)
    }

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
            status: self.status(),
        }
    }

    pub fn stats(&self) -> PlanStats {
        let (total, completed) = self.count_stats();
        PlanStats {
            total,
            completed,
        }
    }

    pub fn count_stats(&self) -> (usize, usize) {
        fn walk(items: &[PlanItem], total: &mut usize, completed: &mut usize) {
            for item in items {
                *total += 1;
                if item.status == PlanItemStatus::Done {
                    *completed += 1;
                }
                walk(&item.children, total, completed);
            }
        }
        let mut total = 0;
        let mut completed = 0;
        walk(&self.items, &mut total, &mut completed);
        (total, completed)
    }

    pub fn find_item(&self, item_id: &str) -> Option<&PlanItem> {
        fn walk<'a>(items: &'a [PlanItem], target: &str) -> Option<&'a PlanItem> {
            for item in items {
                if item.id.eq_ignore_ascii_case(target) {
                    return Some(item);
                }
                if let Some(found) = walk(&item.children, target) {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.items, item_id)
    }

    pub fn is_leaf_item(&self, item_id: &str) -> Option<bool> {
        self.find_item(item_id).map(|item| item.is_leaf())
    }

    /// Evaluates a scope (leaf item, composite subtree, or entire plan if `scope_id` is None)
    /// for autonomous execution.
    pub fn evaluate_scope(&self, scope_id: Option<&str>) -> PlanScopeEvaluation {
        match scope_id {
            None => {
                let mut discussion = Vec::new();
                let mut leaves = Vec::new();

                fn walk(items: &[PlanItem], discussion: &mut Vec<String>, leaves: &mut Vec<PlanItem>) {
                    for item in items {
                        if item.status == PlanItemStatus::Discussion {
                            discussion.push(format!("#{} {}", item.id, item.title));
                        }
                        if item.is_leaf() {
                            if item.status == PlanItemStatus::Todo || item.status == PlanItemStatus::InProgress {
                                leaves.push(item.clone());
                            }
                        } else {
                            walk(&item.children, discussion, leaves);
                        }
                    }
                }

                walk(&self.items, &mut discussion, &mut leaves);

                if !discussion.is_empty() {
                    PlanScopeEvaluation::BlockedByDiscussion(discussion)
                } else if leaves.is_empty() {
                    PlanScopeEvaluation::AllCompleted
                } else {
                    PlanScopeEvaluation::Ready(leaves)
                }
            }
            Some(id) => {
                let target = match self.find_item(id) {
                    Some(it) => it,
                    None => return PlanScopeEvaluation::NotFound,
                };

                if target.is_leaf() {
                    match target.status {
                        PlanItemStatus::Discussion => {
                            PlanScopeEvaluation::BlockedByDiscussion(vec![format!("#{} {}", target.id, target.title)])
                        }
                        PlanItemStatus::Cancelled => PlanScopeEvaluation::Cancelled,
                        PlanItemStatus::Done => PlanScopeEvaluation::AllCompleted,
                        PlanItemStatus::Todo | PlanItemStatus::InProgress => {
                            PlanScopeEvaluation::Ready(vec![target.clone()])
                        }
                    }
                } else {
                    let mut discussion = Vec::new();
                    let mut leaves = Vec::new();

                    if target.status == PlanItemStatus::Discussion {
                        discussion.push(format!("#{} {}", target.id, target.title));
                    }

                    fn walk(items: &[PlanItem], discussion: &mut Vec<String>, leaves: &mut Vec<PlanItem>) {
                        for item in items {
                            if item.status == PlanItemStatus::Discussion {
                                discussion.push(format!("#{} {}", item.id, item.title));
                            }
                            if item.is_leaf() {
                                if item.status == PlanItemStatus::Todo || item.status == PlanItemStatus::InProgress {
                                    leaves.push(item.clone());
                                }
                            } else {
                                walk(&item.children, discussion, leaves);
                            }
                        }
                    }

                    walk(&target.children, &mut discussion, &mut leaves);

                    if !discussion.is_empty() {
                        PlanScopeEvaluation::BlockedByDiscussion(discussion)
                    } else if leaves.is_empty() {
                        PlanScopeEvaluation::AllCompleted
                    } else {
                        PlanScopeEvaluation::Ready(leaves)
                    }
                }
            }
        }
    }

    pub fn refresh_parent_statuses(&mut self) {
        fn walk(items: &mut [PlanItem]) {
            for item in items.iter_mut() {
                if !item.children.is_empty() {
                    walk(&mut item.children);
                    item.status = compute_items_status(&item.children);
                }
            }
        }
        walk(&mut self.items);
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
        let updated = walk(&mut self.items, item_id, status);
        if updated {
            self.refresh_parent_statuses();
        }
        updated
    }

    pub fn to_markdown(&self) -> String {
        let mut out = format!("# Plan: {}\n", self.title);
        if let Some(desc) = &self.description {
            let trimmed = desc.trim();
            if !trimmed.is_empty() {
                out.push('\n');
                out.push_str(trimmed);
                out.push('\n');
            }
        }
        out.push('\n');

        fn write_items(items: &[PlanItem], depth: usize, out: &mut String) {
            for item in items {
                let indent = "  ".repeat(depth);
                let check = match item.status {
                    PlanItemStatus::Done => "[x]",
                    PlanItemStatus::InProgress => "[>]",
                    PlanItemStatus::Cancelled => "[-]",
                    PlanItemStatus::Discussion => "[?]",
                    PlanItemStatus::Todo => "[ ]",
                };
                let clean_title = item.title.replace(['\r', '\n'], " ").trim().to_string();
                out.push_str(&format!("{}- {} {} #{}: {}\n", indent, check, item.status, item.id, clean_title));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        for line in trimmed.lines() {
                            out.push_str(&format!("{}    {}\n", indent, line));
                        }
                    }
                }
                write_items(&item.children, depth + 1, out);
            }
        }

        write_items(&self.items, 0, &mut out);
        out
    }
}

pub const PLAN_DISCUSSION_PREFIX: &str = "# Plan Discussion:";

/// Returns true if the prompt represents an explicit plan discussion turn.
pub fn is_plan_discussion_prompt(prompt: &str) -> bool {
    prompt.starts_with(PLAN_DISCUSSION_PREFIX)
}

/// Formats a targeted prompt for discussing and refining a plan or specific marked items.
///
/// Preserves session history and passes the targeted plan structure, focused items,
/// and developer question or instructions for architectural refinement.
pub fn format_plan_discussion_prompt(
    plan: &Plan,
    focused_item_ids: &[String],
    user_comment: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} [{}] {}\n\n",
        PLAN_DISCUSSION_PREFIX, plan.id, plan.title
    ));

    if let Some(desc) = &plan.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("## Plan Architecture & Context:\n{}\n\n", trimmed));
        }
    }

    out.push_str("## Target Plan Context:\n");
    let (total, completed) = plan.count_stats();
    out.push_str(&format!(
        "<active_plan id=\"{}\" title=\"{}\" progress=\"{}/{} completed\">\n",
        plan.id, plan.title, completed, total
    ));
    if let Some(desc) = &plan.description {
        out.push_str(&format!("  <description>{}</description>\n", desc.trim()));
    }

    fn render_discussion_items(
        out: &mut String,
        items: &[PlanItem],
        depth: usize,
        focused_item_ids: &[String],
    ) {
        let indent = "  ".repeat(depth + 1);
        for item in items {
            let is_focused = focused_item_ids.iter().any(|id| id == &item.id);
            let focus_marker = if is_focused { " [FOCUSED]" } else { "" };
            out.push_str(&format!(
                "{}- {}{} (id: \"{}\", status: {})",
                indent, item.title, focus_marker, item.id, item.status
            ));
            if let Some(details) = &item.details {
                let trimmed = details.trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!(": {}", trimmed));
                }
            }
            out.push('\n');
            if !item.children.is_empty() {
                render_discussion_items(out, &item.children, depth + 1, focused_item_ids);
            }
        }
    }
    render_discussion_items(&mut out, &plan.items, 0, focused_item_ids);
    out.push_str("</active_plan>\n\n");

    if !focused_item_ids.is_empty() {
        out.push_str("### Items Focused by User for Discussion:\n");
        for id in focused_item_ids {
            if let Some(item) = plan.find_item(id) {
                out.push_str(&format!("- **#{} {}** (status: {})\n", item.id, item.title, item.status));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!("  {}\n", trimmed));
                    }
                }
            }
        }
        out.push('\n');
    }

    out.push_str("## Developer Comment / Instructions:\n");
    out.push_str(user_comment.trim());
    out.push_str("\n\n");

    out.push_str("## Instructions for Model:\n");
    out.push_str("1. Address the developer's question or feedback regarding the plan architecture and steps.\n");
    out.push_str("2. If the plan structure, steps, or statuses should be updated based on this discussion, explain the proposed modifications in conversational text in `message` AND populate the structured `plan_update` field in your response object (with action, id, title, description, and items). Never describe plan changes solely in conversational text without populating `plan_update`.\n");
    out.push_str("3. Keep explanations clear, rigorous, and directly aligned with codebase design.\n");

    out
}

/// Evaluation result of a targeted plan execution scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanScopeEvaluation {
    Ready(Vec<PlanItem>),
    BlockedByDiscussion(Vec<String>),
    AllCompleted,
    Cancelled,
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanStats {
    pub total: usize,
    pub completed: usize,
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
    pub plans: Vec<Plan>,
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
pub struct PlanExecuteStepParams {
    pub plan_id: String,
    pub step_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanUpdatedEvent {
    pub plan: Plan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanListChangedEvent {
    pub plans: Vec<Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_item_status_lifecycle() {
        let status = PlanItemStatus::Discussion;
        assert_eq!(status.next(), PlanItemStatus::Todo);
        assert_eq!(status.next().next(), PlanItemStatus::InProgress);
        assert_eq!(status.next().next().next(), PlanItemStatus::Done);
        assert_eq!(status.next().next().next().next(), PlanItemStatus::Cancelled);
        assert_eq!(status.next().next().next().next().next(), PlanItemStatus::Discussion);
        assert_eq!(status.to_string(), "DISCUSSION");
    }

    #[test]
    fn test_plan_item_serde() {
        let item = PlanItem {
            id: "1.1".to_string(),
            title: "Define claims".to_string(),
            details: Some("Claims struct with exp and sub".to_string()),
            status: PlanItemStatus::InProgress,
            children: vec![],
        };

        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"in_progress\""));

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
                    children: vec![
                        PlanItem {
                            id: "1.1".to_string(),
                            title: "Claims".to_string(),
                            details: None,
                            status: PlanItemStatus::Done,
                            children: vec![],
                        },
                    ],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Endpoint".to_string(),
                    details: None,
                    status: PlanItemStatus::InProgress,
                    children: vec![],
                },
            ],
        };

        let (total, completed) = plan.count_stats();
        assert_eq!(total, 3);
        assert_eq!(completed, 2);

        let summary = plan.summary();
        assert_eq!(summary.total_items, 3);
        assert_eq!(summary.completed_items, 2);
        assert_eq!(summary.id, "jwt-auth");
        assert_eq!(summary.status, PlanItemStatus::InProgress);
        assert_eq!(plan.status(), PlanItemStatus::InProgress);
    }

    #[test]
    fn test_plan_status_computation() {
        let mut plan = Plan {
            id: "status-test".to_string(),
            title: "Status Test".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Step 1".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    children: vec![],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Step 2".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    children: vec![],
                },
            ],
        };

        // All Todo -> Todo
        assert_eq!(plan.status(), PlanItemStatus::Todo);

        // One InProgress -> InProgress
        plan.items[0].status = PlanItemStatus::InProgress;
        assert_eq!(plan.status(), PlanItemStatus::InProgress);

        // One Done, one Todo -> InProgress
        plan.items[0].status = PlanItemStatus::Done;
        assert_eq!(plan.status(), PlanItemStatus::InProgress);

        // One Discussion -> Discussion (top priority)
        plan.items[1].status = PlanItemStatus::Discussion;
        assert_eq!(plan.status(), PlanItemStatus::Discussion);

        // All Done -> Done
        plan.items[1].status = PlanItemStatus::Done;
        assert_eq!(plan.status(), PlanItemStatus::Done);

        // One Cancelled, one Done -> Done
        plan.items[0].status = PlanItemStatus::Cancelled;
        assert_eq!(plan.status(), PlanItemStatus::Done);

        // All Cancelled -> Cancelled
        plan.items[1].status = PlanItemStatus::Cancelled;
        assert_eq!(plan.status(), PlanItemStatus::Cancelled);
    }

    #[test]
    fn test_plan_item_leaf_and_refresh_parent_statuses() {
        let mut plan = Plan {
            id: "leaf-test".to_string(),
            title: "Leaf Test".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            items: vec![
                PlanItem {
                    id: "parent".to_string(),
                    title: "Parent".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    children: vec![
                        PlanItem {
                            id: "sub-1".to_string(),
                            title: "Sub 1".to_string(),
                            details: None,
                            status: PlanItemStatus::Todo,
                            children: vec![],
                        },
                        PlanItem {
                            id: "sub-2".to_string(),
                            title: "Sub 2".to_string(),
                            details: None,
                            status: PlanItemStatus::Todo,
                            children: vec![],
                        },
                    ],
                },
            ],
        };

        assert_eq!(plan.is_leaf_item("parent"), Some(false));
        assert_eq!(plan.is_leaf_item("sub-1"), Some(true));
        assert_eq!(plan.is_leaf_item("unknown"), None);

        // Update first sub-item to InProgress: parent becomes InProgress
        plan.update_item_status("sub-1", PlanItemStatus::InProgress);
        assert_eq!(plan.find_item("parent").unwrap().status, PlanItemStatus::InProgress);

        // Update first sub-item to Discussion: parent becomes Discussion (highest attention priority)
        plan.update_item_status("sub-1", PlanItemStatus::Discussion);
        assert_eq!(plan.find_item("parent").unwrap().status, PlanItemStatus::Discussion);

        // Update first sub-item to Done, second is still Todo: parent becomes InProgress
        plan.update_item_status("sub-1", PlanItemStatus::Done);
        assert_eq!(plan.find_item("parent").unwrap().status, PlanItemStatus::InProgress);

        // Update second sub-item to done: parent automatically transitions to Done
        plan.update_item_status("sub-2", PlanItemStatus::Done);
        assert_eq!(plan.find_item("parent").unwrap().status, PlanItemStatus::Done);
    }

    #[test]
    fn test_plan_evaluate_scope() {
        let plan = Plan {
            id: "batch-test".to_string(),
            title: "Batch Test".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Parent Group".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    children: vec![
                        PlanItem {
                            id: "1.1".to_string(),
                            title: "Step 1.1".to_string(),
                            details: None,
                            status: PlanItemStatus::Todo,
                            children: vec![],
                        },
                        PlanItem {
                            id: "1.2".to_string(),
                            title: "Step 1.2".to_string(),
                            details: None,
                            status: PlanItemStatus::Discussion,
                            children: vec![],
                        },
                    ],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Step 2".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    children: vec![],
                },
            ],
        };

        // 1. Entire plan has step 1.2 in Discussion -> Blocked
        match plan.evaluate_scope(None) {
            PlanScopeEvaluation::BlockedByDiscussion(items) => {
                assert_eq!(items.len(), 1);
                assert!(items[0].contains("1.2"));
            }
            other => panic!("Expected BlockedByDiscussion, got {:?}", other),
        }

        // 2. Leaf step 1.1 is Todo -> Ready with 1 item
        match plan.evaluate_scope(Some("1.1")) {
            PlanScopeEvaluation::Ready(leaves) => {
                assert_eq!(leaves.len(), 1);
                assert_eq!(leaves[0].id, "1.1");
            }
            other => panic!("Expected Ready, got {:?}", other),
        }

        // 3. Leaf step 1.2 is Discussion -> Blocked
        match plan.evaluate_scope(Some("1.2")) {
            PlanScopeEvaluation::BlockedByDiscussion(items) => {
                assert_eq!(items.len(), 1);
            }
            other => panic!("Expected BlockedByDiscussion, got {:?}", other),
        }

        // 4. Once 1.2 is approved to Todo -> entire plan is Ready with 3 items
        let mut clean_plan = plan.clone();
        clean_plan.update_item_status("1.2", PlanItemStatus::Todo);
        match clean_plan.evaluate_scope(None) {
            PlanScopeEvaluation::Ready(leaves) => {
                assert_eq!(leaves.len(), 3);
                assert_eq!(leaves[0].id, "1.1");
                assert_eq!(leaves[1].id, "1.2");
                assert_eq!(leaves[2].id, "2");
            }
            other => panic!("Expected Ready, got {:?}", other),
        }

        // 5. Group 1 is Ready with 2 items
        match clean_plan.evaluate_scope(Some("1")) {
            PlanScopeEvaluation::Ready(leaves) => {
                assert_eq!(leaves.len(), 2);
                assert_eq!(leaves[0].id, "1.1");
                assert_eq!(leaves[1].id, "1.2");
            }
            other => panic!("Expected Ready, got {:?}", other),
        }
    }

    #[test]
    fn test_plan_to_markdown() {
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
                    details: Some("Create token struct".to_string()),
                    status: PlanItemStatus::Done,
                    children: vec![
                        PlanItem {
                            id: "1.1".to_string(),
                            title: "Claims".to_string(),
                            details: None,
                            status: PlanItemStatus::Done,
                            children: vec![],
                        },
                    ],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Endpoint".to_string(),
                    details: None,
                    status: PlanItemStatus::InProgress,
                    children: vec![],
                },
            ],
        };

        let md = plan.to_markdown();
        assert!(md.contains("# Plan: JWT Authentication"));
        assert!(md.contains("Replace session cookies"));
        assert!(md.contains("- [x] DONE #1: Models"));
        assert!(md.contains("    Create token struct"));
        assert!(md.contains("  - [x] DONE #1.1: Claims"));
        assert!(md.contains("- [>] IN_PROGRESS #2: Endpoint"));
    }
}
