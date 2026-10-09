use std::collections::{HashMap, HashSet};

use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};
use tauqe_protocol::{UiHistoryItem, UiHistoryKind};

use crate::app::AppState;
use crate::ui::wrap_lines;

const COLLAPSED_PREVIEW_LINES: usize = 3;

/// Saturating conversion for scroll arithmetic; avoids wrap-around on very long histories.
fn clamp_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

#[derive(Debug, Clone)]
pub struct HistoryCodeBlock {
    pub item_id: u64,
    pub block_id: usize,
    pub lang: String,
    pub code: String,
    pub visual_start_line: usize,
    pub visual_end_line: usize,
}

/// Code block position relative to the start of a cached item body.
#[derive(Debug, Clone)]
struct CachedCodeBlock {
    block_id: usize,
    lang: String,
    code: String,
    start: usize,
    end: usize,
}

/// Wrapped, highlighted body lines of one history item (everything below its header).
#[derive(Debug, Clone)]
struct CachedBody {
    text_len: usize,
    files_len: usize,
    lines: Vec<Line<'static>>,
    blocks: Vec<CachedCodeBlock>,
}

/// Caches expensive per-item rendering (markdown, syntax highlighting, wrapping),
/// keyed by `(item id, expanded)` and invalidated whenever the wrap width changes.
#[derive(Debug, Clone, Default)]
pub(crate) struct HistoryBodyCache {
    width: Option<usize>,
    entries: HashMap<(u64, bool), CachedBody>,
}

impl HistoryBodyCache {
    fn prepare(&mut self, max_width: Option<usize>, items: &[UiHistoryItem]) {
        if self.width != max_width {
            self.entries.clear();
            self.width = max_width;
        }
        if self.entries.len() > items.len() * 2 {
            let live: HashSet<u64> = items.iter().map(|it| it.id).collect();
            self.entries.retain(|(id, _), _| live.contains(id));
        }
    }

    fn body_for(
        &mut self,
        item: &UiHistoryItem,
        is_expanded: bool,
        max_width: Option<usize>,
    ) -> &CachedBody {
        let key = (item.id, is_expanded);
        let stale = self
            .entries
            .get(&key)
            .is_some_and(|b| b.text_len != item.text.len() || b.files_len != item.files.len());
        if stale {
            self.entries.remove(&key);
        }
        self.entries
            .entry(key)
            .or_insert_with(|| build_item_body(item, is_expanded, max_width, None))
    }
}

#[derive(Debug, Clone)]
pub struct HistoryViewState {
    pub items: Vec<UiHistoryItem>,
    pub scroll: u16,
    pub auto_scroll: bool,
    pub has_more: bool,
    pub total_count: usize,
    pub estimated_tokens: u64,
    pub loading: bool,
    pub rendered_lines_count: usize,
    pub pending_before_id: Option<u64>,
    pub select_prev_after_load: bool,
    pub recenter_cycle: u8,
    pub last_recenter_index: Option<usize>,
    pub selected_item_index: usize,
    pub content_width: usize,
    pub expanded_items: HashSet<u64>,
    pub code_blocks: Vec<HistoryCodeBlock>,
    pub content_rect: (u16, u16, u16, u16),
    pub copy_flash: Option<((u64, usize), std::time::Instant)>,
    pub(crate) body_cache: HistoryBodyCache,
}

impl Default for HistoryViewState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            scroll: 0,
            auto_scroll: true,
            has_more: false,
            total_count: 0,
            estimated_tokens: 0,
            loading: false,
            rendered_lines_count: 0,
            pending_before_id: None,
            select_prev_after_load: false,
            recenter_cycle: 0,
            last_recenter_index: None,
            selected_item_index: 0,
            content_width: 80,
            expanded_items: HashSet::new(),
            code_blocks: Vec::new(),
            content_rect: (0, 0, 0, 0),
            copy_flash: None,
            body_cache: HistoryBodyCache::default(),
        }
    }
}

