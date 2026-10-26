use std::collections::HashSet;

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use tauqe_protocol::{
    edit::EditFileOp, HistoryEntryStatus, HistoryFileChange, UiHistoryItem, UiHistoryKind,
};

use crate::clipboard::CopyResult;
use crate::i18n::active_t;
use crate::markdown::MarkdownBodyCache;
use crate::ui::wrap_lines;
use crate::views::recenter::RecenterState;

pub mod input;
pub mod render;

pub use self::input::{handle_history_key, request_older_history, HISTORY_COMMAND_MAP};
pub use self::render::render_history_view;

const COLLAPSED_PREVIEW_LINES: usize = 3;

/// Columns every rendered History body line is prefixed with.
///
/// The Markdown theme paints the prefix and the copy-button geometry offsets by
/// the same constant, so a drawn button and its hit region share one source of
/// truth (c-1).
pub(crate) const LINE_PREFIX: &str = "  ";

/// Saturating conversion for scroll arithmetic; avoids wrap-around on very long histories.
fn clamp_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// A fenced code block placed in the rendered document, addressed by owner.
#[derive(Debug, Clone)]
pub struct HistoryCodeBlock {
    pub item_id: u64,
    pub block_id: usize,
    pub lang: String,
    pub code: String,
    pub visual_start_line: usize,
    pub visual_end_line: usize,
}

/// A fenced code block located inside one item body.
///
/// `start` and `end` are inclusive indices into the body lines, so the layout
/// places the copy button over the exact rows the cache painted without parsing
/// the Markdown a second time (c-1).
#[derive(Debug, Clone)]
struct BodyCodeBlock {
    block_id: usize,
    lang: String,
    code: String,
    start: usize,
    end: usize,
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
    pub recenter_state: RecenterState,
    pub selected_item_index: usize,
    pub content_width: usize,
    pub expanded_items: HashSet<u64>,
    pub code_blocks: Vec<HistoryCodeBlock>,
    pub copy_flash: Option<((u64, usize), std::time::Instant)>,
    pub theme_mode: crate::config::ThemeMode,
    /// Rendered Markdown bodies of expanded answers, owned by the shared cache.
    ///
    /// The cache tracks the wrap width, the theme mode, and the color depth
    /// itself, so the view keeps no second invalidation rule of its own: a
    /// cursor move or a scroll serves the retained body, while a resize or a new
    /// palette drops it (c-1, c-3).
    pub(crate) body_cache: MarkdownBodyCache<u64>,
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
            recenter_state: RecenterState::default(),
            selected_item_index: 0,
            content_width: 80,
            expanded_items: HashSet::new(),
            code_blocks: Vec::new(),
            copy_flash: None,
            theme_mode: crate::config::ThemeMode::Auto,
            body_cache: MarkdownBodyCache::new(),
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

    /// Invalidates the cached Markdown bodies so the next render re-parses every
    /// visible entry under the newly active palette. Called on hot theme
    /// reloads, where the light/dark mode is unchanged but the palette is not.
    pub fn refresh_theme(&mut self) {
        self.body_cache.clear();
    }

    pub fn get_estimated_tokens(&self) -> u64 {
        if self.estimated_tokens > 0 {
            self.estimated_tokens
        } else {
            let total_chars: usize = self.items.iter().map(|it| it.text.chars().count()).sum();
            total_chars.div_ceil(4) as u64
        }
    }

    /// Applies a server-side session compaction event to the in-memory view.
    ///
    /// Compaction is a structural replacement, not an append: the summarized
    /// head entries are dropped, the authoritative `summary` becomes the first
    /// item (index zero), and the preserved `tail_ids` follow in their original
    /// monotonic order. Stale fold state and cached bodies of the removed head
    /// are discarded, and the viewport is re-anchored against the new geometry.
    pub fn apply_compaction(
        &mut self,
        summary: UiHistoryItem,
        tail_ids: &[u64],
        total_count: usize,
        estimated_tokens: u64,
        view_height: u16,
    ) {
        let tail: HashSet<u64> = tail_ids.iter().copied().collect();

        // Drop every entry folded into the summary; whatever the server kept in
        // the tail survives in its original, monotonically increasing order.
        self.items.retain(|it| tail.contains(&it.id));
        // The summary always occupies index zero.
        self.items.insert(0, summary);

        self.total_count = total_count;
        self.estimated_tokens = estimated_tokens;

        // Fold state and cached bodies of the removed head entries are stale. The
        // shared cache owns no per-key eviction, so the structural replacement
        // drops every body and the retained tail renders once on the next frame
        // (c-3).
        let live: HashSet<u64> = self.items.iter().map(|it| it.id).collect();
        self.expanded_items.retain(|id| live.contains(id));
        self.body_cache.clear();

        // Recompute geometry against the new item set and re-anchor the viewport.
        let max_width = Some(self.content_width);
        if self.auto_scroll {
            self.selected_item_index = self.items.len().saturating_sub(1);
        } else {
            self.selected_item_index = self
                .selected_item_index
                .min(self.items.len().saturating_sub(1));
        }
        let (total_lines, _) = self.layout_offsets(max_width, Some(self.selected_item_index));
        self.rendered_lines_count = total_lines;
        let max_scroll = clamp_u16(total_lines).saturating_sub(view_height);
        if self.auto_scroll {
            self.scroll = max_scroll;
        } else {
            self.scroll = self.scroll.min(max_scroll);
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
            self.theme_mode,
            &mut self.body_cache,
        );
        (lines.len(), offsets)
    }

