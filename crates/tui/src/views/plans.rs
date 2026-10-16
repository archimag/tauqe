use std::collections::HashSet;

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use tauqe_protocol::{
    ConceptItem, DiscussionFocus, DiscussionTarget, ModelEntry, ModelRole, Plan, PlanItem,
    PlanItemStatus,
};

use crate::components::model_selector::ModelSelector;
use crate::theme::engine::current_engine;
use crate::ui::wrap_lines;

pub mod input;
pub mod render;

pub use self::input::{handle_plans_key, PLANS_COMMAND_MAP};
pub use self::render::render_plans_view;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisiblePlanRow {
    PlanHeader {
        plan_id: String,
        title: String,
        description: Option<String>,
        total: usize,
        completed: usize,
        status: PlanItemStatus,
        is_expanded: bool,
        /// Whether the active discussion focus anchors on the whole plan.
        focused: bool,
    },
    ConceptHeader {
        plan_id: String,
        count: usize,
        is_expanded: bool,
    },
    ConceptItem {
        plan_id: String,
        item_id: String,
        title: String,
        details: Option<String>,
        depth: usize,
        has_children: bool,
        has_details: bool,
        is_expandable: bool,
        is_expanded: bool,
        /// Whether the active discussion focus anchors on this concept item.
        focused: bool,
    },
    ImplementationHeader {
        plan_id: String,
        total: usize,
        completed: usize,
        is_expanded: bool,
    },
    PlanItem {
        plan_id: String,
        item_id: String,
        title: String,
        details: Option<String>,
        status: PlanItemStatus,
        files: Vec<String>,
        readonly_files: Vec<String>,
        concept_refs: Vec<String>,
        depth: usize,
        has_children: bool,
        has_details: bool,
        is_expandable: bool,
        is_expanded: bool,
        /// Whether the active discussion focus anchors on this step.
        focused: bool,
    },
}

impl VisiblePlanRow {
    pub fn plan_id(&self) -> &str {
        match self {
            Self::PlanHeader { plan_id, .. } => plan_id,
            Self::ConceptHeader { plan_id, .. } => plan_id,
            Self::ConceptItem { plan_id, .. } => plan_id,
            Self::ImplementationHeader { plan_id, .. } => plan_id,
            Self::PlanItem { plan_id, .. } => plan_id,
        }
    }
}

/// Focus section of the plan refine dialog.
///
/// The dialog exposes three keyboard targets — the embedded Architect model
/// list, the instructions editor, and the action buttons — and `Tab` cycles
/// through them in visual order. Making the section explicit mirrors the review
/// launcher (`ReviewDialogFocus`) and keeps list navigation, text editing, and
/// button activation from fighting over the same keys (Conventions §6, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefineDialogFocus {
    ModelList,
    #[default]
    InstructionsEditor,
    Actions,
}

impl RefineDialogFocus {
    /// Advances focus to the next section in visual order.
    ///
    /// When only one model is configured, `ModelList` is omitted from cycling so
    /// navigation moves directly between instructions and action buttons.
    pub fn cycle_next(&mut self, entry_count: usize) {
        *self = match self {
            Self::ModelList => Self::InstructionsEditor,
            Self::InstructionsEditor => Self::Actions,
            Self::Actions => {
                if entry_count > 1 {
                    Self::ModelList
                } else {
                    Self::InstructionsEditor
                }
            }
        };
    }

    /// Moves focus to the previous section in visual order.
    pub fn cycle_prev(&mut self, entry_count: usize) {
        *self = match self {
            Self::ModelList => Self::Actions,
            Self::InstructionsEditor => {
                if entry_count > 1 {
                    Self::ModelList
                } else {
                    Self::Actions
                }
            }
            Self::Actions => Self::InstructionsEditor,
        };
    }

    pub fn is_model_list(&self) -> bool {
        matches!(self, Self::ModelList)
    }

    pub fn is_instructions_editor(&self) -> bool {
        matches!(self, Self::InstructionsEditor)
    }

    pub fn is_actions(&self) -> bool {
        matches!(self, Self::Actions)
    }
}

#[derive(Debug, Clone)]
pub struct PlanRefineDialogState {
    pub plan_id: String,
    pub plan_title: String,
    pub selector: ModelSelector,
    pub instructions_editor: crate::editor::InputEditor,
    /// Section that currently owns the keyboard.
    pub focus: RefineDialogFocus,
    pub confirm_button: crate::app::ConfirmDialogButton,
}

impl PlanRefineDialogState {
    /// Builds a plan refinement dialog selecting among the configured
    /// `architect_entries` for the Architect role.
    pub fn new(
        plan_id: String,
        plan_title: String,
        architect_entries: Vec<ModelEntry>,
    ) -> Self {
        Self {
            plan_id,
            plan_title,
            selector: ModelSelector::new(ModelRole::Architect, architect_entries, 0),
            instructions_editor: crate::editor::InputEditor::default(),
            focus: RefineDialogFocus::default(),
            confirm_button: crate::app::ConfirmDialogButton::Cancel,
        }
    }

    /// Currently selected architect entry, if any.
    pub fn selected_entry(&self) -> Option<&ModelEntry> {
        self.selector.selected_entry()
    }
}

#[derive(Debug, Clone)]
pub struct PlanRefineBlockedState {
    pub plan_id: String,
    pub plan_title: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct PlansViewState {
    pub plans: Vec<Plan>,
    pub selected_index: usize,
    pub scroll: u16,
    pub expanded_plans: HashSet<String>,
    pub expanded_items: HashSet<String>,
    pub collapsed_sections: HashSet<String>,
    pub view_height: u16,
    pub content_width: usize,
    pub rendered_lines: usize,
    pub status_message: Option<String>,
    pub refining: bool,
    pub refining_plan_id: Option<String>,
    pub refine_dialog: Option<PlanRefineDialogState>,
    pub refine_blocked: Option<PlanRefineBlockedState>,
    pub theme_mode: crate::config::ThemeMode,
    /// Active discussion focus mirrored from the server, used to render the
    /// `[FOCUS]` indicator. Synced from `AppState::discussion_focus` on render.
    pub discussion_focus: DiscussionFocus,
}

impl PlansViewState {
    pub fn is_plan_expanded(&self, plan_id: &str) -> bool {
        self.expanded_plans.contains(plan_id)
    }