impl HistoryViewState {
    pub fn is_expanded(&self, id: u64) -> bool {
        self.expanded_items.contains(&id)
    }

    pub fn toggle_expanded(&mut self, id: u64) {
        if self.expanded_items.contains(&id) {
            self.expanded_items.remove(&id);
        } else {
            self.expanded_items.insert(id);
        }
    }

    pub fn get_estimated_tokens(&self) -> u64 {
        if self.estimated_tokens > 0 {
            self.estimated_tokens
        } else {
            let total_chars: usize = self.items.iter().map(|it| it.text.chars().count()).sum();
            total_chars.div_ceil(4) as u64
        }
    }

    /// Returns the total visual line count and per-item start offsets, reusing cached bodies.
    fn layout_offsets(
        &mut self,
        max_width: Option<usize>,
        selected: Option<usize>,
    ) -> (usize, Vec<usize>) {
        let (lines, offsets, _) = layout_history(
            &self.items,
            max_width,
            selected,
            Some(&self.expanded_items),
            None,
            &mut self.body_cache,
        );
        (lines.len(), offsets)
    }

    pub fn scroll_to_selected_item(&mut self, view_height: u16, max_width: Option<usize>) {
        if self.items.is_empty() || view_height == 0 {
            return;
        }
        let sel = self.selected_item_index.min(self.items.len().saturating_sub(1));
        self.selected_item_index = sel;
        let (total_lines, offsets) = self.layout_offsets(max_width, Some(sel));
        let max_scroll = clamp_u16(total_lines).saturating_sub(view_height);

        if let Some(&start_offset) = offsets.get(sel) {
            let header_line = start_offset + if sel > 0 { 1 } else { 0 };
            let next_offset = offsets.get(sel + 1).copied().unwrap_or(total_lines);

            let vis_top = self.scroll as usize;
            let vis_bottom = vis_top + view_height as usize;

            if header_line < vis_top {
                self.auto_scroll = false;
                self.scroll = clamp_u16(start_offset).min(max_scroll);
            } else if next_offset > vis_bottom {
                self.auto_scroll = false;
                let item_height = next_offset.saturating_sub(start_offset);
                if item_height <= view_height as usize {
                    let target = next_offset.saturating_sub(view_height as usize);
                    self.scroll = clamp_u16(target).min(max_scroll);
                } else {
                    self.scroll = clamp_u16(start_offset).min(max_scroll);
                }
            }
        }
    }

    pub fn recenter_selected_item(&mut self, view_height: u16, max_width: Option<usize>) {
        if self.items.is_empty() || view_height == 0 {
            return;
        }
        let sel = self.selected_item_index.min(self.items.len().saturating_sub(1));
        self.selected_item_index = sel;
        let (total_lines, offsets) = self.layout_offsets(max_width, Some(sel));
        let max_scroll = clamp_u16(total_lines).saturating_sub(view_height);

        if let Some(&start_offset) = offsets.get(sel) {
            let next_offset = offsets.get(sel + 1).copied().unwrap_or(total_lines);
            let item_height = next_offset.saturating_sub(start_offset);

            if self.last_recenter_index == Some(sel) {
                self.recenter_cycle = (self.recenter_cycle + 1) % 3;
            } else {
                self.recenter_cycle = 0;
                self.last_recenter_index = Some(sel);
            }

            self.auto_scroll = false;
            match self.recenter_cycle {
                0 => {
                    // Center in window
                    let center_scroll = (start_offset as isize + (item_height as isize / 2)
                        - (view_height as isize / 2))
                        .clamp(0, max_scroll as isize) as u16;
                    self.scroll = center_scroll;
                }
                1 => {
                    // Align to top
                    self.scroll = clamp_u16(start_offset).min(max_scroll);
                }
                _ => {
                    // Align to bottom
                    let bottom_scroll = (next_offset as isize - view_height as isize)
                        .clamp(0, max_scroll as isize) as u16;
                    self.scroll = bottom_scroll;
                }
            }
        }
    }