    pub fn scroll_to_selected_item(&mut self, view_height: u16, max_width: Option<usize>) {
        if self.items.is_empty() || view_height == 0 {
            return;
        }
        let sel = self
            .selected_item_index
            .min(self.items.len().saturating_sub(1));
        self.selected_item_index = sel;
        let (total_lines, offsets) = self.layout_offsets(max_width, Some(sel));
        let max_scroll = clamp_u16(total_lines).saturating_sub(view_height);

        let is_last = sel + 1 >= self.items.len();
        if is_last {
            self.auto_scroll = true;
            self.scroll = max_scroll;
            return;
        }

        if let Some(&start_offset) = offsets.get(sel) {
            let header_line = start_offset;
            let next_offset = offsets.get(sel + 1).copied().unwrap_or(total_lines);

            let vis_top = self.scroll as usize;
            let vis_bottom = vis_top + view_height as usize;

            if header_line < vis_top {
                self.auto_scroll = false;
                self.scroll = clamp_u16(start_offset).min(max_scroll);
            } else if next_offset > vis_bottom {
                self.auto_scroll = false;
                let target = next_offset.saturating_sub(view_height as usize);
                self.scroll = clamp_u16(target).min(max_scroll);
            }
        }
    }

    pub fn recenter_selected_item(&mut self, view_height: u16, max_width: Option<usize>) {
        if self.items.is_empty() || view_height == 0 {
            return;
        }
        let sel = self
            .selected_item_index
            .min(self.items.len().saturating_sub(1));
        self.selected_item_index = sel;
        let (total_lines, offsets) = self.layout_offsets(max_width, Some(sel));

        if let Some(&start_offset) = offsets.get(sel) {
            let next_offset = offsets.get(sel + 1).copied().unwrap_or(total_lines);
            self.auto_scroll = false;
            self.scroll = self.recenter_state.recenter(
                sel,
                start_offset,
                next_offset,
                total_lines,
                view_height,
            );
        }
    }

    pub fn update_selected_item_from_scroll(&mut self, view_height: u16, max_width: Option<usize>) {
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

    /// Copies the rendered code block `(item_id, block_id)` and flashes it.
    ///
    /// The view owns the blocks it drew, so it resolves the action to the exact
    /// source it rendered and arms the flash badge; the pointer dispatcher
    /// receives only the typed action and the reported outcome (c-1, c-2).
    /// `None` means the block no longer exists and nothing happens.
    pub fn copy_code_block(
        &mut self,
        item_id: u64,
        block_id: usize,
    ) -> Option<(CopyResult, usize)> {
        let code = self
            .code_blocks
            .iter()
            .find(|block| block.item_id == item_id && block.block_id == block_id)
            .map(|block| block.code.clone())?;
        let lines = code.lines().count().max(1);
        let result = crate::clipboard::copy_to_clipboard(&code);
        if result.is_success() {
            self.copy_flash = Some(((item_id, block_id), std::time::Instant::now()));
        }
        Some((result, lines))
    }
}

pub fn compute_history_items_line_count(
    items: &[UiHistoryItem],
    max_width: Option<usize>,
) -> usize {
    compute_history_lines(items, max_width, None, None).len()
}

pub fn compute_history_lines(
    items: &[UiHistoryItem],
    max_width: Option<usize>,
    selected_index: Option<usize>,
    expanded_items: Option<&HashSet<u64>>,
) -> Vec<Line<'static>> {
    let (lines, _) =
        compute_history_lines_with_offsets(items, max_width, selected_index, expanded_items);
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
    let mut cache = MarkdownBodyCache::new();
    layout_history(
        items,
        max_width,
        selected_index,
        expanded_items,
        flashing_block,
        crate::config::ThemeMode::Auto,
        &mut cache,
    )
}

fn wrap_one(line: Line<'static>, max_width: Option<usize>) -> Vec<Line<'static>> {
    match max_width {
        Some(width) => wrap_lines(vec![line], width),
        None => vec![line],
    }
}

fn build_item_header(
    item: &UiHistoryItem,
    is_selected: bool,
    is_expanded: bool,
    theme_mode: crate::config::ThemeMode,
) -> Line<'static> {
    let engine = crate::theme::engine::active_engine(theme_mode);
    let palette = engine.palette();
    let cursor_prefix = if is_selected { "● " } else { "  " };
    let prefix_style = if is_selected {
        engine.fg(palette.status_info).bold()
    } else {
        engine.fg(palette.text_muted)
    };

    let fold_icon = if is_expanded { "▼ " } else { "▶ " };
    let fold_style = if is_expanded {
        engine.fg(palette.status_warning).bold()
    } else {
        engine.fg(palette.text_muted).bold()
    };

    let badge = match item.kind {
        UiHistoryKind::User => Span::styled(
            format!(" [{}] ", active_t("history.badge.user")),
            engine
                .bg(palette.status_info)
                .fg(engine.color(palette.text_inverse))
                .bold(),
        ),
        UiHistoryKind::Assistant => Span::styled(
            format!(" [{}] ", active_t("history.badge.assistant")),
            engine
                .bg(palette.status_success)
                .fg(engine.color(palette.text_inverse))
                .bold(),
        ),
        // The undo-commit command was removed, so a System record is
        // classified exclusively by its typed kind and never by
        // keyword-searching the server-generated prose (Conventions §1.5).
        UiHistoryKind::System => Span::styled(
            format!(" [{}] ", active_t("history.badge.system")),
            engine
                .bg(palette.text_muted)
                .fg(engine.color(palette.text_inverse))
                .bold(),
        ),
    };

    let id_span = Span::styled(
        format!(" #{}", item.id),
        engine.fg(palette.text_muted).bold(),
    );

    // The lifecycle status is read from the typed field, never inferred from
    // surrounding events or message text: a record stays visible in the
    // developer's chronicle while the model context only receives committed
    // results.
    let status_badge = match item.status {
        HistoryEntryStatus::Committed => None,
        HistoryEntryStatus::Active => Some(Span::styled(
            format!(" [{}] ", active_t("history.badge.in_flight")),
            engine
                .bg(palette.status_info)
                .fg(engine.color(palette.text_inverse))
                .bold(),
        )),
        HistoryEntryStatus::RolledBack => Some(Span::styled(
            format!(" [{}] ", active_t("history.badge.rolled_back")),
            engine
                .bg(palette.status_error)
                .fg(engine.color(palette.text_inverse))
                .bold(),
        )),
    };

    let mut header_spans = vec![
        Span::styled(cursor_prefix, prefix_style),
        Span::styled(fold_icon, fold_style),
        badge,
    ];
    if let Some(status_badge) = status_badge {
        header_spans.push(Span::raw(" "));
        header_spans.push(status_badge);
    }
    header_spans.push(id_span);
    if let Some(hash) = &item.commit_hash {
        header_spans.push(Span::raw(" "));
        header_spans.push(Span::styled(
            format!(
                "[{}]",
                active_t("history.badge.commit").replacen("{}", hash.as_str(), 1)
            ),
            engine.fg(palette.status_info).bold(),
        ));
    }
    if let Some(summary) = &item.summary {
        header_spans.push(Span::raw(" - "));
        header_spans.push(Span::styled(summary.clone(), Style::default().bold()));
    }
    Line::from(header_spans)
}