    pub fn toggle_plan_expanded(&mut self, plan_id: &str) {
        if !self.expanded_plans.remove(plan_id) {
            self.expanded_plans.insert(plan_id.to_string());
        }
    }

    pub fn is_concept_section_expanded(&self, plan_id: &str) -> bool {
        !self.collapsed_sections.contains(&format!("concept:{}", plan_id))
    }

    pub fn toggle_concept_section_expanded(&mut self, plan_id: &str) {
        let key = format!("concept:{}", plan_id);
        if !self.collapsed_sections.remove(&key) {
            self.collapsed_sections.insert(key);
        }
    }

    pub fn is_impl_section_expanded(&self, plan_id: &str) -> bool {
        !self.collapsed_sections.contains(&format!("impl:{}", plan_id))
    }

    pub fn toggle_impl_section_expanded(&mut self, plan_id: &str) {
        let key = format!("impl:{}", plan_id);
        if !self.collapsed_sections.remove(&key) {
            self.collapsed_sections.insert(key);
        }
    }

    pub fn is_concept_item_expanded(&self, plan_id: &str, item_id: &str) -> bool {
        self.expanded_items.contains(&format!("c:{}:{}", plan_id, item_id))
    }

    pub fn toggle_concept_item_expanded(&mut self, plan_id: &str, item_id: &str) {
        let key = format!("c:{}:{}", plan_id, item_id);
        if !self.expanded_items.remove(&key) {
            self.expanded_items.insert(key);
        }
    }

    pub fn is_item_expanded(&self, plan_id: &str, item_id: &str) -> bool {
        self.expanded_items.contains(&format!("{}:{}", plan_id, item_id))
    }

    pub fn toggle_item_expanded(&mut self, plan_id: &str, item_id: &str) {
        let key = format!("{}:{}", plan_id, item_id);
        if !self.expanded_items.remove(&key) {
            self.expanded_items.insert(key);
        }
    }

    pub fn is_all_expanded(&self) -> bool {
        !self.plans.is_empty()
            && self.plans.iter().all(|p| self.expanded_plans.contains(&p.id))
            && self.collapsed_sections.is_empty()
    }

    pub fn expand_all(&mut self) {
        self.collapsed_sections.clear();
        for plan in &self.plans {
            self.expanded_plans.insert(plan.id.clone());

            fn collect_concept_keys(plan_id: &str, item: &ConceptItem, out: &mut HashSet<String>) {
                out.insert(format!("c:{}:{}", plan_id, item.id));
                for child in &item.children {
                    collect_concept_keys(plan_id, child, out);
                }
            }
            for item in &plan.concept {
                collect_concept_keys(&plan.id, item, &mut self.expanded_items);
            }

            fn collect_item_keys(plan_id: &str, item: &PlanItem, out: &mut HashSet<String>) {
                out.insert(format!("{}:{}", plan_id, item.id));
                for child in &item.children {
                    collect_item_keys(plan_id, child, out);
                }
            }
            for item in &plan.items {
                collect_item_keys(&plan.id, item, &mut self.expanded_items);
            }
        }
    }

    pub fn collapse_all(&mut self) {
        self.expanded_plans.clear();
        self.expanded_items.clear();
        self.collapsed_sections.clear();
    }

    pub fn toggle_fold_all(&mut self) {
        if self.is_all_expanded() {
            self.collapse_all();
        } else {
            self.expand_all();
        }
    }

    /// Whether the active discussion focus anchors on a whole plan.
    fn is_plan_focused(&self, plan_id: &str) -> bool {
        matches!(
            &self.discussion_focus,
            DiscussionFocus::Focused {
                target: DiscussionTarget::Plan { id },
                focused_items,
            } if id.as_str() == plan_id && focused_items.is_empty()
        )
    }

    /// Whether the active discussion focus anchors on a specific plan or
    /// concept item.
    fn is_item_focused(&self, plan_id: &str, item_id: &str) -> bool {
        match &self.discussion_focus {
            DiscussionFocus::Focused {
                target: DiscussionTarget::Plan { id },
                focused_items,
            } if id.as_str() == plan_id => {
                focused_items.iter().any(|fid| fid.as_str() == item_id)
            }
            _ => false,
        }
    }

    pub fn flatten_rows(&self) -> Vec<VisiblePlanRow> {
        let mut rows = Vec::new();
        for plan in &self.plans {
            let is_expanded = self.is_plan_expanded(&plan.id);
            let (total, completed) = plan.count_stats();
            let status = plan.status();

            rows.push(VisiblePlanRow::PlanHeader {
                plan_id: plan.id.clone(),
                title: plan.title.clone(),
                description: plan.description.clone(),
                total,
                completed,
                status,
                is_expanded,
                focused: self.is_plan_focused(&plan.id),
            });

            if is_expanded {
                if !plan.concept.is_empty() {
                    let concept_expanded = self.is_concept_section_expanded(&plan.id);
                    rows.push(VisiblePlanRow::ConceptHeader {
                        plan_id: plan.id.clone(),
                        count: plan.concept.len(),
                        is_expanded: concept_expanded,
                    });
                    if concept_expanded {
                        for item in &plan.concept {
                            self.flatten_concept_recursive(&plan.id, item, 2, &mut rows);
                        }
                    }

                    let impl_expanded = self.is_impl_section_expanded(&plan.id);
                    rows.push(VisiblePlanRow::ImplementationHeader {
                        plan_id: plan.id.clone(),
                        total,
                        completed,
                        is_expanded: impl_expanded,
                    });
                    if impl_expanded {
                        for item in &plan.items {
                            self.flatten_item_recursive(&plan.id, item, 2, &mut rows);
                        }
                    }
                } else {
                    for item in &plan.items {
                        self.flatten_item_recursive(&plan.id, item, 1, &mut rows);
                    }
                }
            }
        }
        rows
    }