    pub fn update_selected_item_from_scroll(
        &mut self,
        view_height: u16,
        max_width: Option<usize>,
    ) {
        if self.items.is_empty() {
            return;
        }
        let (_, offsets) = self.layout_offsets(max_width, None);
        if offsets.is_empty() {
            return;
        }

        let target_line = self.scroll as usize
            + (view_height as usize / 3).min((view_height as usize).saturating_sub(1));
        let mut selected = 0;
        for (idx, &start) in offsets.iter().enumerate() {
            if start <= target_line {
                selected = idx;
            } else {
                break;
            }
        }
        self.selected_item_index = selected.min(self.items.len().saturating_sub(1));
    }
}

pub fn compute_history_items_line_count(items: &[UiHistoryItem], max_width: Option<usize>) -> usize {
    compute_history_lines(items, max_width, None, None).len()
}

pub fn compute_history_lines(
    items: &[UiHistoryItem],
    max_width: Option<usize>,
    selected_index: Option<usize>,
    expanded_items: Option<&HashSet<u64>>,
) -> Vec<Line<'static>> {
    let (lines, _) = compute_history_lines_with_offsets(items, max_width, selected_index, expanded_items);
    lines
}

pub fn compute_history_lines_with_offsets(
    items: &[UiHistoryItem],
    max_width: Option<usize>,
    selected_index: Option<usize>,
    expanded_items: Option<&HashSet<u64>>,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let (lines, offsets, _) =
        compute_history_lines_full(items, max_width, selected_index, expanded_items, None);
    (lines, offsets)
}

pub fn compute_history_lines_full(
    items: &[UiHistoryItem],
    max_width: Option<usize>,
    selected_index: Option<usize>,
    expanded_items: Option<&HashSet<u64>>,
    flashing_block: Option<(u64, usize)>,
) -> (Vec<Line<'static>>, Vec<usize>, Vec<HistoryCodeBlock>) {
    let mut cache = HistoryBodyCache::default();
    layout_history(
        items,
        max_width,
        selected_index,
        expanded_items,
        flashing_block,
        &mut cache,
    )
}

