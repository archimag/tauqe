use std::collections::HashSet;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};
use tauqe_protocol::{Plan, PlanItem, PlanItemStatus, PlanSummary};

use crate::app::AppState;
use crate::ui::wrap_lines;

#[derive(Debug, Clone)]
pub struct VisiblePlanRow {
    pub item_id: String,
    pub title: String,
    pub details: Option<String>,
    pub status: PlanItemStatus,
    pub checked: bool,
    pub depth: usize,
    pub has_children: bool,
    pub has_details: bool,
    pub is_expandable: bool,
    pub is_expanded: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PlansViewState {
    pub plans_list: Vec<PlanSummary>,
    pub current_plan: Option<Plan>,
    pub active_plan_id: Option<String>,
    pub initialized_plan_id: Option<String>,
    pub selected_plan_index: usize,
    pub selected_item_index: usize,
    pub scroll: u16,
    pub expanded_items: HashSet<String>,
    pub view_height: u16,
    pub content_width: usize,
    pub rendered_lines: usize,
    pub status_message: Option<String>,
}

impl PlansViewState {
    pub fn expand_all(&mut self) {
        if let Some(plan) = &self.current_plan {
            let mut ids = Vec::new();
            fn collect_ids(item: &PlanItem, ids: &mut Vec<String>) {
                ids.push(item.id.clone());
                for child in &item.children {
                    collect_ids(child, ids);
                }
            }
            for item in &plan.items {
                collect_ids(item, &mut ids);
            }
            self.expanded_items.extend(ids);
        }
    }

    pub fn collapse_all(&mut self) {
        self.expanded_items.clear();
    }

    pub fn reset_view_for_new_plan(&mut self) {
        self.selected_item_index = 0;
        self.scroll = 0;
        self.expanded_items.clear();
    }

    pub fn toggle_expanded(&mut self, item_id: &str) {
        if !self.expanded_items.remove(item_id) {
            self.expanded_items.insert(item_id.to_string());
        }
    }

    pub fn flatten_items(&self) -> Vec<VisiblePlanRow> {
        let Some(plan) = &self.current_plan else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for item in &plan.items {
            self.flatten_recursive(item, 0, &mut rows);
        }
        rows
    }

    fn flatten_recursive(
        &self,
        item: &PlanItem,
        depth: usize,
        out: &mut Vec<VisiblePlanRow>,
    ) {
        let has_children = !item.children.is_empty();
        let has_details = item.details.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
        let is_expandable = has_children || has_details;
        let is_expanded = self.expanded_items.contains(&item.id);

        out.push(VisiblePlanRow {
            item_id: item.id.clone(),
            title: item.title.clone(),
            details: item.details.clone(),
            status: item.status,
            checked: item.checked,
            depth,
            has_children,
            has_details,
            is_expandable,
            is_expanded,
        });

        if has_children && is_expanded {
            for child in &item.children {
                self.flatten_recursive(child, depth + 1, out);
            }
        }
    }

    pub fn selected_row(&self) -> Option<VisiblePlanRow> {
        let rows = self.flatten_items();
        rows.get(self.selected_item_index).cloned()
    }

    pub fn clamp_selection(&mut self) {
        let count = self.flatten_items().len();
        if count == 0 {
            self.selected_item_index = 0;
        } else if self.selected_item_index >= count {
            self.selected_item_index = count - 1;
        }
    }

