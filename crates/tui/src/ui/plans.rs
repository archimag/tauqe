use std::collections::HashSet;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};
use tauqe_protocol::{Plan, PlanItem, PlanItemStatus};

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
    PlanItem {
        plan_id: String,
        item_id: String,
        title: String,
        details: Option<String>,
        status: PlanItemStatus,
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
            Self::PlanItem { plan_id, .. } => plan_id,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PlansViewState {
    pub plans: Vec<Plan>,
    pub selected_index: usize,
    pub scroll: u16,
    pub expanded_plans: HashSet<String>,
    pub expanded_items: HashSet<String>,
    pub view_height: u16,
    pub content_width: usize,
    pub rendered_lines: usize,
    pub status_message: Option<String>,
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
        !self.plans.is_empty() && self.plans.iter().all(|p| self.expanded_plans.contains(&p.id))
    }

    pub fn expand_all(&mut self) {
        for plan in &self.plans {
            self.expanded_plans.insert(plan.id.clone());
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
                for item in &plan.items {
                    self.flatten_item_recursive(&plan.id, item, 1, &mut rows);
                }
            }
        }
        rows
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
        let is_expandable = has_children || has_details;
        let is_expanded = self.is_item_expanded(plan_id, &item.id);

        out.push(VisiblePlanRow::PlanItem {
            plan_id: plan_id.to_string(),
            item_id: item.id.clone(),
            title: item.title.clone(),
            details: item.details.clone(),
            status: item.status,
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
            ancestor_item_ids: Vec<String>,
            is_expandable: bool,
        }

        fn collect_plan_nodes(
            plan_id: &str,
            item: &PlanItem,
            ancestors: &mut Vec<String>,
            out: &mut Vec<PlanNodeRef>,
        ) {
            let has_children = !item.children.is_empty();
            let has_details = item.details.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
            let is_expandable = has_children || has_details;

            out.push(PlanNodeRef {
                plan_id: plan_id.to_string(),
                item_id: Some(item.id.clone()),
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
                ancestor_item_ids: Vec::new(),
                is_expandable: !plan.items.is_empty() || plan.description.as_deref().map(|d| !d.trim().is_empty()).unwrap_or(false),
            });
            let mut ancestors = Vec::new();
            for item in &plan.items {
                collect_plan_nodes(&plan.id, item, &mut ancestors, &mut all_nodes);
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
            Some(VisiblePlanRow::PlanItem { plan_id, item_id, .. }) => {
                all_nodes.iter().position(|n| n.plan_id == plan_id && n.item_id.as_deref() == Some(&item_id)).unwrap_or(0)
            }
            None => 0,
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

        // Update expanded_plans: target plan must be open if an item is targeted; other plans collapsed
        self.expanded_plans.clear();
        if target.item_id.is_some() {
            self.expanded_plans.insert(target.plan_id.clone());
        }

        // Clean expanded_items to keep only target and its ancestors
        self.expanded_items.clear();
        if let Some(ref tid) = target.item_id {
            for anc in &target.ancestor_item_ids {
                self.expanded_items.insert(format!("{}:{}", target.plan_id, anc));
            }
            if target.is_expandable {
                self.expanded_items.insert(format!("{}:{}", target.plan_id, tid));
            }
        }

        let rows = self.flatten_rows();
        if let Some(pos) = rows.iter().position(|r| match (r, &target.item_id) {
            (VisiblePlanRow::PlanHeader { plan_id, .. }, None) => plan_id == &target.plan_id,
            (VisiblePlanRow::PlanItem { plan_id, item_id, .. }, Some(tid)) => {
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
            VisiblePlanRow::PlanItem {
                item_id,
                title,
                details,
                status,
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
        if let Some(ref queue) = state.plan_batch_queue {
            let cur = queue.current_index + 1;
            let total = queue.steps.len();
            let step_id = queue.steps.get(queue.current_index).map(|s| s.id.as_str()).unwrap_or("?");
            status_spans.push(Span::styled(
                format!(" | [BATCH: Step {}/{} (#{})]", cur, total, step_id),
                Style::default().fg(Color::Yellow).bold(),
            ));
        }
    }
    let status_line = Line::from(status_spans);

    let info_widget = Paragraph::new(vec![status_line])
        .block(Block::default().padding(Padding::horizontal(1)));
    frame.render_widget(info_widget, info_area);

    if let Some(plan_id) = &state.confirm_delete_plan {
        crate::ui::dialogs::render_confirm_delete_plan_popup(frame, plan_id, state);
    }
    if state.confirm_execute_scope.is_some() {
        crate::ui::dialogs::render_confirm_execute_scope_popup(frame, state);
    }
    if let Some(discuss) = &state.discuss_plan_dialog {
        crate::ui::dialogs::render_discuss_plan_dialog(frame, discuss);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::PlanItem;

    #[test]
    fn test_plans_collapsed_by_default() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "test-plan".to_string(),
            title: "Test Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
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
}