fn wrap_one(line: Line<'static>, max_width: Option<usize>) -> Vec<Line<'static>> {
    match max_width {
        Some(width) => wrap_lines(vec![line], width),
        None => vec![line],
    }
}

fn build_item_header(item: &UiHistoryItem, is_selected: bool, is_expanded: bool) -> Line<'static> {
    let cursor_prefix = if is_selected { "● " } else { "  " };
    let prefix_style = if is_selected {
        Style::default().fg(Color::Cyan).bold()
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let fold_icon = if is_expanded { "▼ " } else { "▶ " };
    let fold_style = if is_expanded {
        Style::default().fg(Color::Yellow).bold()
    } else {
        Style::default().fg(Color::DarkGray).bold()
    };

    let badge = match item.kind {
        UiHistoryKind::User => Span::styled(
            " [USER] ",
            Style::default().bg(Color::Blue).fg(Color::White).bold(),
        ),
        UiHistoryKind::Assistant => Span::styled(
            " [ASSISTANT] ",
            Style::default().bg(Color::Green).fg(Color::Black).bold(),
        ),
        UiHistoryKind::System => {
            if item.commit_hash.is_some() && item.text.to_lowercase().contains("undid") {
                Span::styled(
                    " [UNDO] ",
                    Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
                )
            } else {
                Span::styled(
                    " [SYSTEM] ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                )
            }
        }
    };

    let id_span = Span::styled(
        format!(" #{}", item.id),
        Style::default().fg(Color::DarkGray).bold(),
    );

    let mut header_spans = vec![
        Span::styled(cursor_prefix, prefix_style),
        Span::styled(fold_icon, fold_style),
        badge,
        id_span,
    ];
    if let Some(hash) = &item.commit_hash {
        header_spans.push(Span::raw(" "));
        header_spans.push(Span::styled(
            format!("[COMMIT: {}]", hash),
            Style::default().fg(Color::Cyan).bold(),
        ));
    }
    if let Some(summary) = &item.summary {
        header_spans.push(Span::raw(" - "));
        header_spans.push(Span::styled(summary.clone(), Style::default().bold()));
    }
    Line::from(header_spans)
}

/// Builds the wrapped body of one item. Independent of selection, so it can be cached.
fn build_item_body(
    item: &UiHistoryItem,
    is_expanded: bool,
    max_width: Option<usize>,
    flash_id: Option<usize>,
) -> CachedBody {
    let text_style = match item.kind {
        UiHistoryKind::User => Style::default().fg(Color::Cyan),
        UiHistoryKind::Assistant => Style::default().fg(Color::White),
        UiHistoryKind::System => Style::default().fg(Color::Yellow),
    };

    let mut raw_lines: Vec<Line<'static>> = Vec::new();
    let mut extracted_blocks = Vec::new();
    let mut md_start_idx = 0;

    if is_expanded {
        if item.kind == UiHistoryKind::Assistant {
            let mut history_theme = crate::markdown::MarkdownTheme::answer();
            history_theme.line_prefix = Some(Span::raw("  "));
            let (md_lines, blocks) =
                crate::markdown::render_markdown_with_blocks(&item.text, &history_theme, flash_id);
            md_start_idx = raw_lines.len();
            extracted_blocks = blocks;
            if md_lines.is_empty() {
                for line in item.text.lines() {
                    raw_lines.push(Line::from(Span::styled(format!("  {}", line), text_style)));
                }
            } else {
                raw_lines.extend(md_lines);
            }
        } else {
            for line in item.text.lines() {
                raw_lines.push(Line::from(Span::styled(format!("  {}", line), text_style)));
            }
        }

        if !item.files.is_empty() {
            if item.kind == UiHistoryKind::Assistant {
                raw_lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("Modified files ({}):", item.files.len()),
                        Style::default().fg(Color::Cyan).bold(),
                    ),
                ]));
                for file_entry in &item.files {
                    let (op_label, op_style, path) = if let Some(p) = file_entry.strip_prefix("[NEW] ") {
                        ("[NEW] ", Style::default().fg(Color::Green).bold(), p)
                    } else if let Some(p) = file_entry.strip_prefix("[DEL] ") {
                        ("[DEL] ", Style::default().fg(Color::Red).bold(), p)
                    } else if let Some(p) = file_entry.strip_prefix("[OVERWRITE] ") {
                        ("[OVERWRITE] ", Style::default().fg(Color::Yellow).bold(), p)
                    } else if let Some(p) = file_entry.strip_prefix("[MOVE] ") {
                        ("[MOVE] ", Style::default().fg(Color::Cyan).bold(), p)
                    } else if let Some(p) = file_entry.strip_prefix("[EDIT] ") {
                        ("[EDIT] ", Style::default().fg(Color::Magenta).bold(), p)
                    } else {
                        ("• ", Style::default().fg(Color::Yellow).bold(), file_entry.as_str())
                    };

                    raw_lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(op_label, op_style),
                        Span::styled(path.to_string(), Style::default().fg(Color::White)),
                    ]));
                }
            } else {
                let mut file_spans = vec![
                    Span::raw("  "),
                    Span::styled("Context: ", Style::default().fg(Color::DarkGray).bold()),
                ];
                let files_joined = item.files.join(", ");
                file_spans.push(Span::styled(files_joined, Style::default().fg(Color::DarkGray)));
                raw_lines.push(Line::from(file_spans));
            }
        }
    } else {
        // Collapsed preview: display up to COLLAPSED_PREVIEW_LINES
        let all_raw_lines: Vec<&str> = item.text.lines().collect();
        let total_text_lines = all_raw_lines.len();
        let preview_slice = &all_raw_lines[..total_text_lines.min(COLLAPSED_PREVIEW_LINES)];

        for line in preview_slice {
            raw_lines.push(Line::from(Span::styled(format!("    {}", line), text_style)));
        }

        let extra_lines = total_text_lines.saturating_sub(COLLAPSED_PREVIEW_LINES);
        let files_count = item.files.len();

        let mut notes = Vec::new();
        if extra_lines > 0 {
            notes.push(format!("+{} more lines", extra_lines));
        }
        if files_count > 0 {
            notes.push(format!("{} modified files", files_count));
        }

        if !notes.is_empty() {
            let note_str = format!("    ... [{}]", notes.join(", "));
            raw_lines.push(Line::from(Span::styled(
                note_str,
                Style::default().fg(Color::DarkGray).italic(),
            )));
        }
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut raw_to_visual = Vec::with_capacity(raw_lines.len());
    for line in raw_lines {
        let line_start = lines.len();
        lines.extend(wrap_one(line, max_width));
        raw_to_visual.push((line_start, lines.len().saturating_sub(1)));
    }

    let mut blocks = Vec::new();
    for block in extracted_blocks {
        let raw_start = md_start_idx + block.start_line;
        let raw_end = md_start_idx + block.end_line;
        if let (Some(start), Some(end)) = (raw_to_visual.get(raw_start), raw_to_visual.get(raw_end)) {
            blocks.push(CachedCodeBlock {
                block_id: block.id,
                lang: block.lang,
                code: block.content,
                start: start.0,
                end: end.1,
            });
        }
    }

    CachedBody {
        text_len: item.text.len(),
        files_len: item.files.len(),
        lines,
        blocks,
    }
}

