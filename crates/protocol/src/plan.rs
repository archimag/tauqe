use serde::{Deserialize, Serialize};

use crate::model::ModelSelection;

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

/// A node in the conceptual architectural tree ("What" / Requirements & Invariants).
///
/// Conceptual items represent design decisions, architectural invariants, and high-level
/// goals. They are formed and discussed in dialogue, and remain read-only during deep
/// implementation refinement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ConceptItem {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ConceptItem>,
}

impl ConceptItem {
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}

/// An actionable engineering step in the implementation blueprint ("How").
///
/// Implementation items are formed and refined by autonomous deep analysis.
/// Each step contains the exact paths of targeted files (`files`) that are
/// deterministically pre-loaded into context upon execution, as well as optional
/// references to conceptual requirements (`concept_refs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanItem {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(default)]
    pub status: PlanItemStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub concept_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<PlanItem>,
}

impl PlanItem {
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}

/// Two-section engineering plan: Concept Tree ("What") and Implementation Blueprint ("How").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Plan {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub concept: Vec<ConceptItem>,
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

    pub fn find_concept_item(&self, item_id: &str) -> Option<&ConceptItem> {
        fn walk<'a>(items: &'a [ConceptItem], target: &str) -> Option<&'a ConceptItem> {
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
        walk(&self.concept, item_id)
    }

    pub fn find_concept_item_mut(&mut self, item_id: &str) -> Option<&mut ConceptItem> {
        fn walk<'a>(items: &'a mut [ConceptItem], target: &str) -> Option<&'a mut ConceptItem> {
            for item in items {
                if item.id.eq_ignore_ascii_case(target) {
                    return Some(item);
                }
                if let Some(found) = walk(&mut item.children, target) {
                    return Some(found);
                }
            }
            None
        }
        walk(&mut self.concept, item_id)
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

    /// Enforces the leaf-only file invariant across the plan.
    ///
    /// Parent items act as conceptual milestones, phases, and task groups.
    /// They are never executed autonomously, so any `files` list attached to a
    /// non-leaf item is cleared. Only leaf items (`is_leaf()`) define target
    /// context files.
    pub fn normalize_leaf_files(&mut self) {
        fn walk(items: &mut [PlanItem]) {
            for item in items.iter_mut() {
                if item.children.is_empty() {
                    if !item.readonly_files.is_empty() && !item.files.is_empty() {
                        item.readonly_files.retain(|r| !item.files.contains(r));
                    }
                    continue;
                }
                item.files.clear();
                item.readonly_files.clear();
                walk(&mut item.children);
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

        if !self.concept.is_empty() {
            out.push_str("\n## Concept\n\n");
            fn write_concept(items: &[ConceptItem], depth: usize, out: &mut String) {
                for item in items {
                    let indent = "  ".repeat(depth);
                    let clean_title = item.title.replace(['\r', '\n'], " ").trim().to_string();
                    out.push_str(&format!("{}- #{}: {}\n", indent, item.id, clean_title));
                    if let Some(details) = &item.details {
                        let trimmed = details.trim();
                        if !trimmed.is_empty() {
                            for line in trimmed.lines() {
                                out.push_str(&format!("{}    {}\n", indent, line));
                            }
                        }
                    }
                    write_concept(&item.children, depth + 1, out);
                }
            }
            write_concept(&self.concept, 0, &mut out);
        }

        if !self.concept.is_empty() && !self.items.is_empty() {
            out.push_str("\n## Implementation\n");
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
                out.push_str(&format!(
                    "{}- {} {} #{}: {}\n",
                    indent, check, item.status, item.id, clean_title
                ));
                if !item.concept_refs.is_empty() {
                    out.push_str(&format!("{}    ref: {}\n", indent, item.concept_refs.join(", ")));
                }
                if !item.files.is_empty() {
                    out.push_str(&format!("{}    files: {}\n", indent, item.files.join(", ")));
                }
                if !item.readonly_files.is_empty() {
                    out.push_str(&format!("{}    readonly_files: {}\n", indent, item.readonly_files.join(", ")));
                }
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

    if !plan.concept.is_empty() {
        out.push_str("  <concept>\n");
        fn render_concept_xml(
            out: &mut String,
            items: &[ConceptItem],
            depth: usize,
            focused_item_ids: &[String],
        ) {
            let indent = "  ".repeat(depth + 2);
            for item in items {
                let is_focused = focused_item_ids.iter().any(|id| id == &item.id);
                let focus_marker = if is_focused { " [FOCUSED]" } else { "" };
                out.push_str(&format!("{}- #{}{} {}", indent, item.id, focus_marker, item.title));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", trimmed));
                    }
                }
                out.push('\n');
                if !item.children.is_empty() {
                    render_concept_xml(out, &item.children, depth + 1, focused_item_ids);
                }
            }
        }
        render_concept_xml(&mut out, &plan.concept, 0, focused_item_ids);
        out.push_str("  </concept>\n");
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
            } else if let Some(concept) = plan.find_concept_item(id) {
                out.push_str(&format!("- **[CONCEPT] #{} {}**\n", concept.id, concept.title));
                if let Some(details) = &concept.details {
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
    out.push_str("2. Reins Invariant: Never mutate the plan (keep `plan_update: null`) without an explicit user instruction to modify it. Questions, analysis, trade-off reviews, and critique must be answered strictly in conversational text in `message`.\n");
    out.push_str("3. When explicitly instructed to update the plan, explain the proposed modifications in `message` and populate the structured `plan_update` field.\n");
    out.push_str("4. Plan Action Semantics: Use `action: \"save\"` to create or completely overwrite the plan (mandatory `concept` tree defining architectural intent, with optional implementation `items`). Use `action: \"update\"` to modify individual existing items or concept nodes by ID without altering unmentioned items. Use `action: \"delete\"` to remove the plan.\n");
    out.push_str("5. Keep explanations clear, rigorous, and directly aligned with codebase design.\n");
    out.push_str("6. Language Consistency Directive: Detect the natural language of the plan title and description. Formulate all plan titles, descriptions, step titles, step details, and conversational explanations strictly in that same language.\n");

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanExecuteParams {
    pub plan_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanRefineParams {
    pub plan_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PlanRefineResult {
    Success {
        plan: Plan,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
    },
    Blocked {
        reason: String,
    },
}

impl PlanRefineResult {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    pub fn plan(&self) -> Option<&Plan> {
        match self {
            Self::Success { plan, .. } => Some(plan),
            Self::Blocked { .. } => None,
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Success { .. } => None,
            Self::Blocked { reason } => Some(reason),
        }
    }
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
    use crate::model::ModelRole;

    fn item(id: &str, title: &str, status: PlanItemStatus, children: Vec<PlanItem>) -> PlanItem {
        PlanItem {
            id: id.to_string(),
            title: title.to_string(),
            status,
            children,
            ..Default::default()
        }
    }

    #[test]
    fn test_plan_refine_types_serde_and_helpers() {
        let params = PlanRefineParams {
            plan_id: "core-feature".to_string(),
            model: Some(ModelSelection::Role(ModelRole::Architect)),
            instructions: Some("Decompose into atomic steps".to_string()),
        };
        let json_params = serde_json::to_string(&params).unwrap();
        assert!(json_params.contains("\"core-feature\""));
        assert!(json_params.contains("\"architect\""));
        let de_params: PlanRefineParams = serde_json::from_str(&json_params).unwrap();
        assert_eq!(de_params, params);

        // Backward compatibility with legacy tier string:
        let legacy_json = r#"{"plan_id":"core-feature","model":"senior"}"#;
        let legacy_params: PlanRefineParams = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(legacy_params.model, Some(ModelSelection::Role(ModelRole::Architect)));

        let plan = Plan {
            id: "core-feature".to_string(),
            title: "Feature".to_string(),
            description: None,
            created_at: 10,
            updated_at: 20,
            concept: vec![],
            items: vec![],
        };

        let success_res = PlanRefineResult::Success {
            plan: plan.clone(),
            explanation: Some("All decomposed".to_string()),
        };
        assert!(success_res.is_success());
        assert!(!success_res.is_blocked());
        assert_eq!(success_res.plan().unwrap().id, "core-feature");
        assert_eq!(success_res.reason(), None);

        let json_success = serde_json::to_string(&success_res).unwrap();
        assert!(json_success.contains("\"status\":\"success\""));
        let de_success: PlanRefineResult = serde_json::from_str(&json_success).unwrap();
        assert_eq!(de_success, success_res);

        let blocked_res = PlanRefineResult::Blocked {
            reason: "Contradiction in requirements".to_string(),
        };
        assert!(!blocked_res.is_success());
        assert!(blocked_res.is_blocked());
        assert_eq!(blocked_res.plan(), None);
        assert_eq!(blocked_res.reason(), Some("Contradiction in requirements"));

        let json_blocked = serde_json::to_string(&blocked_res).unwrap();
        assert!(json_blocked.contains("\"status\":\"blocked\""));
        assert!(json_blocked.contains("Contradiction in requirements"));
        let de_blocked: PlanRefineResult = serde_json::from_str(&json_blocked).unwrap();
        assert_eq!(de_blocked, blocked_res);
    }

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
    fn test_concept_item_and_two_section_plan_serde() {
        let concept = vec![
            ConceptItem {
                id: "c-1".to_string(),
                title: "Stateless Auth".to_string(),
                details: Some("Tokens must be signed with ed25519".to_string()),
                children: vec![ConceptItem {
                    id: "c-1.1".to_string(),
                    title: "Token Expiration Invariant".to_string(),
                    details: None,
                    children: vec![],
                }],
            },
        ];

        let mut step = item("s-1", "Implement JWT signer", PlanItemStatus::Todo, vec![]);
        step.files = vec!["crates/core/src/auth.rs".to_string()];
        step.concept_refs = vec!["c-1.1".to_string()];

        let mut plan = Plan {
            id: "auth-plan".to_string(),
            title: "Auth Plan".to_string(),
            description: Some("Architecture & steps".to_string()),
            created_at: 100,
            updated_at: 200,
            concept,
            items: vec![step],
        };

        // Concept navigation
        assert!(plan.find_concept_item("c-1").is_some());
        assert_eq!(plan.find_concept_item("c-1.1").unwrap().title, "Token Expiration Invariant");
        assert!(plan.find_concept_item("non-existent").is_none());

        // Serde roundtrip
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"concept\""));
        assert!(json.contains("\"files\""));
        assert!(json.contains("\"concept_refs\""));

        let de: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(de, plan);

        // Markdown rendering with concept and files
        let md = plan.to_markdown();
        assert!(md.contains("## Concept"));
        assert!(md.contains("- #c-1: Stateless Auth"));
        assert!(md.contains("    Tokens must be signed with ed25519"));
        assert!(md.contains("  - #c-1.1: Token Expiration Invariant"));
        assert!(md.contains("## Implementation"));
        assert!(md.contains("files: crates/core/src/auth.rs"));
        assert!(md.contains("ref: c-1.1"));

        // Mutate concept item
        if let Some(c) = plan.find_concept_item_mut("c-1.1") {
            c.title = "Updated Invariant".to_string();
        }
        assert_eq!(plan.find_concept_item("c-1.1").unwrap().title, "Updated Invariant");
    }

    #[test]
    fn test_plan_item_serde() {
        let mut it = item("1.1", "Define claims", PlanItemStatus::InProgress, vec![]);
        it.details = Some("Claims struct with exp and sub".to_string());
        it.files = vec!["src/claims.rs".to_string()];
        it.readonly_files = vec!["src/utils.rs".to_string()];
        it.concept_refs = vec!["c-1".to_string()];

        let json = serde_json::to_string(&it).unwrap();
        assert!(json.contains("\"in_progress\""));
        assert!(json.contains("\"files\""));
        assert!(json.contains("\"readonly_files\""));
        assert!(json.contains("\"concept_refs\""));

        let de: PlanItem = serde_json::from_str(&json).unwrap();
        assert_eq!(de, it);

        // Backward compatibility: missing concept, files, readonly_files, concept_refs deserialize to empty defaults
        let legacy_json = r#"{"id":"1.1","title":"Define claims","status":"in_progress"}"#;
        let legacy_item: PlanItem = serde_json::from_str(legacy_json).unwrap();
        assert!(legacy_item.files.is_empty());
        assert!(legacy_item.readonly_files.is_empty());
        assert!(legacy_item.concept_refs.is_empty());

        let legacy_plan_json = r#"{"id":"p1","title":"Plan","created_at":1,"updated_at":2,"items":[]}"#;
        let legacy_plan: Plan = serde_json::from_str(legacy_plan_json).unwrap();
        assert!(legacy_plan.concept.is_empty());
    }

    #[test]
    fn test_plan_item_ignores_legacy_model_field() {
        let legacy: PlanItem = serde_json::from_str(r#"{"id":"2","title":"Old step","model":"junior"}"#).unwrap();
        assert_eq!(legacy.id, "2");
        assert_eq!(legacy.title, "Old step");
    }

    #[test]
    fn test_plan_stats_and_summary() {
        let plan = Plan {
            id: "jwt-auth".to_string(),
            title: "JWT Authentication".to_string(),
            description: Some("Replace session cookies".to_string()),
            created_at: 1700000000,
            updated_at: 1700000010,
            concept: vec![],
            items: vec![
                item(
                    "1",
                    "Models",
                    PlanItemStatus::Done,
                    vec![item("1.1", "Claims", PlanItemStatus::Done, vec![])],
                ),
                item("2", "Endpoint", PlanItemStatus::InProgress, vec![]),
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
            concept: vec![],
            items: vec![
                item("1", "Step 1", PlanItemStatus::Todo, vec![]),
                item("2", "Step 2", PlanItemStatus::Todo, vec![]),
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
            concept: vec![],
            items: vec![item(
                "parent",
                "Parent",
                PlanItemStatus::Todo,
                vec![
                    item("sub-1", "Sub 1", PlanItemStatus::Todo, vec![]),
                    item("sub-2", "Sub 2", PlanItemStatus::Todo, vec![]),
                ],
            )],
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
    fn test_plan_normalize_leaf_files() {
        let mut inner_leaf = item("1.1", "Inner Leaf", PlanItemStatus::Todo, vec![]);
        inner_leaf.files = vec!["src/inner.rs".to_string(), "src/shared.rs".to_string()];
        inner_leaf.readonly_files = vec!["src/ref.rs".to_string(), "src/shared.rs".to_string()];

        let mut mid = item("1", "Mid Group", PlanItemStatus::Todo, vec![inner_leaf]);
        mid.files = vec!["src/mid.rs".to_string()];
        mid.readonly_files = vec!["src/mid_ref.rs".to_string()];

        let mut top = item("root", "Root Group", PlanItemStatus::Todo, vec![mid]);
        top.files = vec!["src/top.rs".to_string()];

        let mut leaf = item("2", "Top Leaf", PlanItemStatus::Todo, vec![]);
        leaf.files = vec!["src/leaf.rs".to_string()];
        leaf.readonly_files = vec!["src/leaf_ref.rs".to_string()];

        let mut plan = Plan {
            id: "leaf-files".to_string(),
            title: "Leaf Files".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            concept: vec![],
            items: vec![top, leaf],
        };

        plan.normalize_leaf_files();

        // Containers are cleared at every depth.
        assert!(plan.items[0].files.is_empty());
        assert!(plan.items[0].readonly_files.is_empty());
        assert!(plan.items[0].children[0].files.is_empty());
        assert!(plan.items[0].children[0].readonly_files.is_empty());
        // Leaves keep their files, overlapping readonly_files are removed.
        assert_eq!(
            plan.items[0].children[0].children[0].files,
            vec!["src/inner.rs".to_string(), "src/shared.rs".to_string()]
        );
        assert_eq!(
            plan.items[0].children[0].children[0].readonly_files,
            vec!["src/ref.rs".to_string()]
        );
        assert_eq!(plan.items[1].files, vec!["src/leaf.rs".to_string()]);
        assert_eq!(plan.items[1].readonly_files, vec!["src/leaf_ref.rs".to_string()]);
    }

    #[test]
    fn test_plan_evaluate_scope() {
        let plan = Plan {
            id: "batch-test".to_string(),
            title: "Batch Test".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            concept: vec![],
            items: vec![
                item(
                    "1",
                    "Parent Group",
                    PlanItemStatus::Todo,
                    vec![
                        item("1.1", "Step 1.1", PlanItemStatus::Todo, vec![]),
                        item("1.2", "Step 1.2", PlanItemStatus::Discussion, vec![]),
                    ],
                ),
                item("2", "Step 2", PlanItemStatus::Todo, vec![]),
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
        let mut models = item(
            "1",
            "Models",
            PlanItemStatus::Done,
            vec![item("1.1", "Claims", PlanItemStatus::Done, vec![])],
        );
        models.details = Some("Create token struct".to_string());
        let mut endpoint = item("2", "Endpoint", PlanItemStatus::InProgress, vec![]);
        endpoint.readonly_files = vec!["src/models.rs".to_string()];

        let plan = Plan {
            id: "jwt-auth".to_string(),
            title: "JWT Authentication".to_string(),
            description: Some("Replace session cookies".to_string()),
            created_at: 1700000000,
            updated_at: 1700000010,
            concept: vec![],
            items: vec![models, endpoint],
        };

        let md = plan.to_markdown();
        assert!(md.contains("# Plan: JWT Authentication"));
        assert!(md.contains("Replace session cookies"));
        assert!(md.contains("- [x] DONE #1: Models"));
        assert!(md.contains("    Create token struct"));
        assert!(md.contains("  - [x] DONE #1.1: Claims"));
        assert!(md.contains("- [>] IN_PROGRESS #2: Endpoint"));
        assert!(md.contains("    readonly_files: src/models.rs"));
    }
}