    fn flatten_concept_recursive(
        &self,
        plan_id: &str,
        item: &ConceptItem,
        depth: usize,
        out: &mut Vec<VisiblePlanRow>,
    ) {
        let has_children = !item.children.is_empty();
        let has_details = item.details.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
        let is_expandable = has_children || has_details;
        let is_expanded = self.is_concept_item_expanded(plan_id, &item.id);

        out.push(VisiblePlanRow::ConceptItem {
            plan_id: plan_id.to_string(),
            item_id: item.id.clone(),
            title: item.title.clone(),
            details: item.details.clone(),
            depth,
            has_children,
            has_details,
            is_expandable,
            is_expanded,
            focused: self.is_item_focused(plan_id, &item.id),
        });

        if has_children && is_expanded {
            for child in &item.children {
                self.flatten_concept_recursive(plan_id, child, depth + 1, out);
            }
        }
    }

    fn flatten_item_recursive(
        &self,
        plan_id: &str,
        item: &PlanItem,
        depth: usize,
        out: &mut Vec<VisiblePlanRow>,
    ) {
        let has_children = !item.children.is_empty();
        let has_details = item.details.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
        let has_meta = !item.files.is_empty()
            || !item.readonly_files.is_empty()
            || !item.concept_refs.is_empty();
        let is_expandable = has_children || has_details || has_meta;
        let is_expanded = self.is_item_expanded(plan_id, &item.id);

        out.push(VisiblePlanRow::PlanItem {
            plan_id: plan_id.to_string(),
            item_id: item.id.clone(),
            title: item.title.clone(),
            details: item.details.clone(),
            status: item.status,
            files: item.files.clone(),
            readonly_files: item.readonly_files.clone(),
            concept_refs: item.concept_refs.clone(),
            depth,
            has_children,
            has_details,
            is_expandable,
            is_expanded,
            focused: self.is_item_focused(plan_id, &item.id),
        });

        if has_children && is_expanded {
            for child in &item.children {
                self.flatten_item_recursive(plan_id, child, depth + 1, out);
            }
        }
    }

    pub fn selected_row(&self) -> Option<VisiblePlanRow> {
        let rows = self.flatten_rows();
        rows.get(self.selected_index).cloned()
    }

    pub fn clamp_selection(&mut self) {
        let count = self.flatten_rows().len();
        if count == 0 {
            self.selected_index = 0;
        } else if self.selected_index >= count {
            self.selected_index = count - 1;
        }
    }

    pub fn scroll_to_selected(&mut self) {
        let rows = self.flatten_rows();
        if rows.is_empty() || self.view_height == 0 {
            return;
        }
        let sel = self.selected_index.min(rows.len() - 1);
        self.selected_index = sel;

        let (lines, offsets) =
            compute_unified_plan_lines(&rows, sel, self.content_width.max(10), self.theme_mode);
        let height = self.view_height as usize;
        let start = offsets.get(sel).copied().unwrap_or(0);
        let end = offsets.get(sel + 1).copied().unwrap_or(lines.len());
        let scroll = self.scroll as usize;

        let new_scroll = if sel == 0 {
            0
        } else if start < scroll {
            start
        } else if end > scroll + height {
            end.saturating_sub(height).min(start)
        } else {
            scroll
        };
        self.scroll = new_scroll as u16;
    }