fn layout_history(
    items: &[UiHistoryItem],
    max_width: Option<usize>,
    selected_index: Option<usize>,
    expanded_items: Option<&HashSet<u64>>,
    flashing_block: Option<(u64, usize)>,
    cache: &mut HistoryBodyCache,
) -> (Vec<Line<'static>>, Vec<usize>, Vec<HistoryCodeBlock>) {
    let mut all_lines = Vec::new();
    let mut offsets = Vec::with_capacity(items.len());
    let mut code_blocks = Vec::new();
    let div_width = max_width.unwrap_or(80).max(10);

    cache.prepare(max_width, items);

    for (idx, item) in items.iter().enumerate() {
        offsets.push(all_lines.len());

        if idx > 0 {
            let divider = Line::from(Span::styled(
                "─".repeat(div_width),
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ));
            all_lines.extend(wrap_one(divider, max_width));
        }

        let is_selected = selected_index == Some(idx);
        let is_expanded = expanded_items.map(|set| set.contains(&item.id)).unwrap_or(false);
        all_lines.extend(wrap_one(
            build_item_header(item, is_selected, is_expanded),
            max_width,
        ));

        let flash_id = flashing_block.and_then(|(i_id, b_id)| {
            if i_id == item.id {
                Some(b_id)
            } else {
                None
            }
        });

        // Items with an active copy flash are rendered fresh so the cache never stores flash styling.
        let uncached;
        let body: &CachedBody = if flash_id.is_some() {
            uncached = build_item_body(item, is_expanded, max_width, flash_id);
            &uncached
        } else {
            cache.body_for(item, is_expanded, max_width)
        };

        let body_base = all_lines.len();
        all_lines.extend(body.lines.iter().cloned());
        for block in &body.blocks {
            code_blocks.push(HistoryCodeBlock {
                item_id: item.id,
                block_id: block.block_id,
                lang: block.lang.clone(),
                code: block.code.clone(),
                visual_start_line: body_base + block.start,
                visual_end_line: body_base + block.end,
            });
        }
    }

    (all_lines, offsets, code_blocks)
}

