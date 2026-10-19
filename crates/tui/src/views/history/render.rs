use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::app::AppState;
use crate::i18n::active_t;

use super::{clamp_u16, layout_history};

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
    state.history_view.content_rect = (inner.x, inner.y, inner.width, content_height);

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
