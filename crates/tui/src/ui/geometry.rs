use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Vertical window of a scrolled body, as a half-open visual-line range.
///
/// A body shows `visible_height` rows starting at `scroll`, so renderers and
/// pointer hit-testing derive their rows from this one span instead of each
/// recomputing the arithmetic (c-1).
fn visible_lines(scroll: u16, visible_height: u16) -> (usize, usize) {
    let start = usize::from(scroll);
    (start, start + usize::from(visible_height))
}

/// Screen row of a visual line inside a scrolled body.
///
/// `None` means the line sits outside the window the body actually shows, so no
/// clickable region may be declared for it.
fn visible_row(content: Rect, visual_line: usize, scroll: u16, visible_height: u16) -> Option<u16> {
    let (start, end) = visible_lines(scroll, visible_height);
    if visual_line < start || visual_line >= end {
        return None;
    }
    let offset = u16::try_from(visual_line - start).ok()?;
    let row = content.y.checked_add(offset)?;
    (row < content.bottom()).then_some(row)
}

/// Screen rectangle of the clickable copy button drawn on a code block header.
///
/// `visual_line` is the header row in the body's visual coordinate space and
/// `line_prefix_cols` is the horizontal prefix the body renderer prepends to
/// every line (zero in Develop, the History line prefix in History). The button
/// span comes from [`crate::markdown::copy_button_span`], the same source the
/// renderer paints from, so a drawn button and its hit region cannot drift
/// apart (c-1).
pub fn code_copy_button_rect(
    content: Rect,
    line_prefix_cols: u16,
    lang: &str,
    visual_line: usize,
    scroll: u16,
    visible_height: u16,
) -> Option<Rect> {
    let row = visible_row(content, visual_line, scroll, visible_height)?;
    let (start, end) = crate::markdown::copy_button_span(lang);
    let x = content
        .x
        .checked_add(line_prefix_cols)?
        .checked_add(start)?;
    let width = end.saturating_sub(start).saturating_add(1);
    let rect = Rect::new(x, row, width, 1);
    (rect.right() <= content.right()).then_some(rect)
}

/// Screen rectangle covering the rows a code block body still shows.
///
/// The block is clipped to the visible window, so a block that only partly fits
/// the viewport never registers a region over a row it does not occupy.
pub fn code_block_body_rect(
    content: Rect,
    visual_start: usize,
    visual_end: usize,
    scroll: u16,
    visible_height: u16,
) -> Option<Rect> {
    let (window_start, window_end) = visible_lines(scroll, visible_height);
    let first = visual_start.max(window_start);
    let last = visual_end.min(window_end.checked_sub(1)?);
    if first > last || content.width == 0 {
        return None;
    }
    let y = content
        .y
        .checked_add(u16::try_from(first - window_start).ok()?)?;
    let height = u16::try_from(last - first + 1).ok()?;
    (y.saturating_add(height) <= content.bottom()).then_some(Rect::new(
        content.x,
        y,
        content.width,
        height,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTENT: Rect = Rect::new(4, 10, 80, 20);

    #[test]
    fn copy_button_sits_on_the_header_row_of_a_visible_block() {
        let rect = code_copy_button_rect(CONTENT, 0, "rust", 3, 2, 20).expect("visible");
        assert_eq!(rect.y, 11, "visual row 3 of the body is row 11 on screen");
        assert_eq!(rect.x, CONTENT.x + 7, "the header starts with `  rust `");
        assert_eq!(rect.width, 10, "the `[ Copy ]` span plus its trailing gap");
        assert_eq!(rect.height, 1);
    }

    #[test]
    fn history_line_prefix_shifts_the_button() {
        let plain = code_copy_button_rect(CONTENT, 0, "rust", 3, 2, 20).expect("visible");
        let prefixed = code_copy_button_rect(CONTENT, 2, "rust", 3, 2, 20).expect("visible");
        assert_eq!(prefixed.x, plain.x + 2);
        assert_eq!(prefixed.width, plain.width);
    }

    #[test]
    fn a_header_outside_the_scroll_window_declares_no_button() {
        assert!(code_copy_button_rect(CONTENT, 0, "rust", 1, 2, 20).is_none());
        assert!(code_copy_button_rect(CONTENT, 0, "rust", 22, 2, 20).is_none());
        assert_eq!(
            code_copy_button_rect(CONTENT, 0, "rust", 21, 2, 20).map(|rect| rect.y),
            Some(29)
        );
    }

    #[test]
    fn block_body_is_clipped_to_the_visible_window() {
        let partial = code_block_body_rect(CONTENT, 0, 5, 2, 20).expect("partly visible");
        assert_eq!(partial.y, CONTENT.y, "the block starts above the window");
        assert_eq!(partial.height, 4, "rows 2..=5 stay visible");
        assert_eq!(partial.width, CONTENT.width);

        let whole = code_block_body_rect(CONTENT, 4, 6, 2, 20).expect("visible");
        assert_eq!(whole.y, CONTENT.y + 2);
        assert_eq!(whole.height, 3);

        assert!(code_block_body_rect(CONTENT, 0, 1, 2, 20).is_none());
        assert!(code_block_body_rect(CONTENT, 0, 5, 2, 0).is_none());
    }
}