/// Badge label and style of one typed file change.
///
/// The operation is authored once by the producer and matched here, so the
/// label and its color can never drift from the operation they describe.
/// `None` marks a path that carries no operation — a context file or a plain
/// listing — and keeps the neutral bullet (Conventions §1.5).
fn file_change_badge(
    op: Option<EditFileOp>,
    theme_mode: crate::config::ThemeMode,
) -> (&'static str, Style) {
    let engine = crate::theme::engine::active_engine(theme_mode);
    let palette = engine.palette();
    match op {
        Some(EditFileOp::Create) => ("[NEW] ", engine.fg(palette.status_success).bold()),
        Some(EditFileOp::Delete) => ("[DEL] ", engine.fg(palette.status_error).bold()),
        Some(EditFileOp::Overwrite) => ("[OVERWRITE] ", engine.fg(palette.status_warning).bold()),
        Some(EditFileOp::Move) => ("[MOVE] ", engine.fg(palette.status_info).bold()),
        Some(EditFileOp::Replace) => ("[EDIT] ", engine.fg(palette.button_active_bg).bold()),
        None => ("• ", engine.fg(palette.status_warning).bold()),
    }
}

/// Path half of a rendered file change: the path, or the move arrow pair.
fn file_change_path(change: &HistoryFileChange) -> String {
    match (change.op, change.to.as_deref()) {
        (Some(EditFileOp::Move), Some(to)) => format!("{} -> {}", change.path, to),
        _ => change.path.clone(),
    }
}

/// Builds the rendered body of one item: everything below its header.
///
/// Only the Markdown answer of an expanded assistant record is expensive to
/// produce, so the shared [`MarkdownBodyCache`] owns it and returns the wrapped
/// lines together with the fenced blocks they contain. The bands around it —
/// the collapsed preview, the modified-file list, the restore hint — are
/// assembled from typed fields on every frame and need no cache of their own,
/// which is also why a lifecycle status change never invalidates a body (c-1,
/// c-3).
fn build_item_body(
    item: &UiHistoryItem,
    is_expanded: bool,
    max_width: Option<usize>,
    theme_mode: crate::config::ThemeMode,
    cache: &mut MarkdownBodyCache<u64>,
    flash_id: Option<usize>,
) -> (Vec<Line<'static>>, Vec<BodyCodeBlock>) {
    let engine = crate::theme::engine::active_engine(theme_mode);
    let palette = engine.palette();
    let text_style = match item.kind {
        UiHistoryKind::User => engine.fg(palette.status_info),
        UiHistoryKind::Assistant => engine.fg(palette.text_primary),
        UiHistoryKind::System => engine.fg(palette.status_warning),
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut blocks: Vec<BodyCodeBlock> = Vec::new();

    if is_expanded {
        if item.kind == UiHistoryKind::Assistant {
            let mut history_theme = crate::markdown::MarkdownTheme::answer_themed(theme_mode);
            history_theme.line_prefix = Some(Span::raw(LINE_PREFIX));
            if let Some(w) = max_width {
                history_theme.max_width = Some(w.saturating_sub(2));
            }
            // The Markdown body leads the item body, so its block positions are
            // shifted by the lines already emitted above it instead of being
            // re-derived from a second parse (c-1).
            let md_offset = lines.len();
            let (md_lines, md_blocks) = cache.lines_and_blocks_for(
                &item.id,
                &item.text,
                &history_theme,
                max_width,
                theme_mode,
                flash_id,
            );
            if md_lines.is_empty() {
                for line in item.text.lines() {
                    lines.extend(wrap_one(
                        Line::from(Span::styled(format!("  {}", line), text_style)),
                        max_width,
                    ));
                }
            } else {
                lines.extend(md_lines.iter().cloned());
                blocks.extend(md_blocks.iter().map(|block| BodyCodeBlock {
                    block_id: block.id,
                    lang: block.lang.clone(),
                    code: block.content.clone(),
                    start: md_offset + block.start,
                    end: md_offset + block.end,
                }));
            }
        } else {
            for line in item.text.lines() {
                lines.extend(wrap_one(
                    Line::from(Span::styled(format!("  {}", line), text_style)),
                    max_width,
                ));
            }
        }

        if !item.files.is_empty() {
            if item.kind == UiHistoryKind::Assistant {
                lines.extend(wrap_one(
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(
                            active_t("history.modified_files_label").replacen(
                                "{}",
                                &item.files.len().to_string(),
                                1,
                            ),
                            engine.fg(palette.status_info).bold(),
                        ),
                    ]),
                    max_width,
                ));
                for change in &item.files {
                    let (op_label, op_style) = file_change_badge(change.op, theme_mode);
                    lines.extend(wrap_one(
                        Line::from(vec![
                            Span::raw("    "),
                            Span::styled(op_label, op_style),
                            Span::styled(file_change_path(change), engine.fg(palette.text_primary)),
                        ]),
                        max_width,
                    ));
                }
            } else {
                let mut file_spans = vec![
                    Span::raw("  "),
                    Span::styled(
                        active_t("history.context_label"),
                        engine.fg(palette.text_muted).bold(),
                    ),
                ];
                let files_joined = item
                    .files
                    .iter()
                    .map(|change| change.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                file_spans.push(Span::styled(files_joined, engine.fg(palette.text_muted)));
                lines.extend(wrap_one(Line::from(file_spans), max_width));
            }
        }
    } else {
        // Collapsed preview: display up to COLLAPSED_PREVIEW_LINES
        let all_raw_lines: Vec<&str> = item.text.lines().collect();
        let total_text_lines = all_raw_lines.len();
        let preview_slice = &all_raw_lines[..total_text_lines.min(COLLAPSED_PREVIEW_LINES)];

        for line in preview_slice {
            lines.extend(wrap_one(
                Line::from(Span::styled(format!("    {}", line), text_style)),
                max_width,
            ));
        }

        let extra_lines = total_text_lines.saturating_sub(COLLAPSED_PREVIEW_LINES);
        let files_count = item.files.len();

        let mut notes = Vec::new();
        if extra_lines > 0 {
            notes.push(active_t("history.more_lines_note").replacen(
                "{}",
                &extra_lines.to_string(),
                1,
            ));
        }
        if files_count > 0 {
            notes.push(active_t("history.modified_files_note").replacen(
                "{}",
                &files_count.to_string(),
                1,
            ));
        }

        if !notes.is_empty() {
            let note_str = format!("    ... [{}]", notes.join(", "));
            lines.extend(wrap_one(
                Line::from(Span::styled(
                    note_str,
                    engine.fg(palette.text_muted).italic(),
                )),
                max_width,
            ));
        }
    }

    // A rolled back record explains why it never reached the model context and
    // how the developer can recover the prompt without retyping it. The hint is
    // derived from the typed status, so it needs no cache invalidation.
    if item.status == HistoryEntryStatus::RolledBack {
        lines.extend(wrap_one(
            Line::from(Span::styled(
                format!("    {}", active_t("history.rolled_back_hint")),
                engine.fg(palette.status_warning).italic(),
            )),
            max_width,
        ));
    }

    (lines, blocks)
}