    pub fn scroll_to_selected(&mut self) {
        let rows = self.flatten_items();
        if rows.is_empty() || self.view_height == 0 {
            return;
        }
        let sel = self.selected_item_index.min(rows.len() - 1);
        self.selected_item_index = sel;

        let (lines, offsets) = compute_plan_lines(
            self.current_plan.as_ref(),
            &self.plans_list,
            self.active_plan_id.as_deref(),
            self.selected_plan_index,
            &rows,
            sel,
            self.content_width.max(10),
        );
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
}

fn status_style(status: PlanItemStatus) -> Style {
    match status {
        PlanItemStatus::Todo => Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
        PlanItemStatus::InProgress => Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        PlanItemStatus::Done => Style::default().bg(Color::Green).fg(Color::Black).bold(),
        PlanItemStatus::Cancelled => Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
    }
}

fn compute_plan_lines(
    plan: Option<&Plan>,
    plans_list: &[PlanSummary],
    active_plan_id: Option<&str>,
    selected_plan_idx: usize,
    rows: &[VisiblePlanRow],
    selected_idx: usize,
    width: usize,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut all_lines = Vec::new();
    let mut offsets = Vec::with_capacity(rows.len());

    if plans_list.len() > 1 {
        let mut switcher_spans = vec![
            Span::styled(" Plans: ", Style::default().bold().fg(Color::Cyan)),
        ];
        for (idx, summary) in plans_list.iter().enumerate() {
            let is_sel = idx == selected_plan_idx;
            let is_act = active_plan_id == Some(&summary.id);
            let style = if is_sel {
                Style::default().bg(Color::Cyan).fg(Color::Black).bold()
            } else if is_act {
                Style::default().bg(Color::DarkGray).fg(Color::White).bold()
            } else {
                Style::default().fg(Color::Gray)
            };
            let star = if is_act { "★ " } else { "" };
            switcher_spans.push(Span::styled(
                format!(" [{}{}] ", star, summary.id),
                style,
            ));
            switcher_spans.push(Span::raw(" "));
        }
        all_lines.push(Line::from(switcher_spans));
        all_lines.push(Line::raw(""));
    }

    if let Some(plan) = plan {
        let mut header_spans = vec![
            Span::styled(format!(" Plan: {} ", plan.title), Style::default().bold().fg(Color::Cyan)),
        ];
        if let Some(desc) = &plan.description {
            if !desc.trim().is_empty() {
                header_spans.push(Span::raw(" — "));
                header_spans.push(Span::styled(desc.clone(), Style::default().fg(Color::Gray).italic()));
            }
        }
        all_lines.extend(wrap_lines(vec![Line::from(header_spans), Line::raw("")], width));
    }

    for (idx, row) in rows.iter().enumerate() {
        let is_selected = idx == selected_idx;
        let is_expanded = row.is_expanded;

        let cursor_style = if is_selected {
            Style::default().fg(Color::Cyan).bold()
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let fold_icon = if row.is_expandable {
            if is_expanded { "▼ " } else { "▶ " }
        } else {
            "• "
        };
        let fold_style = if row.is_expandable {
            Style::default().fg(Color::Yellow).bold()
        } else {
            Style::default().fg(Color::Gray)
        };

        let check_badge = Span::styled(
            if row.checked { "[x] " } else { "[ ] " },
            Style::default().fg(Color::Green).bold(),
        );

        let title_style = if row.status == PlanItemStatus::Done || row.status == PlanItemStatus::Cancelled {
            Style::default().fg(Color::Gray)
        } else {
            Style::default().bold()
        };

        let indent = "  ".repeat(row.depth);

        let clean_title = row.title.replace(['\r', '\n'], " ").trim().to_string();
        let spans = vec![
            Span::styled(if is_selected { "● " } else { "  " }, cursor_style),
            Span::raw(indent.clone()),
            Span::styled(fold_icon, fold_style),
            check_badge,
            Span::styled(format!(" {} ", row.status), status_style(row.status)),
            Span::raw(" "),
            Span::styled(format!("#{} {}", row.item_id, clean_title), title_style),
        ];

        let mut row_lines = vec![Line::from(spans)];

        if is_expanded {
            if let Some(details) = &row.details {
                let trimmed = details.trim();
                if !trimmed.is_empty() {
                    let mut theme = crate::markdown::MarkdownTheme::answer();
                    theme.text_style = Style::default();
                    theme.line_prefix = Some(Span::raw(format!("{}    ", indent)));
                    let md = crate::markdown::render_markdown(trimmed, &theme);
                    if !md.is_empty() {
                        row_lines.extend(md);
                        row_lines.push(Line::raw(""));
                    }
                }
            }
        }

        offsets.push(all_lines.len());
        all_lines.extend(wrap_lines(row_lines, width));
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

    let rows = state.plans_view.flatten_items();
    state.plans_view.clamp_selection();

    let (lines, offsets) = if state.plans_view.current_plan.is_none() && state.plans_view.plans_list.is_empty() {
        (
            vec![
                Line::raw(""),
                Line::from(Span::styled(
                    "  No local plans yet.",
                    Style::default().fg(Color::DarkGray).bold(),
                )),
                Line::from(Span::styled(
                    "  Ask Tauqe in Develop (Ctrl+1) to formulate a plan, or press 'r' to refresh.",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            Vec::new(),
        )
    } else {
        compute_plan_lines(
            state.plans_view.current_plan.as_ref(),
            &state.plans_view.plans_list,
            state.plans_view.active_plan_id.as_deref(),
            state.plans_view.selected_plan_index,
            &rows,
            state.plans_view.selected_item_index,
            width,
        )
    };

    state.plans_view.rendered_lines = lines.len();

    if !rows.is_empty() && height > 0 {
        let sel = state.plans_view.selected_item_index.min(rows.len() - 1);
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
    } else if let Some(plan) = &state.plans_view.current_plan {
        let stats = plan.stats();
        status_spans.push(Span::styled(
            format!(
                " {} item(s), {} completed, {} checked for Develop",
                stats.total, stats.completed, stats.checked
            ),
            Style::default().fg(Color::Gray),
        ));
        if !rows.is_empty() {
            status_spans.push(Span::styled(
                format!(" | [Item {}/{}]", state.plans_view.selected_item_index + 1, rows.len()),
                Style::default().fg(Color::Gray),
            ));
        }
    }
    let status_line = Line::from(status_spans);

    let info_widget = Paragraph::new(vec![status_line])
        .block(Block::default().padding(Padding::horizontal(1)));
    frame.render_widget(info_widget, info_area);
}
