use std::collections::HashSet;

use tauqe_protocol::{
    ArchitectTier, ConceptItem, DiscussionFocus, DiscussionTarget, ModelEntry, ModelRole, Plan,
    PlanItem, PlanItemStatus,
};

use crate::app::AppState;
use crate::components::model_selector::ModelSelector;
use crate::markdown::MarkdownBodyCache;
use crate::views::recenter::RecenterState;

pub mod input;
pub mod render;
pub mod tree;

pub use self::input::{handle_plans_key, PLANS_COMMAND_MAP};
pub use self::render::render_plans_view;

// Tree-line rendering lives in its own module; the facade re-exports the three
// entry points its renderer and the Review view consume, so those callers keep
// importing one tree vocabulary from the plans module.
pub(crate) use self::tree::{architect_badge, compute_unified_plan_lines, status_style};

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
        /// The Architect configuration that produced this plan's
        /// decomposition, rendered so the *What* role's provenance stays
        /// visible in the tree.
        architect: Option<ArchitectTier>,
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
        /// Diagnostic report of the semantic blocker that stopped the step.
        ///
        /// Present only while the step is blocked, so the reason survives the
        /// interrupted turn on disk and in the tree instead of being lost with
        /// the transient session.
        blocked_reason: Option<String>,
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
    pub fn new(plan_id: String, plan_title: String, architect_entries: Vec<ModelEntry>) -> Self {
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

/// Resolves the Architect model entries a decomposition dialog offers.
///
/// Configured Architect entries win; otherwise the resolved Architect role (or
/// the active model) backs the dialog so it always presents a valid selection.
/// Both the plan horizon refinement and the review finding decomposition dialogs
/// read this single helper, so the architect ladder can never drift between the
/// two refinement surfaces (Conventions §1.3).
pub fn architect_entries(st: &AppState) -> Vec<ModelEntry> {
    st.model_choice
        .roles
        .as_ref()
        .and_then(|roles| roles.entries.as_ref())
        .map(|entries| entries.architect.clone())
        .filter(|entries| !entries.is_empty())
        .unwrap_or_else(|| {
            let fallback_model = st
                .model_choice
                .resolve(&tauqe_protocol::ModelSelection::Role(ModelRole::Architect))
                .unwrap_or_else(|| st.active_model.clone());
            let fallback_effort = st
                .model_choice
                .roles
                .as_ref()
                .and_then(|roles| roles.efforts.as_ref())
                .and_then(|efforts| efforts.architect);
            vec![ModelEntry::new(fallback_model, fallback_effort)]
        })
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
    /// Rendered Markdown bodies of the expanded descriptions, keyed by the item
    /// that owns the description. The cache keeps a parsed body alive across
    /// frames, so moving the cursor or scrolling never re-parses the Markdown of
    /// the tree, and a changed description, indent, width, or palette drops only
    /// the body it invalidated (c-1, c-3).
    pub markdown_cache: MarkdownBodyCache,
    pub status_message: Option<String>,
    pub refining: bool,
    pub refining_plan_id: Option<String>,
    pub refine_dialog: Option<PlanRefineDialogState>,
    pub refine_blocked: Option<PlanRefineBlockedState>,
    pub theme_mode: crate::config::ThemeMode,
    /// Active discussion focus mirrored from the server, used to render the
    /// `[FOCUS]` indicator. Synced from `AppState::discussion_focus` on render.
    pub discussion_focus: DiscussionFocus,
    pub recenter_state: RecenterState,
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
        !self
            .collapsed_sections
            .contains(&format!("concept:{}", plan_id))
    }

    pub fn toggle_concept_section_expanded(&mut self, plan_id: &str) {
        let key = format!("concept:{}", plan_id);
        if !self.collapsed_sections.remove(&key) {
            self.collapsed_sections.insert(key);
        }
    }

    pub fn is_impl_section_expanded(&self, plan_id: &str) -> bool {
        !self
            .collapsed_sections
            .contains(&format!("impl:{}", plan_id))
    }

    pub fn toggle_impl_section_expanded(&mut self, plan_id: &str) {
        let key = format!("impl:{}", plan_id);
        if !self.collapsed_sections.remove(&key) {
            self.collapsed_sections.insert(key);
        }
    }

    pub fn is_concept_item_expanded(&self, plan_id: &str, item_id: &str) -> bool {
        self.expanded_items
            .contains(&format!("c:{}:{}", plan_id, item_id))
    }

    pub fn toggle_concept_item_expanded(&mut self, plan_id: &str, item_id: &str) {
        let key = format!("c:{}:{}", plan_id, item_id);
        if !self.expanded_items.remove(&key) {
            self.expanded_items.insert(key);
        }
    }

    pub fn is_item_expanded(&self, plan_id: &str, item_id: &str) -> bool {
        self.expanded_items
            .contains(&format!("{}:{}", plan_id, item_id))
    }

    pub fn toggle_item_expanded(&mut self, plan_id: &str, item_id: &str) {
        let key = format!("{}:{}", plan_id, item_id);
        if !self.expanded_items.remove(&key) {
            self.expanded_items.insert(key);
        }
    }

    pub fn is_all_expanded(&self) -> bool {
        !self.plans.is_empty()
            && self
                .plans
                .iter()
                .all(|p| self.expanded_plans.contains(&p.id))
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
            } if id.as_str() == plan_id => focused_items.iter().any(|fid| fid.as_str() == item_id),
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
                architect: plan.architect.clone(),
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
        let has_details = item
            .details
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
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
        let has_details = item
            .details
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let has_meta = !item.files.is_empty()
            || !item.readonly_files.is_empty()
            || !item.concept_refs.is_empty();
        let has_blocked_reason = item
            .blocked_reason
            .as_deref()
            .map(|reason| !reason.trim().is_empty())
            .unwrap_or(false);
        let is_expandable = has_children || has_details || has_meta || has_blocked_reason;
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
            blocked_reason: item.blocked_reason.clone(),
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

    /// Number of concept rows one concept subtree contributes to the tree.
    ///
    /// Counts the item itself plus, while it is expanded, the subtrees of its
    /// children, mirroring `flatten_concept_recursive` branch for branch.
    fn count_concept_rows(&self, plan_id: &str, item: &ConceptItem) -> usize {
        let mut count = 1;
        if !item.children.is_empty() && self.is_concept_item_expanded(plan_id, &item.id) {
            for child in &item.children {
                count += self.count_concept_rows(plan_id, child);
            }
        }
        count
    }

    /// Number of step rows one step subtree contributes to the tree.
    ///
    /// Counts the step itself plus, while it is expanded, the subtrees of its
    /// children, mirroring `flatten_item_recursive` branch for branch.
    fn count_item_rows(&self, plan_id: &str, item: &PlanItem) -> usize {
        let mut count = 1;
        if !item.children.is_empty() && self.is_item_expanded(plan_id, &item.id) {
            for child in &item.children {
                count += self.count_item_rows(plan_id, child);
            }
        }
        count
    }

    /// Number of rows the tree currently exposes, computed without allocating
    /// the row list.
    ///
    /// Selection and viewport bounds only need the count, so they read it here
    /// instead of building every owned row projection just to call `len()`. The
    /// walk mirrors `flatten_rows` branch for branch, and the regression tests
    /// pin the two to the same value in every fold state (c-2).
    pub fn row_count(&self) -> usize {
        let mut count = 0;
        for plan in &self.plans {
            count += 1; // The plan header.
            if self.is_plan_expanded(&plan.id) {
                if plan.concept.is_empty() {
                    for item in &plan.items {
                        count += self.count_item_rows(&plan.id, item);
                    }
                } else {
                    count += 1; // The concept section header.
                    if self.is_concept_section_expanded(&plan.id) {
                        for item in &plan.concept {
                            count += self.count_concept_rows(&plan.id, item);
                        }
                    }
                    count += 1; // The implementation section header.
                    if self.is_impl_section_expanded(&plan.id) {
                        for item in &plan.items {
                            count += self.count_item_rows(&plan.id, item);
                        }
                    }
                }
            }
        }
        count
    }

    pub fn selected_row(&self) -> Option<VisiblePlanRow> {
        let rows = self.flatten_rows();
        rows.get(self.selected_index).cloned()
    }

    pub fn clamp_selection(&mut self) {
        let count = self.row_count();
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

        let (lines, offsets) = compute_unified_plan_lines(
            &rows,
            sel,
            self.content_width.max(10),
            self.theme_mode,
            &mut self.markdown_cache,
        );
        let height = self.view_height as usize;
        let start = offsets.get(sel).copied().unwrap_or(0);
        let end = offsets.get(sel + 1).copied().unwrap_or(lines.len());
        let scroll = self.scroll as usize;

        // A row taller than the viewport cannot be shown whole, so the window
        // stays inside `[start, end - height]`: entering such a row from above
        // aligns it to the top of the screen, while a viewport already deep
        // inside it keeps its offset instead of snapping back to the first line
        // of the row. A row that fits is revealed completely (c-scroll-visibility).
        let new_scroll = if end.saturating_sub(start) > height {
            scroll.clamp(start, end.saturating_sub(height))
        } else if start < scroll {
            start
        } else if end > scroll + height {
            end.saturating_sub(height)
        } else {
            scroll
        };
        self.scroll = new_scroll as u16;
    }

    /// Positions the viewport around the selected plan row using the three-beat
    /// Emacs `recenter-top-bottom` cycle (center -> top -> bottom).
    pub fn recenter_selected(&mut self) {
        let rows = self.flatten_rows();
        if rows.is_empty() || self.view_height == 0 {
            return;
        }
        let sel = self.selected_index.min(rows.len().saturating_sub(1));
        self.selected_index = sel;

        let (lines, offsets) = compute_unified_plan_lines(
            &rows,
            sel,
            self.content_width.max(10),
            self.theme_mode,
            &mut self.markdown_cache,
        );
        let start = offsets.get(sel).copied().unwrap_or(0);
        let end = offsets.get(sel + 1).copied().unwrap_or(lines.len());
        self.scroll = self
            .recenter_state
            .recenter(sel, start, end, lines.len(), self.view_height);
    }

    /// Collapses every previously opened plan except `plan_id`, expands the
    /// given plan, and moves the cursor to its header row.
    ///
    /// A `save` action creates or fully replaces a plan, so the developer should
    /// land directly on it instead of hunting for it among the plans that were
    /// already expanded (c-4, Conventions.md §4). A request for a plan absent
    /// from the list is a safe no-op, so a pruned plan never clears the tree.
    pub fn focus_plan_header(&mut self, plan_id: &str) {
        if !self.plans.iter().any(|plan| plan.id.as_str() == plan_id) {
            return;
        }

        // Collapse every other plan and expand only the target so the fresh
        // plan stands alone in the tree.
        self.expanded_plans.clear();
        self.expanded_plans.insert(plan_id.to_string());

        let target = self.flatten_rows().iter().position(|row| {
            matches!(
                row,
                VisiblePlanRow::PlanHeader { plan_id: header_id, .. }
                    if header_id.as_str() == plan_id
            )
        });
        if let Some(index) = target {
            self.selected_index = index;
        }
        self.clamp_selection();
        self.scroll_to_selected();
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
                let has_details = item
                    .details
                    .as_deref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
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
                let has_details = item
                    .details
                    .as_deref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
                let has_meta = !item.files.is_empty()
                    || !item.readonly_files.is_empty()
                    || !item.concept_refs.is_empty();
                let has_blocked_reason = item
                    .blocked_reason
                    .as_deref()
                    .map(|reason| !reason.trim().is_empty())
                    .unwrap_or(false);
                let is_expandable = has_details || has_meta || has_blocked_reason;
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
            Some(VisiblePlanRow::ConceptItem {
                ref plan_id,
                ref item_id,
                ..
            }) => {
                if let Some(pos) = leaves.iter().position(|l| {
                    l.plan_id == *plan_id && l.is_concept && l.item_id.as_deref() == Some(item_id)
                }) {
                    if forward {
                        if pos + 1 < leaves.len() {
                            pos + 1
                        } else {
                            return;
                        }
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
                        if let Some(first) = first_descendant {
                            first
                        } else {
                            return;
                        }
                    } else if let Some(first) = first_descendant {
                        if first > 0 {
                            first - 1
                        } else {
                            return;
                        }
                    } else {
                        return;
                    }
                }
            }
            Some(VisiblePlanRow::PlanItem {
                ref plan_id,
                ref item_id,
                ..
            }) => {
                if let Some(pos) = leaves.iter().position(|l| {
                    l.plan_id == *plan_id && !l.is_concept && l.item_id.as_deref() == Some(item_id)
                }) {
                    if forward {
                        if pos + 1 < leaves.len() {
                            pos + 1
                        } else {
                            return;
                        }
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
                        if let Some(first) = first_descendant {
                            first
                        } else {
                            return;
                        }
                    } else if let Some(first) = first_descendant {
                        if first > 0 {
                            first - 1
                        } else {
                            return;
                        }
                    } else {
                        return;
                    }
                }
            }
            Some(VisiblePlanRow::ImplementationHeader { ref plan_id, .. }) => {
                // The implementation section sits directly after the concept
                // section. Anchor the header at the first implementation leaf
                // when one exists, or just past the plan's last leaf when the
                // section is empty, so C-p still crosses into the concept
                // section's last leaf and C-n continues into the next plan
                // (c-5, Conventions.md §4).
                let enter = leaves
                    .iter()
                    .position(|l| l.plan_id == *plan_id && !l.is_concept)
                    .or_else(|| {
                        leaves
                            .iter()
                            .rposition(|l| l.plan_id == *plan_id)
                            .map(|last| last + 1)
                    });
                match enter {
                    Some(idx) if forward && idx < leaves.len() => idx,
                    Some(idx) if !forward && idx > 0 => idx - 1,
                    _ => return,
                }
            }
            Some(VisiblePlanRow::ConceptHeader { ref plan_id, .. }) => {
                // The concept section opens a plan, so its first leaf is the
                // plan's first leaf; fall back to the plan's first leaf when
                // the section unexpectedly carries none.
                let enter = leaves
                    .iter()
                    .position(|l| l.plan_id == *plan_id && l.is_concept)
                    .or_else(|| leaves.iter().position(|l| l.plan_id == *plan_id));
                match enter {
                    Some(idx) if forward && idx < leaves.len() => idx,
                    Some(idx) if !forward && idx > 0 => idx - 1,
                    _ => return,
                }
            }
            Some(VisiblePlanRow::PlanHeader { ref plan_id, .. }) => {
                let first_in_plan = leaves.iter().position(|l| l.plan_id == *plan_id);
                if forward {
                    if let Some(first) = first_in_plan {
                        first
                    } else {
                        return;
                    }
                } else if let Some(first) = first_in_plan {
                    if first > 0 {
                        first - 1
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            }
            None => {
                if forward {
                    0
                } else {
                    leaves.len().saturating_sub(1)
                }
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
        if let Some(pos) = rows
            .iter()
            .position(|r| match (r, &target.item_id, target.is_concept) {
                (VisiblePlanRow::PlanHeader { plan_id, .. }, None, _) => plan_id == &target.plan_id,
                (
                    VisiblePlanRow::ConceptItem {
                        plan_id, item_id, ..
                    },
                    Some(tid),
                    true,
                ) => plan_id == &target.plan_id && item_id == tid,
                (
                    VisiblePlanRow::PlanItem {
                        plan_id, item_id, ..
                    },
                    Some(tid),
                    false,
                ) => plan_id == &target.plan_id && item_id == tid,
                _ => false,
            })
        {
            self.selected_index = pos;
        }

        self.clamp_selection();
        self.scroll_to_selected();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ConceptItem, PlanItem};

    fn concept_leaf(id: &str, title: &str) -> ConceptItem {
        ConceptItem {
            id: id.to_string(),
            title: title.to_string(),
            details: None,
            children: vec![],
        }
    }

    fn leaf_item(id: &str, title: &str) -> PlanItem {
        PlanItem {
            id: id.to_string(),
            title: title.to_string(),
            details: None,
            status: PlanItemStatus::Todo,
            blocked_reason: None,
            files: vec![],
            readonly_files: vec![],
            concept_refs: vec![],
            depends_on: vec![],
            children: vec![],
        }
    }

    fn parent_item(id: &str, title: &str, children: Vec<PlanItem>) -> PlanItem {
        PlanItem {
            children,
            ..leaf_item(id, title)
        }
    }

    #[test]
    fn test_plans_collapsed_by_default() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "test-plan".to_string(),
            last_replanned_step: None,
            title: "Test Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            architect: None,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                blocked_reason: None,
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
        assert!(matches!(
            rows[0],
            VisiblePlanRow::PlanHeader {
                is_expanded: false,
                ..
            }
        ));

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
            last_replanned_step: None,
            title: "Authentication".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            architect: None,
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
                blocked_reason: None,
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
        assert!(matches!(
            rows[1],
            VisiblePlanRow::ConceptHeader {
                count: 1,
                is_expanded: true,
                ..
            }
        ));
        assert!(
            matches!(rows[2], VisiblePlanRow::ConceptItem { ref item_id, .. } if item_id == "c-1")
        );
        assert!(matches!(
            rows[3],
            VisiblePlanRow::ImplementationHeader {
                is_expanded: true,
                ..
            }
        ));
        assert!(
            matches!(rows[4], VisiblePlanRow::PlanItem { ref item_id, .. } if item_id == "s-1")
        );

        // Toggle collapse concept section
        view.toggle_concept_section_expanded("auth-feature");
        let rows_collapsed_concept = view.flatten_rows();
        // PlanHeader, ConceptHeader (collapsed), ImplementationHeader, PlanItem
        assert_eq!(rows_collapsed_concept.len(), 4);
        assert!(matches!(
            rows_collapsed_concept[1],
            VisiblePlanRow::ConceptHeader {
                is_expanded: false,
                ..
            }
        ));
    }

    #[test]
    fn test_accordion_navigation_navigates_strictly_across_leaves() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "nested-plan".to_string(),
            last_replanned_step: None,
            title: "Nested Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            architect: None,
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
                    blocked_reason: None,
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
                            blocked_reason: None,
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
                            blocked_reason: None,
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
                    blocked_reason: None,
                    files: vec![],
                    readonly_files: vec![],
                    concept_refs: vec![],
                    depends_on: vec![],
                    children: vec![PlanItem {
                        id: "2.1".to_string(),
                        title: "Step 2.1 Leaf".to_string(),
                        details: None,
                        status: PlanItemStatus::Todo,
                        blocked_reason: None,
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
    fn test_accordion_crosses_concept_implementation_boundary_and_expands_ancestors() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "cross-plan".to_string(),
            title: "Cross Plan".to_string(),
            concept: vec![concept_leaf("c-1", "Concept 1")],
            items: vec![parent_item(
                "1",
                "Parent Step",
                vec![leaf_item("1.1", "Nested Step")],
            )],
            ..Default::default()
        };
        view.plans = vec![plan];
        view.expand_all();

        // Forward from the last Concept leaf lands on the first Implementation
        // leaf and expands its ancestors so it becomes visible.
        view.selected_index = view
            .flatten_rows()
            .iter()
            .position(
                |r| matches!(r, VisiblePlanRow::ConceptItem { item_id, .. } if item_id == "c-1"),
            )
            .expect("concept leaf present");
        view.accordion_navigate(true);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::PlanItem { ref item_id, .. }) if item_id == "1.1"
        ));
        assert!(view.is_item_expanded("cross-plan", "1"));

        // Backward from the first Implementation leaf returns to the last
        // Concept leaf across the same boundary.
        view.accordion_navigate(false);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::ConceptItem { ref item_id, .. }) if item_id == "c-1"
        ));
    }

    #[test]
    fn test_accordion_from_implementation_header_reaches_adjacent_extreme_leaves() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "header-plan".to_string(),
            title: "Header Plan".to_string(),
            concept: vec![
                concept_leaf("c-1", "Concept 1"),
                concept_leaf("c-2", "Concept 2"),
            ],
            items: vec![leaf_item("1", "Step 1"), leaf_item("2", "Step 2")],
            ..Default::default()
        };
        view.plans = vec![plan];
        view.expand_all();

        let header_index = view
            .flatten_rows()
            .iter()
            .position(|r| matches!(r, VisiblePlanRow::ImplementationHeader { .. }))
            .expect("implementation header present");

        // Backward from the header reaches the last Concept leaf.
        view.selected_index = header_index;
        view.accordion_navigate(false);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::ConceptItem { ref item_id, .. }) if item_id == "c-2"
        ));

        // Forward from the header enters the implementation section on its
        // first leaf.
        view.selected_index = header_index;
        view.accordion_navigate(true);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::PlanItem { ref item_id, .. }) if item_id == "1"
        ));
    }

    #[test]
    fn test_accordion_from_implementation_header_with_empty_implementation_reaches_last_concept_leaf(
    ) {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "concept-only".to_string(),
            title: "Concept Only".to_string(),
            concept: vec![
                concept_leaf("c-1", "Concept 1"),
                concept_leaf("c-2", "Concept 2"),
            ],
            items: vec![],
            ..Default::default()
        };
        view.plans = vec![plan];
        view.expand_all();

        view.selected_index = view
            .flatten_rows()
            .iter()
            .position(|r| matches!(r, VisiblePlanRow::ImplementationHeader { .. }))
            .expect("implementation header present");

        // Even with no implementation leaves, C-p crosses into the concept
        // section instead of stalling on the header.
        view.accordion_navigate(false);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::ConceptItem { ref item_id, .. }) if item_id == "c-2"
        ));
    }

    #[test]
    fn test_accordion_from_concept_header_reaches_first_concept_leaf() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "concept-only".to_string(),
            title: "Concept Only".to_string(),
            concept: vec![
                concept_leaf("c-1", "Concept 1"),
                concept_leaf("c-2", "Concept 2"),
            ],
            items: vec![],
            ..Default::default()
        };
        view.plans = vec![plan];
        view.expand_all();

        view.selected_index = view
            .flatten_rows()
            .iter()
            .position(|r| matches!(r, VisiblePlanRow::ConceptHeader { .. }))
            .expect("concept header present");

        view.accordion_navigate(true);
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::ConceptItem { ref item_id, .. }) if item_id == "c-1"
        ));
    }

    #[test]
    fn test_focus_badge_marks_the_anchored_whole_plan() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "focus-plan".to_string(),
            last_replanned_step: None,
            title: "Focus Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            architect: None,
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

    #[test]
    fn test_focus_plan_header_collapses_others_and_selects_the_target() {
        let mut view = PlansViewState::default();
        let first = Plan {
            id: "plan-a".to_string(),
            title: "Plan A".to_string(),
            ..Default::default()
        };
        let second = Plan {
            id: "plan-b".to_string(),
            title: "Plan B".to_string(),
            ..Default::default()
        };
        view.plans = vec![first, second];
        // Both plans start expanded before the focus request.
        view.expand_all();
        assert!(view.is_plan_expanded("plan-a"));
        assert!(view.is_plan_expanded("plan-b"));

        view.focus_plan_header("plan-b");

        // Only the target plan stays expanded; the cursor anchors on its header.
        assert!(!view.is_plan_expanded("plan-a"));
        assert!(view.is_plan_expanded("plan-b"));
        assert!(matches!(
            view.selected_row(),
            Some(VisiblePlanRow::PlanHeader { ref plan_id, .. }) if plan_id == "plan-b"
        ));
    }

    #[test]
    fn test_focus_plan_header_is_a_noop_for_unknown_plan() {
        let mut view = PlansViewState {
            plans: vec![Plan {
                id: "plan-a".to_string(),
                title: "Plan A".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        view.expand_all();
        view.selected_index = 0;

        view.focus_plan_header("missing");

        // Nothing changes: an unknown plan is ignored.
        assert!(view.is_plan_expanded("plan-a"));
        assert_eq!(view.selected_index, 0);
    }

    /// A plan whose concept section, implementation section, and nested items all
    /// carry expansion state, so every branch of the row walk is exercised.
    fn row_count_fixture() -> PlansViewState {
        PlansViewState {
            plans: vec![Plan {
                id: "count-plan".to_string(),
                title: "Count Plan".to_string(),
                description: Some("Plan description.".to_string()),
                concept: vec![ConceptItem {
                    id: "c-1".to_string(),
                    title: "Invariant".to_string(),
                    details: Some("Tokens must be signed.".to_string()),
                    children: vec![concept_leaf("c-1.1", "Nested Invariant")],
                }],
                items: vec![parent_item(
                    "1",
                    "Parent Step",
                    vec![leaf_item("1.1", "Leaf Step")],
                )],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn assert_row_count_matches(view: &PlansViewState, state: &str) {
        assert_eq!(
            view.row_count(),
            view.flatten_rows().len(),
            "row_count must match the flattened rows while {state}"
        );
    }

    #[test]
    fn test_row_count_matches_flattened_rows_across_fold_states() {
        let mut view = row_count_fixture();
        assert_row_count_matches(&view, "the plan is collapsed");

        view.toggle_plan_expanded("count-plan");
        assert_row_count_matches(&view, "the plan is expanded");

        view.toggle_concept_section_expanded("count-plan");
        assert_row_count_matches(&view, "the concept section is folded");
        view.toggle_concept_section_expanded("count-plan");
        assert_row_count_matches(&view, "the concept section is unfolded");

        view.toggle_impl_section_expanded("count-plan");
        assert_row_count_matches(&view, "the implementation section is folded");
        view.toggle_impl_section_expanded("count-plan");
        assert_row_count_matches(&view, "the implementation section is unfolded");

        view.toggle_concept_item_expanded("count-plan", "c-1");
        assert_row_count_matches(&view, "the concept item is expanded");
        view.toggle_concept_item_expanded("count-plan", "c-1");
        assert_row_count_matches(&view, "the concept item is folded");

        view.toggle_item_expanded("count-plan", "1");
        assert_row_count_matches(&view, "the parent step is expanded");

        view.expand_all();
        assert_row_count_matches(&view, "every row is expanded");

        view.collapse_all();
        assert_row_count_matches(&view, "every row is collapsed");
    }

    #[test]
    fn test_clamp_selection_clamps_against_the_row_count() {
        let mut view = PlansViewState {
            plans: vec![Plan {
                id: "clamp-plan".to_string(),
                title: "Clamp Plan".to_string(),
                items: vec![leaf_item("1", "Step")],
                ..Default::default()
            }],
            ..Default::default()
        };

        // A collapsed plan exposes only its header, so a cursor past the last row
        // clamps back to it.
        assert_eq!(view.row_count(), 1);
        view.selected_index = 7;
        view.clamp_selection();
        assert_eq!(view.selected_index, 0);

        // Expanding the plan reveals its header and its single step.
        view.toggle_plan_expanded("clamp-plan");
        assert_eq!(view.row_count(), 2);
        view.selected_index = 7;
        view.clamp_selection();
        assert_eq!(view.selected_index, 1);

        // Collapsing the plan again re-clamps the cursor to the only row.
        view.toggle_plan_expanded("clamp-plan");
        view.selected_index = 1;
        view.clamp_selection();
        assert_eq!(view.selected_index, 0);
    }

    /// Expected `Center`, `Top`, and `Bottom` viewport offsets for a row that
    /// spans `start..end` inside a `height`-row viewport (Conventions §5.2).
    fn recenter_beats(start: usize, end: usize, total: usize, height: u16) -> [u16; 3] {
        let height = height as usize;
        let max_scroll = total.saturating_sub(height);
        let clamp = |value: usize| u16::try_from(value.min(max_scroll)).unwrap_or(u16::MAX);
        [
            clamp(
                start
                    .saturating_add(end.saturating_sub(start) / 2)
                    .saturating_sub(height / 2),
            ),
            clamp(start),
            clamp(end.saturating_sub(height)),
        ]
    }

    /// Expected beats for the row at `index`, derived from the same layout
    /// offsets the view uses to position its viewport.
    fn plan_recenter_beats(view: &mut PlansViewState, index: usize) -> [u16; 3] {
        let rows = view.flatten_rows();
        let (lines, offsets) = compute_unified_plan_lines(
            &rows,
            index,
            view.content_width.max(10),
            view.theme_mode,
            &mut view.markdown_cache,
        );
        let start = offsets.get(index).copied().unwrap_or(0);
        let end = offsets.get(index + 1).copied().unwrap_or(lines.len());
        recenter_beats(start, end, lines.len(), view.view_height)
    }

    fn recenter_plans_fixture(selected: usize) -> PlansViewState {
        PlansViewState {
            plans: (1..=20)
                .map(|i| Plan {
                    id: format!("plan-{i}"),
                    title: format!("Plan {i}"),
                    ..Default::default()
                })
                .collect(),
            selected_index: selected,
            view_height: 8,
            content_width: 80,
            ..Default::default()
        }
    }

    #[test]
    fn test_recenter_selected_walks_the_center_top_bottom_cycle() {
        let mut view = recenter_plans_fixture(12);

        let index = view.selected_index;
        let [center, top, bottom] = plan_recenter_beats(&mut view, index);
        assert_ne!(center, top, "the center and top beats must differ");
        assert_ne!(top, bottom, "the top and bottom beats must differ");
        assert_ne!(center, bottom, "the center and bottom beats must differ");

        view.recenter_selected();
        assert_eq!(view.scroll, center, "the first press centers the row");
        view.recenter_selected();
        assert_eq!(
            view.scroll, top,
            "the second press aligns the row to the top"
        );
        view.recenter_selected();
        assert_eq!(
            view.scroll, bottom,
            "the third press aligns the row to the bottom"
        );
        view.recenter_selected();
        assert_eq!(
            view.scroll, center,
            "the fourth press wraps back to the center"
        );
    }

    #[test]
    fn test_recenter_selected_restarts_the_cycle_on_cursor_move() {
        let mut view = recenter_plans_fixture(12);

        // Walk to the top beat of the first row, so a continuation would land on
        // the bottom beat instead of starting over.
        view.recenter_selected();
        view.recenter_selected();

        view.selected_index = 5;
        let index = view.selected_index;
        let [center, top, bottom] = plan_recenter_beats(&mut view, index);
        assert_ne!(center, bottom, "the first and third beats must differ");

        view.recenter_selected();
        assert_eq!(
            view.scroll, center,
            "a new cursor position starts over on the center beat"
        );
        view.recenter_selected();
        assert_eq!(view.scroll, top, "the new row keeps its own cycle");
        view.recenter_selected();
        assert_eq!(view.scroll, bottom);
    }

    /// The recenter command is declared on both the abstract primary chord and
    /// the raw `Ctrl+L` chord, and no other binding claims the same key event
    /// under either modifier configuration (c-2).
    #[test]
    fn test_recenter_chord_is_declared_and_unambiguous() {
        use super::input::PlansAction;
        use crate::config::{InputConfig, MetaModifier, PrimaryModifier};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        // The abstract `C-l` chord and the raw `Ctrl+L` chord deliberately name
        // the same intent, so under the default primary assignment both of them
        // match a single key event. The resolved intents are deduplicated, since
        // the invariant under test is which action the key reaches, not how many
        // spellings declare it (c-2).
        fn claims(input: &InputConfig, code: KeyCode, modifiers: KeyModifiers) -> Vec<PlansAction> {
            let mut actions = Vec::new();
            for spec in PLANS_COMMAND_MAP.specs() {
                let matched = spec
                    .chord
                    .matches(KeyEvent::new(code, modifiers), input, false);
                if matched && !actions.contains(&spec.action) {
                    actions.push(spec.action);
                }
            }
            actions
        }

        let ctrl_primary = InputConfig {
            primary_modifier: PrimaryModifier::Ctrl,
            meta_modifier: MetaModifier::Alt,
            ..InputConfig::default()
        };
        let alt_primary = InputConfig {
            primary_modifier: PrimaryModifier::Alt,
            meta_modifier: MetaModifier::Ctrl,
            ..InputConfig::default()
        };

        assert_eq!(
            claims(&ctrl_primary, KeyCode::Char('l'), KeyModifiers::CONTROL),
            vec![PlansAction::Recenter],
            "C-l must recenter under the default primary modifier"
        );
        assert_eq!(
            claims(&alt_primary, KeyCode::Char('l'), KeyModifiers::ALT),
            vec![PlansAction::Recenter],
            "the abstract C-l chord must follow a remapped primary modifier"
        );
        assert_eq!(
            claims(&alt_primary, KeyCode::Char('l'), KeyModifiers::CONTROL),
            vec![PlansAction::Recenter],
            "a raw Ctrl+L must still recenter when the primary modifier is Alt"
        );
    }

    /// Line bounds of the row at `index` in the rendered plan tree, derived from
    /// the same layout offsets the view uses to position its viewport.
    fn row_bounds(view: &mut PlansViewState, index: usize) -> (usize, usize) {
        let rows = view.flatten_rows();
        let (lines, offsets) = compute_unified_plan_lines(
            &rows,
            index,
            view.content_width.max(10),
            view.theme_mode,
            &mut view.markdown_cache,
        );
        let start = offsets.get(index).copied().unwrap_or(0);
        let end = offsets.get(index + 1).copied().unwrap_or(lines.len());
        (start, end)
    }

    /// A plan holding twenty single-line steps, so every row fits the viewport.
    fn scroll_fixture(view_height: u16) -> PlansViewState {
        PlansViewState {
            plans: vec![Plan {
                id: "scroll-plan".to_string(),
                title: "Scroll Plan".to_string(),
                items: (1..=20)
                    .map(|i| leaf_item(&i.to_string(), &format!("Step {i}")))
                    .collect(),
                ..Default::default()
            }],
            view_height,
            content_width: 80,
            ..Default::default()
        }
    }

    #[test]
    fn test_scroll_to_selected_reveals_a_row_below_the_viewport() {
        let mut view = scroll_fixture(5);
        view.toggle_plan_expanded("scroll-plan");

        let selected = view
            .flatten_rows()
            .iter()
            .position(
                |row| matches!(row, VisiblePlanRow::PlanItem { item_id, .. } if item_id == "15"),
            )
            .expect("the step is rendered in the tree");
        view.selected_index = selected;
        view.scroll = 0;
        view.scroll_to_selected();

        let (start, end) = row_bounds(&mut view, selected);
        let height = view.view_height as usize;
        assert!(
            view.scroll as usize <= start,
            "the viewport must not start below the selected row"
        );
        assert!(
            view.scroll as usize + height >= end,
            "the selected row must be fully visible"
        );
        assert_eq!(
            view.scroll as usize,
            end.saturating_sub(height),
            "a row below the viewport scrolls its bottom edge into view"
        );

        // Walking back up to a row above the viewport aligns it to the top.
        let earlier = view
            .flatten_rows()
            .iter()
            .position(
                |row| matches!(row, VisiblePlanRow::PlanItem { item_id, .. } if item_id == "5"),
            )
            .expect("the earlier step is rendered in the tree");
        view.selected_index = earlier;
        view.scroll_to_selected();
        let (earlier_start, _) = row_bounds(&mut view, earlier);
        assert_eq!(
            view.scroll as usize, earlier_start,
            "a row above the viewport aligns to the top of the screen"
        );
    }

    /// A plan holding sixty single-line steps, so the rendered tree is much
    /// taller than the viewport and every navigation command can move the
    /// cursor off screen.
    fn tall_scroll_fixture(view_height: u16) -> PlansViewState {
        PlansViewState {
            plans: vec![Plan {
                id: "scroll-plan".to_string(),
                title: "Scroll Plan".to_string(),
                items: (1..=60)
                    .map(|i| leaf_item(&i.to_string(), &format!("Step {i}")))
                    .collect(),
                ..Default::default()
            }],
            view_height,
            content_width: 80,
            ..Default::default()
        }
    }

    /// Asserts that the row at `index` lies entirely inside the viewport.
    fn assert_selected_row_visible(view: &mut PlansViewState, index: usize) {
        let (start, end) = row_bounds(view, index);
        let scroll = view.scroll as usize;
        let height = view.view_height as usize;
        assert!(
            scroll <= start && scroll + height >= end,
            "the selected row {start}..{end} must be fully visible in {scroll}..{}",
            scroll + height
        );
    }

    /// Selecting the last row must scroll the viewport down to it, selecting a
    /// middle row must pull the viewport up to it, and selecting the first row
    /// must reveal the very top of the document (c-scroll-visibility).
    #[test]
    fn test_scroll_to_selected_reveals_rows_in_both_directions() {
        let mut view = tall_scroll_fixture(10);
        view.toggle_plan_expanded("scroll-plan");

        // Down: the last row must push the viewport away from the top, and the
        // cursor must rest on a visible row.
        let last = view.row_count() - 1;
        view.selected_index = last;
        view.scroll_to_selected();
        assert!(
            view.scroll > 0,
            "the last row must scroll the viewport down"
        );
        assert_selected_row_visible(&mut view, last);

        // Up: a row in the middle must be pulled into view instead of staying
        // above the viewport top.
        let middle = last / 2;
        view.selected_index = middle;
        view.scroll_to_selected();
        assert_selected_row_visible(&mut view, middle);

        // Home: the first row reveals the document top.
        view.selected_index = 0;
        view.scroll_to_selected();
        assert_eq!(view.scroll, 0, "the first row must reveal the document top");
        assert_selected_row_visible(&mut view, 0);
    }

    /// Entering a tall expanded step from below must reveal its bottom edge
    /// instead of pinning the viewport to its first screenful, so the lines the
    /// cursor lands near stay readable (c-scroll-visibility).
    #[test]
    fn test_scroll_to_selected_reveals_the_bottom_edge_of_a_tall_row_from_below() {
        let tall_details = (0..60)
            .map(|line| format!("Invariant line {line}."))
            .collect::<Vec<_>>()
            .join("\n");
        let mut view = tall_scroll_fixture(5);
        view.plans[0].items = vec![
            leaf_item("1", "First step"),
            PlanItem {
                id: "2".to_string(),
                title: "Tall step".to_string(),
                details: Some(tall_details),
                status: PlanItemStatus::Todo,
                ..Default::default()
            },
            leaf_item("3", "Last step"),
        ];
        view.toggle_plan_expanded("scroll-plan");
        view.toggle_item_expanded("scroll-plan", "2");

        let tall = view
            .flatten_rows()
            .iter()
            .position(
                |row| matches!(row, VisiblePlanRow::PlanItem { item_id, .. } if item_id == "2"),
            )
            .expect("the tall step is rendered in the tree");
        let after = view
            .flatten_rows()
            .iter()
            .position(
                |row| matches!(row, VisiblePlanRow::PlanItem { item_id, .. } if item_id == "3"),
            )
            .expect("the following step is rendered in the tree");

        let (start, end) = row_bounds(&mut view, tall);
        let height = view.view_height as usize;
        assert!(
            end - start > height,
            "the fixture row must be taller than the viewport"
        );

        // Rest the viewport on the row below the tall step, then move the cursor
        // up into the tall step.
        let after_start = row_bounds(&mut view, after).0;
        view.scroll = u16::try_from(after_start).expect("the viewport offset fits in u16");
        view.selected_index = tall;
        view.scroll_to_selected();
        assert_eq!(
            view.scroll as usize,
            end - height,
            "a tall row entered from below reveals its bottom edge"
        );
    }

    /// A row taller than the screen cannot be shown whole: entering it from above
    /// aligns it to the top, a viewport already deep inside it keeps its offset,
    /// and an offset past its bottom clamps to the last screenful
    /// (c-scroll-visibility).
    #[test]
    fn test_scroll_to_selected_keeps_a_tall_row_inside_its_bounds() {
        let tall_details = (0..60)
            .map(|line| format!("Invariant line {line}."))
            .collect::<Vec<_>>()
            .join("\n");
        let mut view = scroll_fixture(5);
        view.plans[0].items = vec![
            leaf_item("1", "First step"),
            PlanItem {
                id: "2".to_string(),
                title: "Tall step".to_string(),
                details: Some(tall_details),
                status: PlanItemStatus::Todo,
                ..Default::default()
            },
            leaf_item("3", "Last step"),
        ];
        view.toggle_plan_expanded("scroll-plan");
        view.toggle_item_expanded("scroll-plan", "2");

        let selected = view
            .flatten_rows()
            .iter()
            .position(
                |row| matches!(row, VisiblePlanRow::PlanItem { item_id, .. } if item_id == "2"),
            )
            .expect("the tall step is rendered in the tree");
        let (start, end) = row_bounds(&mut view, selected);
        let height = view.view_height as usize;
        assert!(
            end - start > height,
            "the fixture row must be taller than the viewport"
        );

        view.selected_index = selected;
        view.scroll = 0;
        view.scroll_to_selected();
        assert_eq!(view.scroll as usize, start, "the row aligns to the top");

        view.scroll = (start + 20) as u16;
        view.scroll_to_selected();
        assert_eq!(
            view.scroll as usize,
            start + 20,
            "a viewport inside the row keeps its offset instead of snapping back"
        );

        view.scroll = end as u16;
        view.scroll_to_selected();
        assert_eq!(
            view.scroll as usize,
            end - height,
            "an offset past the row bottom clamps to the last screenful"
        );
    }
}