    /// Accordion-style navigation: moves to the next or previous leaf item in the plan hierarchy,
    /// closing previous siblings/branches and opening the new target item.
    pub fn accordion_navigate(&mut self, forward: bool) {
        #[derive(Clone)]
        struct PlanNodeRef {
            plan_id: String,
            item_id: Option<String>,
            is_concept: bool,
            ancestor_item_ids: Vec<String>,
            is_expandable: bool,
        }

        fn collect_concept_leaves(
            plan_id: &str,
            item: &ConceptItem,
            ancestors: &mut Vec<String>,
            out: &mut Vec<PlanNodeRef>,
        ) {
            if item.children.is_empty() {
                let has_details =
                    item.details.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
                out.push(PlanNodeRef {
                    plan_id: plan_id.to_string(),
                    item_id: Some(item.id.clone()),
                    is_concept: true,
                    ancestor_item_ids: ancestors.clone(),
                    is_expandable: has_details,
                });
            } else {
                ancestors.push(item.id.clone());
                for child in &item.children {
                    collect_concept_leaves(plan_id, child, ancestors, out);
                }
                ancestors.pop();
            }
        }

        fn collect_item_leaves(
            plan_id: &str,
            item: &PlanItem,
            ancestors: &mut Vec<String>,
            out: &mut Vec<PlanNodeRef>,
        ) {
            if item.children.is_empty() {
                let has_details =
                    item.details.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
                let has_meta = !item.files.is_empty()
                    || !item.readonly_files.is_empty()
                    || !item.concept_refs.is_empty();
                let is_expandable = has_details || has_meta;
                out.push(PlanNodeRef {
                    plan_id: plan_id.to_string(),
                    item_id: Some(item.id.clone()),
                    is_concept: false,
                    ancestor_item_ids: ancestors.clone(),
                    is_expandable,
                });
            } else {
                ancestors.push(item.id.clone());
                for child in &item.children {
                    collect_item_leaves(plan_id, child, ancestors, out);
                }
                ancestors.pop();
            }
        }

        let mut leaves = Vec::new();
        for plan in &self.plans {
            let mut plan_leaves = Vec::new();

            let mut concept_ancestors = Vec::new();
            for item in &plan.concept {
                collect_concept_leaves(&plan.id, item, &mut concept_ancestors, &mut plan_leaves);
            }

            let mut item_ancestors = Vec::new();
            for item in &plan.items {
                collect_item_leaves(&plan.id, item, &mut item_ancestors, &mut plan_leaves);
            }

            if plan_leaves.is_empty() {
                plan_leaves.push(PlanNodeRef {
                    plan_id: plan.id.clone(),
                    item_id: None,
                    is_concept: false,
                    ancestor_item_ids: Vec::new(),
                    is_expandable: plan
                        .description
                        .as_deref()
                        .map(|d| !d.trim().is_empty())
                        .unwrap_or(false),
                });
            }

            leaves.extend(plan_leaves);
        }

        if leaves.is_empty() {
            return;
        }

        let sel_row = self.selected_row();
        let target_idx = match sel_row {
            Some(VisiblePlanRow::ConceptItem { ref plan_id, ref item_id, .. }) => {
                if let Some(pos) = leaves.iter().position(|l| {
                    l.plan_id == *plan_id && l.is_concept && l.item_id.as_deref() == Some(item_id)
                }) {
                    if forward {
                        if pos + 1 < leaves.len() { pos + 1 } else { return; }
                    } else if pos > 0 {
                        pos - 1
                    } else {
                        return;
                    }
                } else {
                    let first_descendant = leaves.iter().position(|l| {
                        l.plan_id == *plan_id
                            && l.is_concept
                            && l.ancestor_item_ids.iter().any(|a| a == item_id)
                    });
                    if forward {
                        if let Some(first) = first_descendant { first } else { return; }
                    } else if let Some(first) = first_descendant {
                        if first > 0 { first - 1 } else { return; }
                    } else {
                        return;
                    }
                }
            }
            Some(VisiblePlanRow::PlanItem { ref plan_id, ref item_id, .. }) => {
                if let Some(pos) = leaves.iter().position(|l| {
                    l.plan_id == *plan_id && !l.is_concept && l.item_id.as_deref() == Some(item_id)
                }) {
                    if forward {
                        if pos + 1 < leaves.len() { pos + 1 } else { return; }
                    } else if pos > 0 {
                        pos - 1
                    } else {
                        return;
                    }
                } else {
                    let first_descendant = leaves.iter().position(|l| {
                        l.plan_id == *plan_id
                            && !l.is_concept
                            && l.ancestor_item_ids.iter().any(|a| a == item_id)
                    });
                    if forward {
                        if let Some(first) = first_descendant { first } else { return; }
                    } else if let Some(first) = first_descendant {
                        if first > 0 { first - 1 } else { return; }
                    } else {
                        return;
                    }
                }
            }
            Some(VisiblePlanRow::ImplementationHeader { ref plan_id, .. }) => {
                let first_impl = leaves.iter().position(|l| l.plan_id == *plan_id && !l.is_concept);
                if forward {
                    if let Some(first) = first_impl { first } else { return; }
                } else if let Some(first) = first_impl {
                    if first > 0 { first - 1 } else { return; }
                } else {
                    return;
                }
            }
            Some(VisiblePlanRow::ConceptHeader { ref plan_id, .. }) => {
                let first_concept = leaves.iter().position(|l| l.plan_id == *plan_id && l.is_concept);
                if forward {
                    if let Some(first) = first_concept { first } else { return; }
                } else if let Some(first) = first_concept {
                    if first > 0 { first - 1 } else { return; }
                } else {
                    return;
                }
            }
            Some(VisiblePlanRow::PlanHeader { ref plan_id, .. }) => {
                let first_in_plan = leaves.iter().position(|l| l.plan_id == *plan_id);
                if forward {
                    if let Some(first) = first_in_plan { first } else { return; }
                } else if let Some(first) = first_in_plan {
                    if first > 0 { first - 1 } else { return; }
                } else {
                    return;
                }
            }
            None => {
                if forward { 0 } else { leaves.len().saturating_sub(1) }
            }
        };

        let target = leaves[target_idx].clone();

        self.expanded_plans.clear();
        if target.item_id.is_some() {
            self.expanded_plans.insert(target.plan_id.clone());
        }

        self.expanded_items.clear();
        self.collapsed_sections.clear();

        for anc in &target.ancestor_item_ids {
            let key = if target.is_concept {
                format!("c:{}:{}", target.plan_id, anc)
            } else {
                format!("{}:{}", target.plan_id, anc)
            };
            self.expanded_items.insert(key);
        }

        if let Some(ref tid) = target.item_id {
            if target.is_expandable {
                let key = if target.is_concept {
                    format!("c:{}:{}", target.plan_id, tid)
                } else {
                    format!("{}:{}", target.plan_id, tid)
                };
                self.expanded_items.insert(key);
            }
        }

        let rows = self.flatten_rows();
        if let Some(pos) = rows.iter().position(|r| match (r, &target.item_id, target.is_concept) {
            (VisiblePlanRow::PlanHeader { plan_id, .. }, None, _) => plan_id == &target.plan_id,
            (VisiblePlanRow::ConceptItem { plan_id, item_id, .. }, Some(tid), true) => {
                plan_id == &target.plan_id && item_id == tid
            }
            (VisiblePlanRow::PlanItem { plan_id, item_id, .. }, Some(tid), false) => {
                plan_id == &target.plan_id && item_id == tid
            }
            _ => false,
        }) {
            self.selected_index = pos;
        }

        self.clamp_selection();
        self.scroll_to_selected();
    }
}

fn status_style(status: PlanItemStatus) -> Style {
    let engine = current_engine();
    let palette = engine.palette();
    let inverse = engine.color(palette.text_inverse);
    match status {
        PlanItemStatus::Discussion => engine.bg(palette.button_active_bg).fg(inverse).bold(),
        PlanItemStatus::Todo => engine.bg(palette.status_warning).fg(inverse).bold(),
        PlanItemStatus::InProgress => engine.bg(palette.status_info).fg(inverse).bold(),
        PlanItemStatus::Done => engine.bg(palette.status_success).fg(inverse).bold(),
        PlanItemStatus::Blocked => engine.bg(palette.status_error).fg(inverse).bold(),
        PlanItemStatus::Cancelled => engine.bg(palette.text_muted).fg(inverse).bold(),
    }
}

/// Compact indicator marking the row anchored by the active discussion focus.
fn focus_badge() -> Span<'static> {
    let engine = current_engine();
    let palette = engine.palette();
    Span::styled(
        " [FOCUS]",
        engine
            .bg(palette.status_success)
            .fg(engine.color(palette.text_inverse))
            .bold(),
    )
}