pub fn render_history_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    main_area: Rect,
    info_area: Rect,
) {
    let hist_block = Block::default().padding(Padding::horizontal(1));
    let inner = hist_block.inner(main_area);
    let content_height = inner.height;
    let content_width = (inner.width as usize).max(10);
    state.last_model_height = content_height;
    state.history_view.content_width = content_width;

    if !state.history_view.items.is_empty()
        && state.history_view.selected_item_index >= state.history_view.items.len()
    {
        state.history_view.selected_item_index = state.history_view.items.len().saturating_sub(1);
    }

    if let Some((_, inst)) = state.history_view.copy_flash {
        if inst.elapsed().as_secs_f32() >= 2.0 {
            state.history_view.copy_flash = None;
        }
    }

    let flash_target = state.history_view.copy_flash.map(|(pair, _)| pair);
    let (history_lines, _, code_blocks) = layout_history(
        &state.history_view.items,
        Some(content_width),
        Some(state.history_view.selected_item_index),
        Some(&state.history_view.expanded_items),
        flash_target,
        &mut state.history_view.body_cache,
    );
    let total_lines = history_lines.len();
    state.history_view.rendered_lines_count = total_lines;
    state.history_view.code_blocks = code_blocks;
    state.history_view.content_rect = (
        inner.x,
        inner.y,
        inner.width,
        content_height,
    );

    let max_scroll = clamp_u16(total_lines).saturating_sub(content_height);
    if state.history_view.auto_scroll || state.history_view.scroll > max_scroll {
        state.history_view.scroll = max_scroll;
    }

    let history_paragraph = Paragraph::new(history_lines)
        .block(hist_block)
        .scroll((state.history_view.scroll, 0));
    frame.render_widget(history_paragraph, main_area);

    let sel_idx = if state.history_view.items.is_empty() {
        0
    } else {
        state.history_view.selected_item_index + 1
    };
    let total_shown = state.history_view.items.len();

    let info_line = Line::from(vec![
        Span::styled(
            format!(" History | Item {}/{} ", sel_idx, total_shown),
            Style::default().fg(Color::White).bold(),
        ),
        Span::raw(" | "),
        Span::styled(
            if state.history_view.auto_scroll {
                "[Auto-scroll: ON]"
            } else {
                "[Auto-scroll: OFF]"
            },
            if state.history_view.auto_scroll {
                Style::default().fg(Color::Green).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::raw(" | "),
        Span::styled(
            format!("~{} tokens", state.history_view.get_estimated_tokens()),
            Style::default().fg(Color::Cyan),
        ),
    ]);
    let prompt_widget = Paragraph::new(info_line)
        .block(Block::default().padding(Padding::horizontal(1)));
    frame.render_widget(prompt_widget, info_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_history_lines_assistant_modified_files() {
        let item = UiHistoryItem {
            id: 2,
            kind: UiHistoryKind::Assistant,
            text: "Updated codebase".to_string(),
            summary: Some("Refactoring".to_string()),
            commit_hash: Some("abcdef1".to_string()),
            files: vec![
                "[EDIT] src/main.rs".to_string(),
                "[NEW] src/util.rs".to_string(),
                "[DEL] src/old.rs".to_string(),
                "[MOVE] src/prev.rs -> src/next.rs".to_string(),
            ],
        };
        let mut expanded = HashSet::new();
        expanded.insert(2);
        let lines = compute_history_lines(&[item], None, None, Some(&expanded));
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();

        assert!(rendered.iter().any(|line| line.contains("[COMMIT: abcdef1]")));
        assert!(rendered.iter().any(|line| line.contains("Refactoring")));
        assert!(rendered.iter().any(|line| line.contains("Updated codebase")));
        assert!(rendered.iter().any(|line| line.contains("Modified files (4):")));
        assert!(rendered.iter().any(|line| line.contains("[EDIT] ") && line.contains("src/main.rs")));
        assert!(rendered.iter().any(|line| line.contains("[NEW] ") && line.contains("src/util.rs")));
        assert!(rendered.iter().any(|line| line.contains("[DEL] ") && line.contains("src/old.rs")));
        assert!(rendered.iter().any(|line| line.contains("[MOVE] ") && line.contains("src/prev.rs -> src/next.rs")));
    }

    #[test]
    fn test_compute_history_lines_collapsed_by_default() {
        let item = UiHistoryItem {
            id: 2,
            kind: UiHistoryKind::Assistant,
            text: "Line 1\nLine 2\nLine 3\nLine 4\nLine 5".to_string(),
            summary: Some("Overview".to_string()),
            commit_hash: Some("abcdef1".to_string()),
            files: vec!["[EDIT] src/lib.rs".to_string()],
        };
        // By default without expanded set: collapsed
        let lines = compute_history_lines(&[item], None, None, None);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();

        assert!(rendered.iter().any(|line| line.contains("▶ ") && line.contains("[ASSISTANT]")));
        assert!(rendered.iter().any(|line| line.contains("Line 1")));
        assert!(rendered.iter().any(|line| line.contains("Line 2")));
        assert!(rendered.iter().any(|line| line.contains("Line 3")));
        assert!(!rendered.iter().any(|line| line.contains("Line 5")));
        assert!(rendered.iter().any(|line| line.contains("[+2 more lines, 1 modified files]")));
    }

    #[test]
    fn test_compute_history_lines_wrapping() {
        let long_text = "This is a very long line of text that should definitely be wrapped across multiple lines when rendered in the history view widget with a constrained width constraint.".to_string();
        let item = UiHistoryItem {
            id: 1,
            kind: UiHistoryKind::User,
            text: long_text,
            summary: None,
            commit_hash: None,
            files: Vec::new(),
        };

        let unwrapped = compute_history_lines(std::slice::from_ref(&item), None, None, None);
        let wrapped = compute_history_lines(std::slice::from_ref(&item), Some(40), None, None);

        assert!(wrapped.len() > unwrapped.len());
        for line in &wrapped {
            assert!(line.width() <= 40, "Line width {} exceeds max_width 40", line.width());
        }
    }

    #[test]
    fn test_compute_history_lines_selection_indicator() {
        let item1 = UiHistoryItem {
            id: 1,
            kind: UiHistoryKind::User,
            text: "Hello".to_string(),
            summary: None,
            commit_hash: None,
            files: Vec::new(),
        };
        let item2 = UiHistoryItem {
            id: 2,
            kind: UiHistoryKind::Assistant,
            text: "World".to_string(),
            summary: None,
            commit_hash: None,
            files: Vec::new(),
        };
        let lines = compute_history_lines(&[item1, item2], None, Some(1), None);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();

        assert!(rendered.iter().any(|l| l.contains("● ▶  [ASSISTANT]  #2")));
        assert!(rendered.iter().any(|l| l.contains("  ▶  [USER]  #1")));
    }

    #[test]
    fn test_body_cache_matches_uncached_layout_and_resets_on_width_change() {
        let items = vec![
            UiHistoryItem {
                id: 1,
                kind: UiHistoryKind::User,
                text: "Hello there".to_string(),
                summary: None,
                commit_hash: None,
                files: Vec::new(),
            },
            UiHistoryItem {
                id: 2,
                kind: UiHistoryKind::Assistant,
                text: "```rust\nfn main() {}\n```\nDone".to_string(),
                summary: None,
                commit_hash: None,
                files: Vec::new(),
            },
        ];
        let mut expanded = HashSet::new();
        expanded.insert(2);

        let mut cache = HistoryBodyCache::default();
        let _ = layout_history(&items, Some(60), Some(0), Some(&expanded), None, &mut cache);
        assert_eq!(cache.entries.len(), 2);

        let (cached_lines, cached_offsets, cached_blocks) =
            layout_history(&items, Some(60), Some(1), Some(&expanded), None, &mut cache);
        let (fresh_lines, fresh_offsets, fresh_blocks) =
            compute_history_lines_full(&items, Some(60), Some(1), Some(&expanded), None);
        assert_eq!(cached_lines, fresh_lines);
        assert_eq!(cached_offsets, fresh_offsets);
        assert_eq!(cached_blocks.len(), fresh_blocks.len());

        let _ = layout_history(&items, Some(30), Some(1), Some(&expanded), None, &mut cache);
        assert_eq!(cache.width, Some(30));
    }

    #[test]
    fn test_clamp_u16_saturates() {
        assert_eq!(clamp_u16(10), 10);
        assert_eq!(clamp_u16(70_000), u16::MAX);
    }
}
