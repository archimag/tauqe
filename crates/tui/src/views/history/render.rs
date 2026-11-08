use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::app::AppState;
use crate::i18n::active_t;
use crate::ui::geometry;
use crate::ui::UiAction;

use super::layout_history;

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
    let theme_mode = state.history_view.theme_mode;
    let (history_lines, _, code_blocks) = layout_history(
        &state.history_view.items,
        Some(content_width),
        Some(state.history_view.selected_item_index),
        Some(&state.history_view.expanded_items),
        flash_target,
        theme_mode,
        &mut state.history_view.body_cache,
    );
    let total_lines = history_lines.len();
    state.history_view.rendered_lines_count = total_lines;
    state.history_view.code_blocks = code_blocks;

    // The view owns the saturating bound, and the bound is expressed over the
    // document length alone: a chronicle longer than `u16::MAX` visual lines
    // keeps the largest representable offset instead of one cut short by a
    // saturated line count (c-2). The renderer only bounds the offset;
    // positioning the window belongs to the reading position, because in
    // History the cursor follows the scroll offset and never the other way
    // round (c-render-scroll-decoupling).
    let max_scroll = state.history_view.max_scroll(content_height);
    if state.history_view.auto_scroll || state.history_view.scroll > max_scroll {
        state.history_view.scroll = max_scroll;
    }

    // History owns its Markdown geometry: each visible block declares its body
    // and then the copy button of its header, because a later registration wins
    // over the surface it covers. The dispatcher resolves a click through the
    // shared registry instead of re-deriving the line prefix and the scroll
    // offset (c-1, c-2).
    let scroll = state.history_view.scroll;
    let prefix_cols = u16::try_from(super::LINE_PREFIX.len()).unwrap_or(0);
    let mut block_regions: Vec<(Rect, UiAction)> = Vec::new();
    for block in &state.history_view.code_blocks {
        if let Some(rect) = geometry::code_block_body_rect(
            inner,
            block.visual_start_line,
            block.visual_end_line,
            scroll,
            content_height,
        ) {
            block_regions.push((rect, UiAction::CodeBlockBody));
        }
        if let Some(rect) = geometry::code_copy_button_rect(
            inner,
            prefix_cols,
            &block.lang,
            block.visual_start_line,
            scroll,
            content_height,
        ) {
            block_regions.push((
                rect,
                UiAction::CopyCodeBlock {
                    item: Some(block.item_id),
                    block: block.block_id,
                },
            ));
        }
    }
    for (rect, action) in block_regions {
        state.hit_regions.register(rect, action);
    }

    // `Paragraph` scrolls through a `u16` offset, so the widget itself can
    // neither reach a row past line 65 535 nor add a saturated offset to the
    // viewport height without overflowing its own arithmetic. The visible
    // window is therefore sliced in `usize` and handed over already positioned,
    // which keeps a legitimate deep offset renderable instead of jumping to a
    // wrapped row (c-2).
    let window_start = usize::from(scroll);
    let window = history_lines
        .into_iter()
        .skip(window_start)
        .take(usize::from(content_height))
        .collect::<Vec<_>>();
    let history_paragraph = Paragraph::new(window).block(hist_block);
    frame.render_widget(history_paragraph, main_area);

    let sel_idx = if state.history_view.items.is_empty() {
        0
    } else {
        state.history_view.selected_item_index + 1
    };
    let total_shown = state.history_view.items.len();

    let engine = crate::theme::engine::current_engine();
    let palette = engine.palette();
    let info_line = Line::from(vec![
        Span::styled(
            format!(
                " {} ",
                active_t("history.item_counter")
                    .replacen("{}", &sel_idx.to_string(), 1)
                    .replacen("{}", &total_shown.to_string(), 1)
            ),
            engine.fg(palette.text_primary).bold(),
        ),
        Span::raw(" | "),
        Span::styled(
            if state.history_view.auto_scroll {
                active_t("history.auto_scroll_on")
            } else {
                active_t("history.auto_scroll_off")
            },
            if state.history_view.auto_scroll {
                engine.fg(palette.status_success).bold()
            } else {
                engine.fg(palette.text_muted)
            },
        ),
        Span::raw(" | "),
        Span::styled(
            format!("~{} tokens", state.history_view.get_estimated_tokens()),
            engine.fg(palette.status_info),
        ),
    ]);
    let prompt_widget =
        Paragraph::new(info_line).block(Block::default().padding(Padding::horizontal(1)));
    frame.render_widget(prompt_widget, info_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::history::HISTORY_RETAINED_ITEMS;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tauqe_protocol::{HistoryEntryStatus, UiHistoryItem, UiHistoryKind};

    /// Body lines one tall record carries below its header.
    const TALL_BODY_LINES: usize = 350;

    fn render_once(terminal: &mut Terminal<TestBackend>, state: &mut AppState) {
        terminal
            .draw(|f| {
                let area = f.area();
                let main_area = Rect::new(0, 0, area.width, area.height.saturating_sub(1));
                let info_area = Rect::new(0, area.height.saturating_sub(1), area.width, 1);
                render_history_view(f, state, main_area, info_area);
            })
            .unwrap();
    }

    fn record(id: u64, kind: UiHistoryKind, text: String) -> UiHistoryItem {
        UiHistoryItem {
            id,
            kind,
            text,
            summary: None,
            commit_hash: None,
            files: Vec::new(),
            status: HistoryEntryStatus::Committed,
        }
    }

    /// One expanded prompt whose body alone is taller than a screenful.
    fn tall_record(id: u64) -> UiHistoryItem {
        let text = (0..TALL_BODY_LINES)
            .map(|line| format!("Invariant line {line}."))
            .collect::<Vec<_>>()
            .join("\n");
        record(id, UiHistoryKind::User, text)
    }

    /// A redraw must never pull a freely scrolled viewport back to the cursor:
    /// the renderer bounds the offset by the document length only, and the
    /// window it draws is sliced at that offset instead of being handed to the
    /// widget's own `u16` scroll (c-2, c-render-scroll-decoupling).
    #[test]
    fn render_history_view_preserves_free_scroll_across_frames() {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let mut state = crate::app::test_state();
        state.history_view.items = (1..=40)
            .map(|id| record(id, UiHistoryKind::User, "record".to_string()))
            .collect();
        state.history_view.selected_item_index = 0;
        state.history_view.auto_scroll = false;
        state.history_view.scroll = 20;

        render_once(&mut terminal, &mut state);

        let height = state.last_model_height;
        assert!(
            state.history_view.rendered_lines_count > 20 + usize::from(height),
            "the chronicle must extend past the free scroll offset"
        );
        assert_eq!(
            state.history_view.scroll, 20,
            "a redraw must not snap the viewport back to the selected row"
        );

        state.history_view.scroll = u16::MAX;
        render_once(&mut terminal, &mut state);
        assert_eq!(
            state.history_view.scroll,
            state.history_view.max_scroll(height),
            "the renderer clamps the viewport by the document length only"
        );
    }

    /// The retained window alone can exceed the `u16` viewport unit, so a deep
    /// offset past line 65 535 must stay renderable: the window is sliced in
    /// `usize` and the bound saturates at the largest representable offset
    /// (c-2, c-history-window).
    #[test]
    fn render_history_view_keeps_a_deep_scroll_past_the_u16_viewport_unit() {
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let mut state = crate::app::test_state();
        state.history_view.items = (1..=HISTORY_RETAINED_ITEMS as u64)
            .map(tall_record)
            .collect();
        state.history_view.expanded_items = state
            .history_view
            .items
            .iter()
            .map(|item| item.id)
            .collect();
        state.history_view.selected_item_index = 0;
        state.history_view.auto_scroll = false;
        state.history_view.scroll = 60_000;

        render_once(&mut terminal, &mut state);

        assert!(
            state.history_view.rendered_lines_count > usize::from(u16::MAX),
            "the chronicle must exceed the u16 viewport unit"
        );
        assert_eq!(
            state.history_view.scroll, 60_000,
            "a redraw must keep a legitimate offset past line 65 535"
        );

        state.history_view.scroll = u16::MAX;
        render_once(&mut terminal, &mut state);
        assert_eq!(
            state.history_view.scroll,
            u16::MAX,
            "the bound saturates at the largest representable offset"
        );
    }
}
