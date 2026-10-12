use std::collections::HashSet;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};
use tauqe_protocol::{ConceptItem, ModelSelection, Plan, PlanItem, PlanItemStatus};

use crate::app::AppState;
use crate::ui::wrap_lines;

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
        concept_refs: Vec<String>,
        depth: usize,
        has_children: bool,
        has_details: bool,
        is_expandable: bool,
        is_expanded: bool,
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

#[derive(Debug, Clone)]
pub struct PlanRefineDialogState {
    pub plan_id: String,
    pub plan_title: String,
    pub models: Vec<(String, Option<ModelSelection>)>,
    pub selected_model_index: usize,
    pub instructions_editor: crate::editor::InputEditor,
    pub confirm_button: crate::app::ConfirmDialogButton,
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
        let has_meta = !item.files.is_empty() || !item.concept_refs.is_empty();
        let is_expandable = has_children || has_details || has_meta;
        let is_expanded = self.is_item_expanded(plan_id, &item.id);

        out.push(VisiblePlanRow::PlanItem {
            plan_id: plan_id.to_string(),
            item_id: item.id.clone(),
            title: item.title.clone(),
            details: item.details.clone(),
            status: item.status,
            files: item.files.clone(),
            concept_refs: item.concept_refs.clone(),
            depth,
            has_children,
            has_details,
            is_expandable,
            is_expanded,
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

        let (lines, offsets) = compute_unified_plan_lines(&rows, sel, self.content_width.max(10));
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

    /// Accordion-style navigation: moves to the next or previous item in the plan hierarchy,
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

        fn collect_concept_nodes(
            plan_id: &str,
            item: &ConceptItem,
            ancestors: &mut Vec<String>,
            out: &mut Vec<PlanNodeRef>,
        ) {
            let has_children = !item.children.is_empty();
            let has_details = item.details.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
            let is_expandable = has_children || has_details;

            out.push(PlanNodeRef {
                plan_id: plan_id.to_string(),
                item_id: Some(item.id.clone()),
                is_concept: true,
                ancestor_item_ids: ancestors.clone(),
                is_expandable,
            });

            ancestors.push(item.id.clone());
            for child in &item.children {
                collect_concept_nodes(plan_id, child, ancestors, out);
            }
            ancestors.pop();
        }

        fn collect_plan_nodes(
            plan_id: &str,
            item: &PlanItem,
            ancestors: &mut Vec<String>,
            out: &mut Vec<PlanNodeRef>,
        ) {
            let has_children = !item.children.is_empty();
            let has_details = item.details.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
            let has_meta = !item.files.is_empty() || !item.concept_refs.is_empty();
            let is_expandable = has_children || has_details || has_meta;

            out.push(PlanNodeRef {
                plan_id: plan_id.to_string(),
                item_id: Some(item.id.clone()),
                is_concept: false,
                ancestor_item_ids: ancestors.clone(),
                is_expandable,
            });

            ancestors.push(item.id.clone());
            for child in &item.children {
                collect_plan_nodes(plan_id, child, ancestors, out);
            }
            ancestors.pop();
        }

        let mut all_nodes = Vec::new();
        for plan in &self.plans {
            all_nodes.push(PlanNodeRef {
                plan_id: plan.id.clone(),
                item_id: None,
                is_concept: false,
                ancestor_item_ids: Vec::new(),
                is_expandable: !plan.items.is_empty()
                    || !plan.concept.is_empty()
                    || plan.description.as_deref().map(|d| !d.trim().is_empty()).unwrap_or(false),
            });

            let mut concept_ancestors = Vec::new();
            for item in &plan.concept {
                collect_concept_nodes(&plan.id, item, &mut concept_ancestors, &mut all_nodes);
            }

            let mut plan_ancestors = Vec::new();
            for item in &plan.items {
                collect_plan_nodes(&plan.id, item, &mut plan_ancestors, &mut all_nodes);
            }
        }

        if all_nodes.is_empty() {
            return;
        }

        let sel_row = self.selected_row();
        let cur_idx = match sel_row {
            Some(VisiblePlanRow::PlanHeader { plan_id, .. }) => {
                all_nodes.iter().position(|n| n.plan_id == plan_id && n.item_id.is_none()).unwrap_or(0)
            }
            Some(VisiblePlanRow::ConceptItem { plan_id, item_id, .. }) => {
                all_nodes.iter().position(|n| n.plan_id == plan_id && n.is_concept && n.item_id.as_deref() == Some(&item_id)).unwrap_or(0)
            }
            Some(VisiblePlanRow::PlanItem { plan_id, item_id, .. }) => {
                all_nodes.iter().position(|n| n.plan_id == plan_id && !n.is_concept && n.item_id.as_deref() == Some(&item_id)).unwrap_or(0)
            }
            _ => 0,
        };

        let target_idx = if forward {
            if cur_idx + 1 < all_nodes.len() {
                cur_idx + 1
            } else {
                return;
            }
        } else if cur_idx > 0 {
            cur_idx - 1
        } else {
            return;
        };

        let target = all_nodes[target_idx].clone();

        self.expanded_plans.clear();
        if target.item_id.is_some() {
            self.expanded_plans.insert(target.plan_id.clone());
        }

        self.expanded_items.clear();
        self.collapsed_sections.clear();

        if let Some(ref tid) = target.item_id {
            if target.is_concept {
                for anc in &target.ancestor_item_ids {
                    self.expanded_items.insert(format!("c:{}:{}", target.plan_id, anc));
                }
                if target.is_expandable {
                    self.expanded_items.insert(format!("c:{}:{}", target.plan_id, tid));
                }
            } else {
                for anc in &target.ancestor_item_ids {
                    self.expanded_items.insert(format!("{}:{}", target.plan_id, anc));
                }
                if target.is_expandable {
                    self.expanded_items.insert(format!("{}:{}", target.plan_id, tid));
                }
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
    match status {
        PlanItemStatus::Discussion => Style::default().bg(Color::Magenta).fg(Color::Black).bold(),
        PlanItemStatus::Todo => Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
        PlanItemStatus::InProgress => Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        PlanItemStatus::Done => Style::default().bg(Color::Green).fg(Color::Black).bold(),
        PlanItemStatus::Cancelled => Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
    }
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
        let mut out = String::with_capacity(trimmed.len());
        out.push_str(lines[0]);
        for line in &lines[1..] {
            out.push('\n');
            if line.len() >= min_tail_indent {
                out.push_str(&line[min_tail_indent..]);
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
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut all_lines = Vec::new();
    let mut offsets = Vec::with_capacity(rows.len());

    for (idx, row) in rows.iter().enumerate() {
        let is_selected = idx == selected_idx;
        offsets.push(all_lines.len());

        let row_style = if is_selected {
            Style::default().bg(Color::DarkGray)
        } else {
            Style::default()
        };

        let gutter = if is_selected {
            Span::styled("▌", Style::default().fg(Color::Cyan).bold())
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
            } => {
                if idx > 0 {
                    all_lines.push(Line::raw(""));
                }

                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let stats_str = format!("{}/{} done", completed, total);

                let mut spans = vec![
                    gutter,
                    Span::raw(" "),
                    Span::styled(fold_icon, Style::default().fg(Color::Yellow).bold()),
                    Span::styled(format!(" {} ", status), status_style(*status)),
                    Span::raw(" "),
                    Span::styled(format!("[{}] ", plan_id), Style::default().bold().fg(Color::Cyan)),
                    Span::styled(title.clone(), Style::default().bold().fg(Color::White)),
                    Span::styled(format!(" ({})", stats_str), Style::default().fg(if is_selected { Color::White } else { Color::DarkGray })),
                ];
                if is_selected {
                    spans.push(Span::styled("  [Tab: fold, x: exec, d: discuss, Del: delete]", Style::default().fg(Color::Yellow).bold()));
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
                            let mut theme = crate::markdown::MarkdownTheme::answer();
                            theme.text_style = Style::default().fg(Color::Gray);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix = Some(Span::styled("    │ ", Style::default().fg(Color::DarkGray)));
                            let md = crate::markdown::render_markdown(&prepared, &theme);
                            if !md.is_empty() {
                                desc_lines.extend(md);
                            } else {
                                for line in prepared.lines() {
                                    desc_lines.push(Line::from(vec![
                                        Span::styled("    │ ", Style::default().fg(Color::DarkGray)),
                                        Span::styled(line.to_string(), Style::default().fg(Color::Gray)),
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
                let mut spans = vec![
                    gutter,
                    Span::raw("  "),
                    Span::styled(fold_icon, Style::default().fg(Color::Magenta).bold()),
                    Span::styled(" CONCEPT ", Style::default().bg(Color::Magenta).fg(Color::Black).bold()),
                    Span::raw(" "),
                    Span::styled("Architectural Intent & Invariants", Style::default().bold().fg(Color::White)),
                    Span::styled(format!(" ({} item{})", count, if *count == 1 { "" } else { "s" }), Style::default().fg(if is_selected { Color::White } else { Color::DarkGray })),
                ];
                if is_selected {
                    spans.push(Span::styled("  [Tab: fold, d: discuss]", Style::default().fg(Color::Yellow).bold()));
                }
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
                    Style::default().fg(Color::Magenta).bold()
                } else if is_selected {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let title_style = if is_selected {
                    Style::default().fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::LightMagenta)
                };
                let clean_title = title.replace(['\r', '\n'], " ").trim().to_string();

                let mut spans = vec![
                    gutter,
                    Span::raw(indent.clone()),
                    Span::styled(fold_icon, fold_style),
                    Span::styled(format!("#{} {}", item_id, clean_title), title_style),
                ];
                if is_selected {
                    spans.push(Span::styled("  [Tab: fold, d: discuss]", Style::default().fg(Color::Yellow).bold()));
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
                            let mut theme = crate::markdown::MarkdownTheme::answer();
                            theme.text_style = Style::default().fg(Color::Gray);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix = Some(Span::styled(
                                format!("{}    │ ", indent),
                                Style::default().fg(Color::DarkGray),
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
                let mut spans = vec![
                    gutter,
                    Span::raw("  "),
                    Span::styled(fold_icon, Style::default().fg(Color::Cyan).bold()),
                    Span::styled(" IMPLEMENTATION ", Style::default().bg(Color::Cyan).fg(Color::Black).bold()),
                    Span::raw(" "),
                    Span::styled("Execution Blueprint", Style::default().bold().fg(Color::White)),
                    Span::styled(format!(" ({})", stats_str), Style::default().fg(if is_selected { Color::White } else { Color::DarkGray })),
                ];
                if is_selected {
                    spans.push(Span::styled("  [Tab: fold, x: exec queue]", Style::default().fg(Color::Yellow).bold()));
                }
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
                concept_refs,
                depth,
                has_children,
                has_details: _,
                is_expandable,
                is_expanded,
                ..
            } => {
                let indent = "  ".repeat(*depth);

                let fold_icon = if *is_expandable {
                    if *is_expanded { "▼ " } else { "▶ " }
                } else if is_selected {
                    "▶ "
                } else if *status == PlanItemStatus::Discussion {
                    "? "
                } else {
                    "• "
                };

                let fold_style = if *is_expandable {
                    Style::default().fg(Color::Yellow).bold()
                } else if is_selected {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                };

                let title_style = if is_selected {
                    Style::default().fg(Color::White).bold()
                } else if *status == PlanItemStatus::Done || *status == PlanItemStatus::Cancelled {
                    Style::default().fg(Color::Gray)
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

                if !files.is_empty() {
                    spans.push(Span::styled(
                        format!(" [{} file{}]", files.len(), if files.len() == 1 { "" } else { "s" }),
                        Style::default().fg(Color::DarkGray),
                    ));
                }

                if is_selected {
                    let hint = if *has_children {
                        "  [Tab: fold, x: exec, d: discuss, t: status]"
                    } else if *status == PlanItemStatus::Discussion {
                        "  [d: discuss, t: status (approve)]"
                    } else if *status == PlanItemStatus::Cancelled {
                        "  [t: status]"
                    } else {
                        "  [x: exec, d: discuss, t: status]"
                    };
                    spans.push(Span::styled(hint, Style::default().fg(Color::Yellow).bold()));
                }

                let mut row_line = Line::from(spans).style(row_style);
                if is_selected {
                    row_line = pad_line_to_width(row_line, width, row_style);
                }
                let mut row_lines = vec![row_line];

                if *is_expanded {
                    if !concept_refs.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(format!("{}    │ ", indent), Style::default().fg(Color::DarkGray)),
                            Span::styled("ref: ", Style::default().fg(Color::DarkGray)),
                            Span::styled(concept_refs.join(", "), Style::default().fg(Color::Magenta)),
                        ]));
                    }
                    if !files.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(format!("{}    │ ", indent), Style::default().fg(Color::DarkGray)),
                            Span::styled("files: ", Style::default().fg(Color::DarkGray)),
                            Span::styled(files.join(", "), Style::default().fg(Color::Cyan)),
                        ]));
                    }
                    if let Some(det) = details {
                        let prepared = prepare_plan_markdown(det);
                        if !prepared.is_empty() {
                            let mut theme = crate::markdown::MarkdownTheme::answer();
                            theme.text_style = Style::default().fg(Color::Gray);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix = Some(Span::styled(
                                format!("{}    │ ", indent),
                                Style::default().fg(Color::DarkGray),
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

pub fn render_plans_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    main_area: Rect,
    info_area: Rect,
) {
    let block = Block::default().padding(Padding::horizontal(1));
    let inner = block.inner(main_area);
    let height = inner.height;
    let width = (inner.width as usize).max(10);
    state.plans_view.view_height = height;
    state.plans_view.content_width = width;

    let rows = state.plans_view.flatten_rows();
    state.plans_view.clamp_selection();

    let (lines, offsets) = if rows.is_empty() {
        (
            vec![
                Line::raw(""),
                Line::from(Span::styled(
                    "  No local engineering plans yet.",
                    Style::default().fg(Color::DarkGray).bold(),
                )),
                Line::from(Span::styled(
                    "  Ask Tauqe AI in Develop (Ctrl+1) to formulate a multi-step task plan, or press 'r' to refresh.",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            Vec::new(),
        )
    } else {
        compute_unified_plan_lines(&rows, state.plans_view.selected_index, width)
    };

    state.plans_view.rendered_lines = lines.len();

    if !rows.is_empty() && height > 0 {
        let sel = state.plans_view.selected_index.min(rows.len() - 1);
        let start = offsets.get(sel).copied().unwrap_or(0);
        let end = offsets.get(sel + 1).copied().unwrap_or(lines.len());
        let scroll = state.plans_view.scroll as usize;

        let new_scroll = if sel == 0 {
            0
        } else if start < scroll {
            start
        } else if end > scroll + height as usize {
            end.saturating_sub(height as usize).min(start)
        } else {
            scroll
        };
        state.plans_view.scroll = new_scroll as u16;
    }

    let max_scroll = (lines.len() as u16).saturating_sub(height);
    if state.plans_view.scroll > max_scroll {
        state.plans_view.scroll = max_scroll;
    }

    let tree_widget = Paragraph::new(lines)
        .block(block)
        .scroll((state.plans_view.scroll, 0));
    frame.render_widget(tree_widget, main_area);

    let mut status_spans = Vec::new();
    if let Some(msg) = &state.plans_view.status_message {
        status_spans.push(Span::styled(msg.clone(), Style::default().fg(Color::Gray)));
    } else {
        let plans_count = state.plans_view.plans.len();
        let total_items: usize = state.plans_view.plans.iter().map(|p| p.count_stats().0).sum();
        let total_completed: usize = state.plans_view.plans.iter().map(|p| p.count_stats().1).sum();
        status_spans.push(Span::styled(
            format!(
                " {} plan(s), {} item(s) total ({} completed)",
                plans_count, total_items, total_completed
            ),
            Style::default().fg(Color::Gray),
        ));
        if !rows.is_empty() {
            status_spans.push(Span::styled(
                format!(" | [Row {}/{}]", state.plans_view.selected_index + 1, rows.len()),
                Style::default().fg(Color::Gray),
            ));
        }
        if state.plans_view.refining {
            status_spans.push(Span::styled(
                " | [REFINING: AI Architect is analyzing codebase...]",
                Style::default().fg(Color::Yellow).bold(),
            ));
        }
    }
    let status_line = Line::from(status_spans);

    let info_widget = Paragraph::new(vec![status_line])
        .block(Block::default().padding(Padding::horizontal(1)));
    frame.render_widget(info_widget, info_area);
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
                concept_refs: vec![],
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
                concept_refs: vec!["c-1".to_string()],
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
}