fn layout_history(
    items: &[UiHistoryItem],
    max_width: Option<usize>,
    selected_index: Option<usize>,
    expanded_items: Option<&HashSet<u64>>,
    flashing_block: Option<(u64, usize)>,
    theme_mode: crate::config::ThemeMode,
    cache: &mut MarkdownBodyCache<u64>,
) -> (Vec<Line<'static>>, Vec<usize>, Vec<HistoryCodeBlock>) {
    let engine = crate::theme::engine::active_engine(theme_mode);
    let palette = engine.palette();
    let mut all_lines = Vec::new();
    let mut offsets = Vec::with_capacity(items.len());
    let mut code_blocks = Vec::new();
    let div_width = max_width.unwrap_or(80).max(10);

    for (idx, item) in items.iter().enumerate() {
        if idx > 0 {
            let divider = Line::from(Span::styled(
                "─".repeat(div_width),
                engine.fg(palette.border),
            ));
            all_lines.extend(wrap_one(divider, max_width));
        }

        offsets.push(all_lines.len());

        let is_selected = selected_index == Some(idx);
        let is_expanded = expanded_items
            .map(|set| set.contains(&item.id))
            .unwrap_or(false);
        all_lines.extend(wrap_one(
            build_item_header(item, is_selected, is_expanded, theme_mode),
            max_width,
        ));

        // The flash rides in the cache as part of the rendering variant, so a
        // highlighted body never needs a rendering path of its own.
        let flash_id = flashing_block.and_then(|(i_id, b_id)| (i_id == item.id).then_some(b_id));

        let body_base = all_lines.len();
        let (body_lines, body_blocks) =
            build_item_body(item, is_expanded, max_width, theme_mode, cache, flash_id);
        all_lines.extend(body_lines);
        for block in body_blocks {
            code_blocks.push(HistoryCodeBlock {
                item_id: item.id,
                block_id: block.block_id,
                lang: block.lang,
                code: block.code,
                visual_start_line: body_base + block.start,
                visual_end_line: body_base + block.end,
            });
        }
    }

    (all_lines, offsets, code_blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain text of every rendered line, joined from its spans.
    ///
    /// The active palette is process-wide state, so two layouts produced inside
    /// the same test may carry different colors. The cache contract covers the
    /// rendered text and the geometry, so both are compared by content instead of
    /// by resolved style.
    fn rendered_text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    /// Collapses every whitespace run into a single space.
    ///
    /// The body renderer wraps long sentences at the item width, so a phrase
    /// that was written on one line is drawn on several. Normalizing both the
    /// rendered body and the expected phrase makes the comparison independent
    /// of where the terminal width broke the line.
    fn normalize_whitespace(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn history_item(id: u64, kind: UiHistoryKind, text: &str) -> UiHistoryItem {
        UiHistoryItem {
            id,
            kind,
            text: text.to_string(),
            summary: None,
            commit_hash: None,
            files: Vec::new(),
            status: HistoryEntryStatus::Committed,
        }
    }

    /// Expanded assistant record carrying a fenced code block.
    fn assistant_item(id: u64, text: &str) -> UiHistoryItem {
        history_item(id, UiHistoryKind::Assistant, text)
    }

    /// History view holding one expanded answer followed by a user prompt.
    fn cached_history_fixture() -> HistoryViewState {
        let mut st = HistoryViewState {
            content_width: 60,
            items: vec![
                assistant_item(1, "Intro.\n\n```rust\nfn main() {}\n```\n\nOutro."),
                history_item(2, UiHistoryKind::User, "Explain the change"),
            ],
            ..Default::default()
        };
        st.expanded_items.insert(1);
        st
    }

    #[test]
    fn test_compute_history_lines_assistant_modified_files() {
        let mut item = assistant_item(2, "Updated codebase");
        item.summary = Some("Refactoring".to_string());
        item.commit_hash = Some("abcdef1".to_string());
        item.files = vec![
            HistoryFileChange::operated(EditFileOp::Replace, "src/main.rs"),
            HistoryFileChange::operated(EditFileOp::Create, "src/util.rs"),
            HistoryFileChange::operated(EditFileOp::Delete, "src/old.rs"),
            HistoryFileChange::moved("src/prev.rs", "src/next.rs"),
        ];
        let mut expanded = HashSet::new();
        expanded.insert(2);
        let lines = compute_history_lines(&[item], None, None, Some(&expanded));
        let rendered = rendered_text(&lines);

        assert!(rendered
            .iter()
            .any(|line| line.contains("[COMMIT: abcdef1]")));
        assert!(rendered.iter().any(|line| line.contains("Refactoring")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("Updated codebase")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("Modified files (4):")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("[EDIT] ") && line.contains("src/main.rs")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("[NEW] ") && line.contains("src/util.rs")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("[DEL] ") && line.contains("src/old.rs")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("[MOVE] ") && line.contains("src/prev.rs -> src/next.rs")));
    }

    #[test]
    fn test_compute_history_lines_collapsed_by_default() {
        let mut item = assistant_item(2, "Line 1\nLine 2\nLine 3\nLine 4\nLine 5");
        item.summary = Some("Overview".to_string());
        item.commit_hash = Some("abcdef1".to_string());
        item.files = vec![HistoryFileChange::operated(
            EditFileOp::Replace,
            "src/lib.rs",
        )];

        // By default without expanded set: collapsed
        let lines = compute_history_lines(&[item], None, None, None);
        let rendered = rendered_text(&lines);

        assert!(rendered
            .iter()
            .any(|line| line.contains("▶ ") && line.contains("[ASSISTANT]")));
        assert!(rendered.iter().any(|line| line.contains("Line 1")));
        assert!(rendered.iter().any(|line| line.contains("Line 2")));
        assert!(rendered.iter().any(|line| line.contains("Line 3")));
        assert!(!rendered.iter().any(|line| line.contains("Line 5")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("[+2 more lines, 1 modified files]")));
    }

    /// A path that itself begins with a retired display prefix is a plain
    /// record, so it renders with the neutral bullet instead of a false
    /// operation badge (Conventions §1.5).
    #[test]
    fn test_plain_record_with_prefix_shaped_path_keeps_the_bullet() {
        let mut item = assistant_item(5, "Touched one file");
        item.files = vec![HistoryFileChange::plain("[EDIT] src/lib.rs")];
        let mut expanded = HashSet::new();
        expanded.insert(5);

        let rendered = rendered_text(&compute_history_lines(&[item], None, None, Some(&expanded)));
        let file_line = rendered
            .iter()
            .find(|line| line.contains("src/lib.rs"))
            .expect("the file record is rendered");

        assert!(
            file_line.contains("• [EDIT] src/lib.rs"),
            "the path is kept verbatim behind the neutral bullet, got {file_line:?}"
        );
        assert!(
            !file_line.trim_start().starts_with("[EDIT] "),
            "a prefix-shaped path must never be badged as an edit"
        );
    }

    #[test]
    fn test_compute_history_lines_wrapping() {
        let long_text = "This is a very long line of text that should definitely be wrapped across multiple lines when rendered in the history view widget with a constrained width constraint.".to_string();
        let item = history_item(1, UiHistoryKind::User, &long_text);

        let unwrapped = compute_history_lines(std::slice::from_ref(&item), None, None, None);
        let wrapped = compute_history_lines(std::slice::from_ref(&item), Some(40), None, None);

        assert!(wrapped.len() > unwrapped.len());
        for line in &wrapped {
            assert!(
                line.width() <= 40,
                "Line width {} exceeds max_width 40",
                line.width()
            );
        }
    }

    #[test]
    fn test_compute_history_lines_selection_indicator() {
        let item1 = history_item(1, UiHistoryKind::User, "Hello");
        let item2 = assistant_item(2, "World");
        let lines = compute_history_lines(&[item1, item2], None, Some(1), None);
        let rendered = rendered_text(&lines);

        assert!(rendered.iter().any(|l| l.contains("● ▶  [ASSISTANT]  #2")));
        assert!(rendered.iter().any(|l| l.contains("  ▶  [USER]  #1")));
    }

    #[test]
    fn test_cached_layout_matches_a_cold_render() {
        let mut st = cached_history_fixture();
        let width = Some(st.content_width);

        let _ = layout_history(
            &st.items,
            width,
            Some(0),
            Some(&st.expanded_items),
            None,
            st.theme_mode,
            &mut st.body_cache,
        );
        assert_eq!(
            st.body_cache.len(),
            1,
            "only the expanded Markdown body is worth caching"
        );
        assert_eq!(st.body_cache.render_count(), 1);

        let (cached_lines, cached_offsets, cached_blocks) = layout_history(
            &st.items,
            width,
            Some(1),
            Some(&st.expanded_items),
            None,
            st.theme_mode,
            &mut st.body_cache,
        );
        assert_eq!(
            st.body_cache.render_count(),
            1,
            "the second layout reuses the rendered body"
        );

        let (fresh_lines, fresh_offsets, fresh_blocks) =
            compute_history_lines_full(&st.items, width, Some(1), Some(&st.expanded_items), None);

        assert_eq!(
            rendered_text(&cached_lines),
            rendered_text(&fresh_lines),
            "a warm cache must render the same text as a cold one"
        );
        assert_eq!(cached_offsets, fresh_offsets);
        assert_eq!(cached_blocks.len(), fresh_blocks.len());
        assert_eq!(cached_blocks.len(), 1, "the fenced block is reported once");
    }

    #[test]
    fn test_navigation_never_reparses_a_cached_markdown_body() {
        let mut st = cached_history_fixture();
        let width = Some(st.content_width);
        let height = 20u16;

        st.scroll_to_selected_item(height, width);
        assert_eq!(
            st.body_cache.render_count(),
            1,
            "the expanded answer renders its Markdown once"
        );

        st.selected_item_index = 1;
        st.scroll_to_selected_item(height, width);
        st.selected_item_index = 0;
        st.recenter_selected_item(height, width);
        st.scroll_to_selected_item(height, width);
        st.update_selected_item_from_scroll(height, width);

        assert_eq!(
            st.body_cache.render_count(),
            1,
            "cursor moves and scrolls must serve the cached body"
        );
    }

    #[test]
    fn test_theme_change_and_resize_reset_the_markdown_cache() {
        let mut st = cached_history_fixture();
        let width = Some(st.content_width);

        st.scroll_to_selected_item(20, width);
        let renders = st.body_cache.render_count();
        assert_eq!(renders, 1);

        st.theme_mode = crate::config::ThemeMode::Dark;
        st.scroll_to_selected_item(20, width);
        let after_theme = st.body_cache.render_count();
        assert!(
            after_theme > renders,
            "a new theme mode invalidates every cached body"
        );

        st.content_width = 40;
        st.scroll_to_selected_item(20, Some(40));
        assert!(
            st.body_cache.render_count() > after_theme,
            "a resize invalidates every cached body"
        );
        assert_eq!(
            st.body_cache.len(),
            1,
            "the retained entry is re-rendered, never duplicated"
        );
    }

    fn status_item(id: u64, status: HistoryEntryStatus, summary: Option<&str>) -> UiHistoryItem {
        let mut item = history_item(id, UiHistoryKind::User, "Implement the change");
        item.summary = summary.map(str::to_string);
        item.status = status;
        item
    }

    fn render_item(item: &UiHistoryItem) -> Vec<String> {
        rendered_text(&compute_history_lines(
            std::slice::from_ref(item),
            None,
            None,
            None,
        ))
    }

    #[test]
    fn test_active_record_is_marked_in_flight() {
        let active = status_item(1, HistoryEntryStatus::Active, None);
        let rendered = render_item(&active);

        assert!(
            rendered.iter().any(|line| line.contains("[IN-FLIGHT]")),
            "a running prompt must advertise its in-flight status"
        );
        assert!(!rendered
            .iter()
            .any(|line| line.contains(active_t("history.rolled_back_hint").as_str())));
    }

    #[test]
    fn test_rolled_back_record_shows_status_reason_and_restore_hint() {
        let hint = active_t("history.rolled_back_hint").to_string();
        let item = status_item(
            2,
            HistoryEntryStatus::RolledBack,
            Some("Turn cancelled by the developer"),
        );
        let rendered = render_item(&item);

        assert!(rendered.iter().any(|line| line.contains("[ROLLED BACK]")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("Turn cancelled by the developer")));
        assert!(
            rendered.iter().any(|line| line.contains(hint.as_str())),
            "the restore hint must accompany a rolled back record"
        );
    }

    /// The undo-commit command is gone, so a System record is badged purely by
    /// its typed kind: legacy prose containing the old undo keyword must never
    /// re-derive a removed feature from entry text (Conventions §1.5).
    #[test]
    fn test_system_entry_with_undo_keyword_renders_system_badge() {
        let mut item = history_item(3, UiHistoryKind::System, "Undid commit abc1234.");
        item.summary = Some("Legacy undo record".to_string());
        item.commit_hash = Some("abc1234".to_string());
        let rendered = render_item(&item);

        assert!(rendered.iter().any(|line| line.contains("[SYSTEM]")));
        assert!(!rendered.iter().any(|line| line.contains("[UNDO]")));
    }

    /// A lifecycle status change only alters the per-frame bands, so the cached
    /// Markdown body survives it while the restore hint still appears (c-3).
    #[test]
    fn test_status_change_updates_the_body_without_reparsing_markdown() {
        let mut st = cached_history_fixture();
        let width = Some(st.content_width);
        st.scroll_to_selected_item(20, width);
        let renders = st.body_cache.render_count();

        // The restore hint is a full sentence, so the body renderer wraps it at
        // the fixture width. The comparison collapses whitespace on both sides
        // instead of assuming the phrase stays inside a single rendered line.
        let hint = active_t("history.rolled_back_hint").to_string();
        let needle = normalize_whitespace(&hint);
        assert!(!needle.is_empty(), "the restore hint is localizable text");

        for status in [HistoryEntryStatus::Active, HistoryEntryStatus::RolledBack] {
            st.items[0].status = status;
            let (lines, _, _) = layout_history(
                &st.items,
                width,
                Some(0),
                Some(&st.expanded_items),
                None,
                st.theme_mode,
                &mut st.body_cache,
            );
            let text = normalize_whitespace(&rendered_text(&lines).join("\n"));
            let expects_hint = status == HistoryEntryStatus::RolledBack;
            assert_eq!(
                text.contains(&needle),
                expects_hint,
                "the restore hint follows the typed status only"
            );
        }

        assert_eq!(
            st.body_cache.render_count(),
            renders,
            "a lifecycle badge never re-parses the Markdown"
        );
    }

    /// A copy flash is a rendering variant of the same body: it is rendered
    /// once, reused while it lasts, and restored once it expires.
    #[test]
    fn test_copy_flash_renders_its_own_body_and_expires() {
        let mut st = cached_history_fixture();
        let width = Some(st.content_width);

        let (_, _, blocks) = layout_history(
            &st.items,
            width,
            Some(0),
            Some(&st.expanded_items),
            None,
            st.theme_mode,
            &mut st.body_cache,
        );
        let Some(block) = blocks.first().cloned() else {
            panic!("the expanded answer carries one fenced code block");
        };
        let renders = st.body_cache.render_count();

        let _ = layout_history(
            &st.items,
            width,
            Some(0),
            Some(&st.expanded_items),
            Some((block.item_id, block.block_id)),
            st.theme_mode,
            &mut st.body_cache,
        );
        assert_eq!(
            st.body_cache.render_count(),
            renders + 1,
            "the flash renders the highlighted body once"
        );

        let _ = layout_history(
            &st.items,
            width,
            Some(0),
            Some(&st.expanded_items),
            Some((block.item_id, block.block_id)),
            st.theme_mode,
            &mut st.body_cache,
        );
        assert_eq!(
            st.body_cache.render_count(),
            renders + 1,
            "the highlighted body is reused while it lasts"
        );

        let _ = layout_history(
            &st.items,
            width,
            Some(0),
            Some(&st.expanded_items),
            None,
            st.theme_mode,
            &mut st.body_cache,
        );
        assert_eq!(
            st.body_cache.render_count(),
            renders + 2,
            "the plain body is restored once the flash expires"
        );
    }

    /// Screen range `(start, end, total_lines)` that `recenter_selected_item`
    /// lays out for the selected entry.
    fn selected_range(st: &mut HistoryViewState, width: Option<usize>) -> (usize, usize, usize) {
        let sel = st.selected_item_index;
        let (total_lines, offsets) = st.layout_offsets(width, Some(sel));
        let start = offsets.get(sel).copied().unwrap_or(0);
        let end = offsets.get(sel + 1).copied().unwrap_or(total_lines);
        (start, end, total_lines)
    }

    /// Expected `Center`, `Top`, and `Bottom` viewport offsets for an entry that
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

    fn recenter_history_fixture(selected: usize) -> HistoryViewState {
        HistoryViewState {
            items: (1u64..=20)
                .map(|i| history_item(i, UiHistoryKind::User, "test item content"))
                .collect(),
            selected_item_index: selected,
            content_width: 80,
            ..Default::default()
        }
    }

    #[test]
    fn test_recenter_selected_item_walks_the_center_top_bottom_cycle() {
        let height = 5u16;
        let width = Some(80usize);
        let mut st = recenter_history_fixture(8);

        let (start, end, total) = selected_range(&mut st, width);
        let [center, top, bottom] = recenter_beats(start, end, total, height);
        assert_ne!(center, top, "the center and top beats must differ");
        assert_ne!(top, bottom, "the top and bottom beats must differ");
        assert_ne!(center, bottom, "the center and bottom beats must differ");

        st.recenter_selected_item(height, width);
        assert_eq!(st.scroll, center, "the first press centers the entry");
        st.recenter_selected_item(height, width);
        assert_eq!(
            st.scroll, top,
            "the second press aligns the entry to the top"
        );
        st.recenter_selected_item(height, width);
        assert_eq!(
            st.scroll, bottom,
            "the third press aligns the entry to the bottom"
        );
        st.recenter_selected_item(height, width);
        assert_eq!(
            st.scroll, center,
            "the fourth press wraps back to the center"
        );
    }

    #[test]
    fn test_recenter_selected_item_restarts_the_cycle_on_cursor_move() {
        let height = 5u16;
        let width = Some(80usize);
        let mut st = recenter_history_fixture(8);

        // Walk to the top beat of the first entry, so a continuation would
        // land on the bottom beat instead of starting over.
        st.recenter_selected_item(height, width);
        st.recenter_selected_item(height, width);

        st.selected_item_index = 12;
        let (start, end, total) = selected_range(&mut st, width);
        let [center, top, bottom] = recenter_beats(start, end, total, height);
        assert_ne!(center, bottom, "the first and third beats must differ");

        st.recenter_selected_item(height, width);
        assert_eq!(
            st.scroll, center,
            "a new cursor position starts over on the center beat"
        );
        st.recenter_selected_item(height, width);
        assert_eq!(st.scroll, top, "the new entry keeps its own cycle");
        st.recenter_selected_item(height, width);
        assert_eq!(st.scroll, bottom);
    }

    /// The recenter command is declared on both the abstract primary chord and
    /// the raw `Ctrl+L` chord, and no other binding claims the same key event
    /// under either modifier configuration (c-2).
    #[test]
    fn test_recenter_chord_is_declared_and_unambiguous() {
        use super::input::HistoryAction;
        use crate::config::{InputConfig, MetaModifier, PrimaryModifier};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        // The abstract `C-l` chord and the raw `Ctrl+L` chord deliberately name
        // the same intent, so under the default primary assignment both of them
        // match a single key event. The resolved intents are deduplicated, since
        // the invariant under test is which action the key reaches, not how many
        // spellings declare it (c-2).
        fn claims(
            input: &InputConfig,
            code: KeyCode,
            modifiers: KeyModifiers,
        ) -> Vec<HistoryAction> {
            let mut actions = Vec::new();
            for spec in HISTORY_COMMAND_MAP.specs() {
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
            vec![HistoryAction::Recenter],
            "C-l must recenter under the default primary modifier"
        );
        assert_eq!(
            claims(&alt_primary, KeyCode::Char('l'), KeyModifiers::ALT),
            vec![HistoryAction::Recenter],
            "the abstract C-l chord must follow a remapped primary modifier"
        );
        assert_eq!(
            claims(&alt_primary, KeyCode::Char('l'), KeyModifiers::CONTROL),
            vec![HistoryAction::Recenter],
            "a raw Ctrl+L must still recenter when the primary modifier is Alt"
        );
    }

    #[test]
    fn test_clamp_u16_saturates() {
        assert_eq!(clamp_u16(10), 10);
        assert_eq!(clamp_u16(70_000), u16::MAX);
    }

    #[test]
    fn test_apply_compaction_removes_head_and_anchors_auto_scroll() {
        let mut st = HistoryViewState {
            content_width: 40,
            auto_scroll: true,
            scroll: 0,
            items: vec![
                history_item(1, UiHistoryKind::User, "q1"),
                assistant_item(2, "a1"),
                history_item(3, UiHistoryKind::User, "q2"),
                assistant_item(4, "a2"),
                history_item(5, UiHistoryKind::User, "q3"),
            ],
            // The expanded answers at indices 1 and 3 are what the shared cache
            // retains across the structural replacement.
            total_count: 5,
            has_more: true,
            ..Default::default()
        };
        st.expanded_items.insert(2);
        st.expanded_items.insert(4);

        // Populate the Markdown cache so the structural reset is observable.
        let width = st.content_width;
        st.layout_offsets(Some(width), None);
        assert_eq!(
            st.body_cache.len(),
            2,
            "both expanded answers keep a rendered body"
        );

        st.apply_compaction(
            history_item(3, UiHistoryKind::System, "Session compacted"),
            &[4, 5],
            3,
            256,
            5,
        );

        let ids: Vec<u64> = st.items.iter().map(|it| it.id).collect();
        assert_eq!(ids, vec![3, 4, 5], "summary leads the monotonic tail");
        assert_eq!(st.items[0].kind, UiHistoryKind::System);
        assert_eq!(st.total_count, 3);
        assert_eq!(st.estimated_tokens, 256);
        assert!(
            !st.expanded_items.contains(&2),
            "head fold state is dropped"
        );
        assert!(st.expanded_items.contains(&4), "tail fold state survives");
        assert_eq!(
            st.body_cache.len(),
            1,
            "the head body is dropped and only the retained answer is cached"
        );
        assert_eq!(st.selected_item_index, st.items.len() - 1);

        let expected_lines = compute_history_lines(
            &st.items,
            Some(st.content_width),
            Some(st.selected_item_index),
            Some(&st.expanded_items),
        )
        .len();
        assert_eq!(st.rendered_lines_count, expected_lines);
        assert_eq!(st.scroll, clamp_u16(expected_lines).saturating_sub(5));
    }

    #[test]
    fn test_apply_compaction_without_auto_scroll_preserves_position() {
        let mut st = HistoryViewState {
            content_width: 40,
            auto_scroll: false,
            scroll: 0,
            selected_item_index: 1,
            items: (1u64..=6)
                .map(|i| history_item(i, UiHistoryKind::User, "line"))
                .collect(),
            total_count: 6,
            ..Default::default()
        };

        st.apply_compaction(
            history_item(3, UiHistoryKind::System, "Compacted"),
            &[4, 5, 6],
            4,
            96,
            5,
        );

        let ids: Vec<u64> = st.items.iter().map(|it| it.id).collect();
        assert_eq!(ids, vec![3, 4, 5, 6]);
        assert_eq!(st.selected_item_index, 1, "in-range selection is preserved");
        assert_eq!(st.scroll, 0, "in-range scroll is preserved");
    }

    #[test]
    fn test_apply_compaction_without_auto_scroll_clamps_out_of_range() {
        let mut st = HistoryViewState {
            content_width: 40,
            auto_scroll: false,
            scroll: u16::MAX,
            selected_item_index: 99,
            items: (1u64..=6)
                .map(|i| history_item(i, UiHistoryKind::User, "line"))
                .collect(),
            total_count: 6,
            ..Default::default()
        };

        st.apply_compaction(
            history_item(3, UiHistoryKind::System, "Compacted"),
            &[4, 5, 6],
            4,
            96,
            5,
        );

        assert_eq!(st.selected_item_index, st.items.len() - 1);
        assert_eq!(
            st.scroll,
            clamp_u16(st.rendered_lines_count).saturating_sub(5)
        );
    }

    #[test]
    fn test_apply_compaction_is_repeatable() {
        let mut st = HistoryViewState {
            content_width: 40,
            auto_scroll: true,
            items: (1u64..=6)
                .map(|i| history_item(i, UiHistoryKind::User, "line"))
                .collect(),
            total_count: 6,
            ..Default::default()
        };

        st.apply_compaction(
            history_item(3, UiHistoryKind::System, "first compaction"),
            &[4, 5, 6],
            4,
            128,
            5,
        );
        assert_eq!(
            st.items.iter().map(|it| it.id).collect::<Vec<_>>(),
            vec![3, 4, 5, 6]
        );

        st.apply_compaction(
            history_item(5, UiHistoryKind::System, "second compaction"),
            &[6],
            2,
            64,
            5,
        );
        assert_eq!(
            st.items.iter().map(|it| it.id).collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert_eq!(st.total_count, 2);
        assert_eq!(st.estimated_tokens, 64);
        assert_eq!(st.selected_item_index, 1);
    }
}