fn pad_line_to_width(mut line: Line<'static>, width: usize, fill_style: Style) -> Line<'static> {
    let current_width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    if current_width < width {
        line.spans.push(Span::styled(" ".repeat(width - current_width), fill_style));
    }
    line
}

fn prepare_plan_markdown(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() <= 1 {
        return trimmed.to_string();
    }

    let min_tail_indent = lines[1..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0);

    if min_tail_indent > 0 {
        let prefix = " ".repeat(min_tail_indent);
        let mut out = String::with_capacity(trimmed.len());
        out.push_str(lines[0]);
        for line in &lines[1..] {
            out.push('\n');
            // Whitespace-only lines (including multi-byte characters such
            // as NBSP U+00A0 or U+3000) are excluded from the indent
            // computation, so they must never be sliced by a byte offset.
            if line.trim().is_empty() {
                continue;
            }
            if let Some(stripped) = line.strip_prefix(prefix.as_str()) {
                out.push_str(stripped);
            } else {
                out.push_str(line.trim_start());
            }
        }
        out
    } else {
        trimmed.to_string()
    }
}

fn compute_unified_plan_lines(
    rows: &[VisiblePlanRow],
    selected_idx: usize,
    width: usize,
    theme_mode: crate::config::ThemeMode,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let engine = current_engine();
    let palette = engine.palette();
    let mut all_lines = Vec::new();
    let mut offsets = Vec::with_capacity(rows.len());

    for (idx, row) in rows.iter().enumerate() {
        let is_selected = idx == selected_idx;
        offsets.push(all_lines.len());

        let row_style = if is_selected {
            engine.bg(palette.overlay)
        } else {
            Style::default()
        };

        let gutter = if is_selected {
            Span::styled("▌", engine.fg(palette.status_info).bold())
        } else {
            Span::raw(" ")
        };

        match row {
            VisiblePlanRow::PlanHeader {
                plan_id,
                title,
                description,
                total,
                completed,
                status,
                is_expanded,
                focused,
            } => {
                if idx > 0 {
                    all_lines.push(Line::raw(""));
                }

                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let stats_str = format!("{}/{} done", completed, total);

                let mut spans = vec![
                    gutter,
                    Span::raw(" "),
                    Span::styled(fold_icon, engine.fg(palette.status_warning).bold()),
                    Span::styled(format!(" {} ", status), status_style(*status)),
                    Span::raw(" "),
                    Span::styled(
                        format!("[{}] ", plan_id),
                        engine.fg(palette.status_info).bold(),
                    ),
                    Span::styled(title.clone(), engine.fg(palette.text_primary).bold()),
                    Span::styled(
                        format!(" ({})", stats_str),
                        if is_selected {
                            engine.fg(palette.text_primary)
                        } else {
                            engine.fg(palette.text_muted)
                        },
                    ),
                ];
                if *focused {
                    spans.push(focus_badge());
                }

                let mut header_line = Line::from(spans).style(row_style);
                if is_selected {
                    header_line = pad_line_to_width(header_line, width, row_style);
                }
                all_lines.extend(wrap_lines(vec![header_line], width));

                if *is_expanded {
                    if let Some(desc) = description {
                        let prepared = prepare_plan_markdown(desc);
                        if !prepared.is_empty() {
                            let mut desc_lines = Vec::new();
                            let mut theme =
                                crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                            theme.text_style = engine.fg(palette.text_secondary);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix = Some(Span::styled(
                                "    │ ",
                                engine.fg(palette.text_muted),
                            ));
                            let md = crate::markdown::render_markdown(&prepared, &theme);
                            if !md.is_empty() {
                                desc_lines.extend(md);
                            } else {
                                for line in prepared.lines() {
                                    desc_lines.push(Line::from(vec![
                                        Span::styled(
                                            "    │ ",
                                            engine.fg(palette.text_muted),
                                        ),
                                        Span::styled(
                                            line.to_string(),
                                            engine.fg(palette.text_secondary),
                                        ),
                                    ]));
                                }
                            }
                            all_lines.extend(wrap_lines(desc_lines, width));
                        }
                    }
                }
            }
            VisiblePlanRow::ConceptHeader {
                count,
                is_expanded,
                ..
            } => {
                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let spans = vec![
                    gutter,
                    Span::raw("  "),
                    Span::styled(fold_icon, engine.fg(palette.button_active_bg).bold()),
                    Span::styled(
                        " CONCEPT ",
                        engine
                            .bg(palette.button_active_bg)
                            .fg(engine.color(palette.text_inverse))
                            .bold(),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        "Architectural Intent & Invariants",
                        engine.fg(palette.text_primary).bold(),
                    ),
                    Span::styled(
                        format!(" ({} item{})", count, if *count == 1 { "" } else { "s" }),
                        if is_selected {
                            engine.fg(palette.text_primary)
                        } else {
                            engine.fg(palette.text_muted)
                        },
                    ),
                ];
                let mut line = Line::from(spans).style(row_style);
                if is_selected {
                    line = pad_line_to_width(line, width, row_style);
                }
                all_lines.extend(wrap_lines(vec![line], width));
            }
            VisiblePlanRow::ConceptItem {
                item_id,
                title,
                details,
                depth,
                is_expandable,
                is_expanded,
                focused,
                ..
            } => {
                let indent = "  ".repeat(*depth);
                let fold_icon = if *is_expandable {
                    if *is_expanded { "▼ " } else { "▶ " }
                } else if is_selected {
                    "▶ "
                } else {
                    "◆ "
                };
                let fold_style = if *is_expandable {
                    engine.fg(palette.button_active_bg).bold()
                } else if is_selected {
                    engine.fg(palette.status_info).bold()
                } else {
                    engine.fg(palette.text_muted)
                };
                let title_style = if is_selected {
                    engine.fg(palette.text_primary).bold()
                } else {
                    engine.fg(palette.button_active_bg)
                };
                let clean_title = title.replace(['\r', '\n'], " ").trim().to_string();

                let mut spans = vec![
                    gutter,
                    Span::raw(indent.clone()),
                    Span::styled(fold_icon, fold_style),
                    Span::styled(format!("#{} {}", item_id, clean_title), title_style),
                ];
                if *focused {
                    spans.push(focus_badge());
                }
                let mut row_line = Line::from(spans).style(row_style);
                if is_selected {
                    row_line = pad_line_to_width(row_line, width, row_style);
                }
                let mut row_lines = vec![row_line];

                if *is_expanded {
                    if let Some(det) = details {
                        let prepared = prepare_plan_markdown(det);
                        if !prepared.is_empty() {
                            let mut theme =
                                crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                            theme.text_style = engine.fg(palette.text_secondary);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix = Some(Span::styled(
                                format!("{}    │ ", indent),
                                engine.fg(palette.text_muted),
                            ));
                            let md = crate::markdown::render_markdown(&prepared, &theme);
                            if !md.is_empty() {
                                row_lines.extend(md);
                            }
                        }
                    }
                }
                all_lines.extend(wrap_lines(row_lines, width));
            }
            VisiblePlanRow::ImplementationHeader {
                total,
                completed,
                is_expanded,
                ..
            } => {
                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let stats_str = format!("{}/{} done", completed, total);
                let spans = vec![
                    gutter,
                    Span::raw("  "),
                    Span::styled(fold_icon, engine.fg(palette.status_info).bold()),
                    Span::styled(
                        " IMPLEMENTATION ",
                        engine
                            .bg(palette.status_info)
                            .fg(engine.color(palette.text_inverse))
                            .bold(),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        "Execution Blueprint",
                        engine.fg(palette.text_primary).bold(),
                    ),
                    Span::styled(
                        format!(" ({})", stats_str),
                        if is_selected {
                            engine.fg(palette.text_primary)
                        } else {
                            engine.fg(palette.text_muted)
                        },
                    ),
                ];
                let mut line = Line::from(spans).style(row_style);
                if is_selected {
                    line = pad_line_to_width(line, width, row_style);
                }
                all_lines.extend(wrap_lines(vec![line], width));
            }
            VisiblePlanRow::PlanItem {
                item_id,
                title,
                details,
                status,
                files,
                readonly_files,
                concept_refs,
                depth,
                has_children,
                has_details: _,
                is_expandable,
                is_expanded,
                focused,
                ..
            } => {
                let indent = "  ".repeat(*depth);

                let fold_icon = if *is_expandable {
                    if *is_expanded { "▼ " } else { "▶ " }
                } else if is_selected {
                    "▶ "
                } else if *status == PlanItemStatus::Discussion {
                    "? "
                } else if *status == PlanItemStatus::Blocked {
                    "! "
                } else {
                    "• "
                };

                let fold_style = if *is_expandable {
                    engine.fg(palette.status_warning).bold()
                } else if is_selected {
                    engine.fg(palette.status_info).bold()
                } else {
                    engine.fg(palette.text_muted)
                };

                let title_style = if is_selected {
                    engine.fg(palette.text_primary).bold()
                } else if *status == PlanItemStatus::Done || *status == PlanItemStatus::Cancelled {
                    engine.fg(palette.text_secondary)
                } else {
                    Style::default().bold()
                };

                let clean_title = title.replace(['\r', '\n'], " ").trim().to_string();

                let mut spans = vec![
                    gutter,
                    Span::raw(indent.clone()),
                    Span::styled(fold_icon, fold_style),
                    Span::styled(format!(" {} ", status), status_style(*status)),
                    Span::raw(" "),
                    Span::styled(format!("#{} {}", item_id, clean_title), title_style),
                ];

                if !*has_children && (!files.is_empty() || !readonly_files.is_empty()) {
                    let mut badge_parts = Vec::new();
                    if !files.is_empty() {
                        badge_parts.push(format!(
                            "{} file{}",
                            files.len(),
                            if files.len() == 1 { "" } else { "s" }
                        ));
                    }
                    if !readonly_files.is_empty() {
                        badge_parts.push(format!("{} ref", readonly_files.len()));
                    }
                    spans.push(Span::styled(
                        format!(" [{}]", badge_parts.join(" + ")),
                        engine.fg(palette.text_muted),
                    ));
                }
                if *focused {
                    spans.push(focus_badge());
                }

                let mut row_line = Line::from(spans).style(row_style);
                if is_selected {
                    row_line = pad_line_to_width(row_line, width, row_style);
                }
                let mut row_lines = vec![row_line];

                if *is_expanded {
                    if !*has_children && !files.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(
                                format!("{}    │ ", indent),
                                engine.fg(palette.text_muted),
                            ),
                            Span::styled("files: ", engine.fg(palette.text_muted)),
                            Span::styled(files.join(", "), engine.fg(palette.status_info)),
                        ]));
                    }
                    if !*has_children && !readonly_files.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(
                                format!("{}    │ ", indent),
                                engine.fg(palette.text_muted),
                            ),
                            Span::styled("ref: ", engine.fg(palette.text_muted)),
                            Span::styled(
                                readonly_files.join(", "),
                                engine.fg(palette.status_info),
                            ),
                        ]));
                    }
                    if !concept_refs.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(
                                format!("{}    │ ", indent),
                                engine.fg(palette.text_muted),
                            ),
                            Span::styled("concept: ", engine.fg(palette.text_muted)),
                            Span::styled(
                                concept_refs.join(", "),
                                engine.fg(palette.button_active_bg),
                            ),
                        ]));
                    }
                    if let Some(det) = details {
                        let prepared = prepare_plan_markdown(det);
                        if !prepared.is_empty() {
                            let mut theme =
                                crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                            theme.text_style = engine.fg(palette.text_secondary);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix = Some(Span::styled(
                                format!("{}    │ ", indent),
                                engine.fg(palette.text_muted),
                            ));
                            let md = crate::markdown::render_markdown(&prepared, &theme);
                            if !md.is_empty() {
                                row_lines.extend(md);
                            }
                        }
                    }
                }

                all_lines.extend(wrap_lines(row_lines, width));
            }
        }
    }

    (all_lines, offsets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ConceptItem, PlanItem};

    #[test]
    fn test_plans_collapsed_by_default() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "test-plan".to_string(),
            title: "Test Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                files: vec![],
                readonly_files: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        };
        view.plans = vec![plan];

        // By default, plans are collapsed
        assert!(!view.is_plan_expanded("test-plan"));
        let rows = view.flatten_rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0], VisiblePlanRow::PlanHeader { is_expanded: false, .. }));

        // Toggle expands the plan
        view.toggle_plan_expanded("test-plan");
        assert!(view.is_plan_expanded("test-plan"));
        let rows = view.flatten_rows();
        assert_eq!(rows.len(), 2); // Header + Step 1

        // Toggle again collapses it
        view.toggle_plan_expanded("test-plan");
        assert!(!view.is_plan_expanded("test-plan"));

        // toggle_fold_all expands then collapses
        view.toggle_fold_all();
        assert!(view.is_plan_expanded("test-plan"));
        view.toggle_fold_all();
        assert!(!view.is_plan_expanded("test-plan"));
    }

    #[test]
    fn test_two_section_plan_flatten_and_toggles() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "auth-feature".to_string(),
            title: "Authentication".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            concept: vec![ConceptItem {
                id: "c-1".to_string(),
                title: "JWT Token Spec".to_string(),
                details: Some("Tokens must be signed".to_string()),
                children: vec![],
            }],
            items: vec![PlanItem {
                id: "s-1".to_string(),
                title: "Create JWT Signer".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                files: vec!["src/jwt.rs".to_string()],
                readonly_files: vec![],
                concept_refs: vec!["c-1".to_string()],
                depends_on: vec![],
                children: vec![],
            }],
        };
        view.plans = vec![plan];

        view.toggle_plan_expanded("auth-feature");
        let rows = view.flatten_rows();
        // PlanHeader, ConceptHeader, ConceptItem, ImplementationHeader, PlanItem
        assert_eq!(rows.len(), 5);
        assert!(matches!(rows[0], VisiblePlanRow::PlanHeader { .. }));
        assert!(matches!(rows[1], VisiblePlanRow::ConceptHeader { count: 1, is_expanded: true, .. }));
        assert!(matches!(rows[2], VisiblePlanRow::ConceptItem { ref item_id, .. } if item_id == "c-1"));
        assert!(matches!(rows[3], VisiblePlanRow::ImplementationHeader { is_expanded: true, .. }));
        assert!(matches!(rows[4], VisiblePlanRow::PlanItem { ref item_id, .. } if item_id == "s-1"));

        // Toggle collapse concept section
        view.toggle_concept_section_expanded("auth-feature");
        let rows_collapsed_concept = view.flatten_rows();
        // PlanHeader, ConceptHeader (collapsed), ImplementationHeader, PlanItem
        assert_eq!(rows_collapsed_concept.len(), 4);
        assert!(matches!(rows_collapsed_concept[1], VisiblePlanRow::ConceptHeader { is_expanded: false, .. }));
    }

    #[test]
    fn test_file_badges_render_only_for_leaf_items() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "leaf-files".to_string(),
            title: "Leaf Files".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Phase".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                files: vec!["parent.rs".to_string()],
                readonly_files: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![PlanItem {
                    id: "1.1".to_string(),
                    title: "Leaf Task".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    files: vec!["leaf.rs".to_string()],
                    readonly_files: vec![],
                    concept_refs: vec![],
                    depends_on: vec![],
                    children: vec![],
                }],
            }],
        };
        view.plans = vec![plan];
        view.expand_all();

        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_plan_lines(&rows, usize::MAX, 200, crate::config::ThemeMode::Auto);
        let mut text = String::new();
        for line in &lines {
            for span in &line.spans {
                text.push_str(&span.content);
            }
        }

        assert!(text.contains("Leaf Task"), "leaf row should render: {}", text);
        // Exactly one file badge: the leaf task. The parent phase carries a
        // non-empty `files` list in memory but must never render a badge.
        assert_eq!(
            text.matches("file]").count(),
            1,
            "only leaf rows may show file badges: {}",
            text
        );
    }

    #[test]
    fn test_reference_files_badge_and_details() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "ref-files".to_string(),
            title: "Reference Files".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Wire Auth".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                files: vec!["src/auth.rs".to_string()],
                readonly_files: vec!["contracts/api.md".to_string(), "contracts/types.rs".to_string()],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        };
        view.plans = vec![plan];
        view.expand_all();

        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_plan_lines(&rows, usize::MAX, 200, crate::config::ThemeMode::Auto);
        let mut text = String::new();
        for line in &lines {
            for span in &line.spans {
                text.push_str(&span.content);
            }
        }

        assert!(
            text.contains("[1 file + 2 ref]"),
            "badge should count target and reference files: {}",
            text
        );
        assert!(text.contains("files: src/auth.rs"), "target files line: {}", text);
        assert!(
            text.contains("ref: contracts/api.md, contracts/types.rs"),
            "reference files line: {}",
            text
        );
    }

    #[test]
    fn test_prepare_plan_markdown_does_not_slice_multibyte_whitespace() {
        // A line made only of NBSP (U+00A0) is Unicode whitespace and is
        // excluded from the indent computation, but its two-byte width
        // must never be sliced by a one-byte offset.
        let raw = "Title\n line one\n\u{00A0}\n line three";
        let prepared = prepare_plan_markdown(raw);
        assert_eq!(prepared, "Title\nline one\n\nline three");
    }

    #[test]
    fn test_accordion_navigation_navigates_strictly_across_leaves() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "nested-plan".to_string(),
            title: "Nested Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            concept: vec![ConceptItem {
                id: "c-1".to_string(),
                title: "Concept Root".to_string(),
                details: None,
                children: vec![
                    ConceptItem {
                        id: "c-1.1".to_string(),
                        title: "Concept Leaf 1".to_string(),
                        details: None,
                        children: vec![],
                    },
                    ConceptItem {
                        id: "c-1.2".to_string(),
                        title: "Concept Leaf 2".to_string(),
                        details: None,
                        children: vec![],
                    },
                ],
            }],
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Parent Step 1".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    files: vec![],
                    readonly_files: vec![],
                    concept_refs: vec![],
                    depends_on: vec![],
                    children: vec![
                        PlanItem {
                            id: "1.1".to_string(),
                            title: "Step 1.1 Leaf".to_string(),
                            details: None,
                            status: PlanItemStatus::Todo,
                            files: vec![],
                            readonly_files: vec![],
                            concept_refs: vec![],
                            depends_on: vec![],
                            children: vec![],
                        },
                        PlanItem {
                            id: "1.2".to_string(),
                            title: "Step 1.2 Leaf".to_string(),
                            details: None,
                            status: PlanItemStatus::Todo,
                            files: vec![],
                            readonly_files: vec![],
                            concept_refs: vec![],
                            depends_on: vec![],
                            children: vec![],
                        },
                    ],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Parent Step 2".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    files: vec![],
                    readonly_files: vec![],
                    concept_refs: vec![],
                    depends_on: vec![],
                    children: vec![PlanItem {
                        id: "2.1".to_string(),
                        title: "Step 2.1 Leaf".to_string(),
                        details: None,
                        status: PlanItemStatus::Todo,
                        files: vec![],
                        readonly_files: vec![],
                        concept_refs: vec![],
                        depends_on: vec![],
                        children: vec![],
                    }],
                },
            ],
        };
        view.plans = vec![plan];

        // 1. Initial navigation forward lands on the first leaf: c-1.1
        view.accordion_navigate(true);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::ConceptItem { ref item_id, .. }) if item_id == "c-1.1"
        ));

        // 2. Next leaf in same concept parent: c-1.2
        view.accordion_navigate(true);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::ConceptItem { ref item_id, .. }) if item_id == "c-1.2"
        ));

        // 3. At boundary of concept c-1, C-n ascends and descends to Implementation first leaf: 1.1 (skipping parent 1)
        view.accordion_navigate(true);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::PlanItem { ref item_id, .. }) if item_id == "1.1"
        ));

        // 4. Next sibling leaf inside Step 1: 1.2
        view.accordion_navigate(true);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::PlanItem { ref item_id, .. }) if item_id == "1.2"
        ));

        // 5. At boundary of Step 1, C-n ascends to Step 1 parent, finds Step 2, and descends into Step 2.1 leaf
        view.accordion_navigate(true);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::PlanItem { ref item_id, .. }) if item_id == "2.1"
        ));

        // 6. At the last leaf of the entire tree, forward does not cycle
        view.accordion_navigate(true);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::PlanItem { ref item_id, .. }) if item_id == "2.1"
        ));

        // 7. Backward navigation from 2.1 ascends to Step 2, moves to previous sibling Step 1 and descends to its last leaf: 1.2
        view.accordion_navigate(false);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::PlanItem { ref item_id, .. }) if item_id == "1.2"
        ));

        // 8. Backward again: 1.1
        view.accordion_navigate(false);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::PlanItem { ref item_id, .. }) if item_id == "1.1"
        ));

        // 9. Backward from 1.1 ascends out of Implementation into Concept's last leaf: c-1.2
        view.accordion_navigate(false);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::ConceptItem { ref item_id, .. }) if item_id == "c-1.2"
        ));
    }

    #[test]
    fn test_focus_badge_marks_the_anchored_plan_item() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "focus-plan".to_string(),
            title: "Focus Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                files: vec![],
                readonly_files: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        };
        view.plans = vec![plan];
        view.expand_all();
        view.discussion_focus =
            tauqe_protocol::DiscussionFocus::plan("focus-plan", vec!["1".to_string()]);

        let rows = view.flatten_rows();
        assert!(matches!(
            &rows[0],
            VisiblePlanRow::PlanHeader { focused: false, .. }
        ));
        assert!(rows.iter().any(|r| matches!(
            r,
            VisiblePlanRow::PlanItem { item_id, focused: true, .. } if item_id == "1"
        )));

        let (lines, _) =
            compute_unified_plan_lines(&rows, usize::MAX, 200, crate::config::ThemeMode::Auto);
        let mut text = String::new();
        for line in &lines {
            for span in &line.spans {
                text.push_str(&span.content);
            }
        }
        assert!(text.contains("[FOCUS]"), "focus badge should render: {}", text);
    }

    #[test]
    fn test_focus_badge_marks_the_anchored_whole_plan() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "focus-plan".to_string(),
            title: "Focus Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            concept: vec![],
            items: vec![],
        };
        view.plans = vec![plan];
        view.discussion_focus = tauqe_protocol::DiscussionFocus::plan("focus-plan", vec![]);

        let rows = view.flatten_rows();
        assert!(matches!(
            &rows[0],
            VisiblePlanRow::PlanHeader { focused: true, .. }
        ));
    }
}
