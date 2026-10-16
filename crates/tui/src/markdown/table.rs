//! Table column layout, cell wrapping and rendering.
//!
//! Extracted from the Markdown renderer so the pulldown-cmark event state
//! machine stays focused (Conventions.md §3.5).

use pulldown_cmark::Alignment;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::{push_empty_line, push_styled_line, MarkdownTheme, TableCellData, TableState};

/// Splits the styled spans of a single table cell into visual lines whose
/// width never exceeds `target_width`.
///
/// Wrapping happens on word boundaries so that words stay intact. Words
/// wider than `target_width` are split on Unicode scalar boundaries to
/// avoid corrupting UTF-8. Active span styles are preserved on every
/// produced line. Leading whitespace of a wrapped line and trailing
/// whitespace at a wrap point are dropped.
fn wrap_cell_spans(spans: &[Span<'static>], target_width: usize) -> Vec<Vec<Span<'static>>> {
    if target_width == 0 {
        return vec![Vec::new()];
    }

    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut line_width = 0usize;
    let mut pending_ws: Vec<Span<'static>> = Vec::new();
    let mut pending_ws_width = 0usize;

    for span in spans {
        let style = span.style;
        for (token, is_ws) in split_cell_tokens(&span.content) {
            let token_width = Span::raw(token.as_str()).width();

            if is_ws {
                pending_ws.push(Span::styled(token, style));
                pending_ws_width += token_width;
                continue;
            }

            if !line.is_empty() {
                if line_width + pending_ws_width + token_width <= target_width {
                    line.append(&mut pending_ws);
                    line_width += pending_ws_width;
                    pending_ws_width = 0;
                    line.push(Span::styled(token, style));
                    line_width += token_width;
                    continue;
                }
                lines.push(std::mem::take(&mut line));
                line_width = 0;
            }
            pending_ws.clear();
            pending_ws_width = 0;

            if token_width <= target_width {
                line.push(Span::styled(token, style));
                line_width = token_width;
            } else {
                split_oversized_word(
                    &token,
                    style,
                    target_width,
                    &mut lines,
                    &mut line,
                    &mut line_width,
                );
            }
        }
    }

    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// Splits cell content into alternating word and whitespace tokens,
/// preserving the original character order.
fn split_cell_tokens(content: &str) -> Vec<(String, bool)> {
    let mut tokens: Vec<(String, bool)> = Vec::new();
    let mut current = String::new();
    let mut current_is_ws: Option<bool> = None;

    for ch in content.chars() {
        let is_ws = ch.is_whitespace();
        match current_is_ws {
            Some(prev) if prev == is_ws => current.push(ch),
            Some(prev) => {
                tokens.push((std::mem::take(&mut current), prev));
                current.push(ch);
                current_is_ws = Some(is_ws);
            }
            None => {
                current.push(ch);
                current_is_ws = Some(is_ws);
            }
        }
    }

    if let Some(is_ws) = current_is_ws {
        tokens.push((current, is_ws));
    }

    tokens
}

/// Emits a single word that is wider than `target_width`, flushing full
/// pieces as whole lines and leaving the final remainder on the active
/// line.
fn split_oversized_word(
    word: &str,
    style: Style,
    target_width: usize,
    lines: &mut Vec<Vec<Span<'static>>>,
    line: &mut Vec<Span<'static>>,
    line_width: &mut usize,
) {
    let mut piece = String::new();
    let mut piece_width = 0usize;

    for ch in word.chars() {
        let ch_width = Span::raw(ch.to_string()).width();
        if piece_width + ch_width > target_width && !piece.is_empty() {
            lines.push(vec![Span::styled(std::mem::take(&mut piece), style)]);
            piece_width = 0;
        }
        piece.push(ch);
        piece_width += ch_width;
    }

    if !piece.is_empty() {
        line.push(Span::styled(piece, style));
        *line_width = piece_width;
    }
}

/// Base width reserved for a column regardless of its content.
const TABLE_MIN_COL_WIDTH: usize = 8;
/// Default total table width applied when the theme sets no explicit limit.
const DEFAULT_TABLE_MAX_WIDTH: usize = 120;

/// Computes the rendered width of every table column.
///
/// The natural width of a column comes from its widest cell plus one
/// space of padding on each side. The minimum width keeps the longest
/// unbreakable word intact so that wrapping never splits a word that could
/// otherwise fit. When the whole table would exceed the effective `max_width`,
/// columns are shrunk proportionally down to their minimum widths; when space
/// permits, columns with wrapping content expand up to the available width.
fn compute_col_widths(theme: &MarkdownTheme, ts: &TableState, col_count: usize) -> Vec<usize> {
    let mut natural = vec![TABLE_MIN_COL_WIDTH; col_count];
    let mut longest_word = vec![1usize; col_count];

    for (i, cell) in ts.headers.iter().enumerate() {
        if i >= col_count {
            break;
        }
        natural[i] = natural[i].max(cell.text_len + 2);
        longest_word[i] = longest_word[i].max(cell_longest_word(cell));
    }
    for row in &ts.rows {
        for (i, cell) in row.iter().enumerate() {
            if i >= col_count {
                break;
            }
            natural[i] = natural[i].max(cell.text_len + 2);
            longest_word[i] = longest_word[i].max(cell_longest_word(cell));
        }
    }

    let max_width = theme.max_width.unwrap_or(DEFAULT_TABLE_MAX_WIDTH);
    let available = max_width.saturating_sub(col_count + 1);

    for w in natural.iter_mut() {
        *w = (*w).min(available.max(TABLE_MIN_COL_WIDTH));
    }

    let minimum: Vec<usize> = (0..col_count)
        .map(|i| (longest_word[i] + 2).min(natural[i]).max(1))
        .collect();

    fit_col_widths(&mut natural, &minimum, max_width);

    natural
}

/// Returns the display width of the longest whitespace-delimited word in
/// a table cell, joining the styled spans first so that a word split
/// across several spans is measured as a whole.
fn cell_longest_word(cell: &TableCellData) -> usize {
    let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
    text.split_whitespace()
        .map(|word| Span::raw(word).width())
        .max()
        .unwrap_or(0)
}

/// Shrinks `widths` in place so the rendered table fits within `max_width`.
///
/// The rendered table inserts one border character before, between, and
/// after the columns (`col_count + 1` characters in total). Columns are
/// never reduced below their `minimum` width; if even the minimum widths
/// overflow, the table keeps those minimums so that intra-cell wrapping
/// can split oversized words later.
fn fit_col_widths(widths: &mut [usize], minimum: &[usize], max_width: usize) {
    let col_count = widths.len();
    if col_count == 0 {
        return;
    }

    let available = max_width.saturating_sub(col_count + 1);

    let total: usize = widths.iter().sum();
    if total <= available {
        return;
    }

    let min_total: usize = minimum.iter().sum();
    if min_total >= available {
        for (w, m) in widths.iter_mut().zip(minimum.iter()) {
            *w = *m;
        }
        return;
    }

    let excess_total: usize = widths
        .iter()
        .zip(minimum.iter())
        .map(|(w, m)| w.saturating_sub(*m))
        .sum();
    if excess_total == 0 {
        return;
    }

    let distributable = available - min_total;
    let mut assigned = 0usize;
    for (w, m) in widths.iter_mut().zip(minimum.iter()) {
        let excess = w.saturating_sub(*m);
        let share = excess * distributable / excess_total;
        *w = m + share;
        assigned += share;
    }

    let mut remainder = distributable.saturating_sub(assigned);
    let mut i = 0usize;
    while remainder > 0 {
        widths[i % col_count] += 1;
        remainder -= 1;
        i += 1;
    }
}

/// Renders one logical table row (header or data) as one or more physical
/// terminal lines.
///
/// Each cell is wrapped to the inner width of its column (`width - 2`,
/// reserving one space of padding on each side). The logical row expands to
/// `H` physical lines, where `H` is the tallest wrapped cell in the row.
/// Shorter cells are padded with blank content so that every vertical border
/// stays on the same terminal column.
fn push_table_row(
    theme: &MarkdownTheme,
    cells: &[TableCellData],
    col_widths: &[usize],
    alignments: &[Alignment],
    is_head: bool,
    out_lines: &mut Vec<Line<'static>>,
) {
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = col_widths
        .iter()
        .enumerate()
        .map(|(i, &w)| match cells.get(i) {
            Some(cell) => wrap_cell_spans(&cell.spans, w.saturating_sub(2)),
            None => vec![Vec::new()],
        })
        .collect();

    let height = wrapped
        .iter()
        .map(|lines| lines.len())
        .max()
        .unwrap_or(1)
        .max(1);

    for line_idx in 0..height {
        let mut row_line: Vec<Span<'static>> = Vec::new();
        row_line.push(Span::styled("│", theme.table_border));

        for (i, &w) in col_widths.iter().enumerate() {
            let align = alignments.get(i).copied().unwrap_or(Alignment::None);
            let (left_fixed, right_fixed, content_width) = if w >= 2 {
                (1usize, 1usize, w - 2)
            } else {
                (0usize, 0usize, w)
            };

            let empty: Vec<Span<'static>> = Vec::new();
            let line_spans = wrapped
                .get(i)
                .and_then(|lines| lines.get(line_idx))
                .unwrap_or(&empty);

            let line_width: usize = line_spans.iter().map(|s| s.width()).sum();
            let pad = content_width.saturating_sub(line_width);
            let (left_pad, right_pad) = match align {
                Alignment::Right => (pad, 0),
                Alignment::Center => (pad / 2, pad - (pad / 2)),
                _ => (0, pad),
            };

            if left_fixed > 0 {
                row_line.push(Span::raw(" ".repeat(left_fixed)));
            }
            if left_pad > 0 {
                row_line.push(Span::raw(" ".repeat(left_pad)));
            }
            for span in line_spans {
                if is_head {
                    row_line.push(Span::styled(span.content.clone(), theme.table_head));
                } else {
                    row_line.push(span.clone());
                }
            }
            if right_pad > 0 {
                row_line.push(Span::raw(" ".repeat(right_pad)));
            }
            if right_fixed > 0 {
                row_line.push(Span::raw(" ".repeat(right_fixed)));
            }
            row_line.push(Span::styled("│", theme.table_border));
        }

        push_styled_line(theme, out_lines, row_line);
    }
}

pub(super) fn render_table(
    theme: &MarkdownTheme,
    ts: TableState,
    out_lines: &mut Vec<Line<'static>>,
    is_streaming: bool,
) {
    let col_count = ts
        .headers
        .len()
        .max(ts.rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if col_count == 0 {
        return;
    }

    let col_widths = compute_col_widths(theme, &ts, col_count);

    let mut top = String::from("┌");
    for (i, &w) in col_widths.iter().enumerate() {
        top.push_str(&"─".repeat(w));
        if i + 1 < col_count {
            top.push('┬');
        } else {
            top.push('┐');
        }
    }
    push_styled_line(
        theme,
        out_lines,
        vec![Span::styled(top, theme.table_border)],
    );

    let mut row_sep = String::from("├");
    for (i, &w) in col_widths.iter().enumerate() {
        row_sep.push_str(&"─".repeat(w));
        if i + 1 < col_count {
            row_sep.push('┼');
        } else {
            row_sep.push('┤');
        }
    }

    if !ts.headers.is_empty() {
        push_table_row(
            theme,
            &ts.headers,
            &col_widths,
            &ts.alignments,
            true,
            out_lines,
        );

        push_styled_line(
            theme,
            out_lines,
            vec![Span::styled(row_sep.clone(), theme.table_border)],
        );
    }

    for (idx, row) in ts.rows.iter().enumerate() {
        if idx > 0 {
            push_styled_line(
                theme,
                out_lines,
                vec![Span::styled(row_sep.clone(), theme.table_border)],
            );
        }
        push_table_row(theme, row, &col_widths, &ts.alignments, false, out_lines);
    }

    if is_streaming {
        let mut bot = String::from("├");
        for (i, &w) in col_widths.iter().enumerate() {
            bot.push_str(&"─".repeat(w));
            if i + 1 < col_count {
                bot.push('┴');
            } else {
                bot.push_str("┤ (streaming)");
            }
        }
        push_styled_line(
            theme,
            out_lines,
            vec![Span::styled(bot, theme.table_border)],
        );
    } else {
        let mut bot = String::from("└");
        for (i, &w) in col_widths.iter().enumerate() {
            bot.push_str(&"─".repeat(w));
            if i + 1 < col_count {
                bot.push('┴');
            } else {
                bot.push('┘');
            }
        }
        push_styled_line(
            theme,
            out_lines,
            vec![Span::styled(bot, theme.table_border)],
        );
        if !theme.compact {
            push_empty_line(theme, out_lines);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::{render_markdown, MarkdownTheme};
    use ratatui::style::Modifier;

    #[test]
    fn wrap_cell_spans_respects_target_width() {
        let spans = vec![Span::raw("alpha beta gamma delta")];
        let lines = wrap_cell_spans(&spans, 11);
        assert!(lines.len() >= 2);
        for line in &lines {
            let width: usize = line.iter().map(|s| s.width()).sum();
            assert!(width <= 11, "line width {} exceeds target", width);
        }
        let joined: String = lines
            .iter()
            .map(|l| l.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join(" ");
        for word in ["alpha", "beta", "gamma", "delta"] {
            assert!(joined.contains(word));
        }
    }

    #[test]
    fn wrap_cell_spans_preserves_styles_across_lines() {
        let spans = vec![
            Span::styled("alpha", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled("beta", Style::default().add_modifier(Modifier::ITALIC)),
        ];
        let lines = wrap_cell_spans(&spans, 5);
        for line in &lines {
            for span in line {
                assert!(span.width() <= 5);
            }
        }
        assert!(lines
            .iter()
            .flatten()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD)));
        assert!(lines
            .iter()
            .flatten()
            .any(|s| s.style.add_modifier.contains(Modifier::ITALIC)));
    }

    #[test]
    fn wrap_cell_spans_splits_words_longer_than_target() {
        let spans = vec![Span::raw("abcdefghij")];
        let lines = wrap_cell_spans(&spans, 4);
        assert_eq!(lines.len(), 3);
        for line in &lines {
            let width: usize = line.iter().map(|s| s.width()).sum();
            assert!(width <= 4);
        }
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(joined, "abcdefghij");
    }

    #[test]
    fn wrap_cell_spans_handles_unicode_widths() {
        let spans = vec![Span::raw("Привет мир")];
        let lines = wrap_cell_spans(&spans, 8);
        for line in &lines {
            let width: usize = line.iter().map(|s| s.width()).sum();
            assert!(width <= 8);
        }
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("Привет"));
        assert!(joined.contains("мир"));
    }

    #[test]
    fn wrap_cell_spans_returns_single_line_for_empty_input() {
        let lines = wrap_cell_spans(&[], 10);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].is_empty());
    }

    #[test]
    fn fit_col_widths_keeps_columns_when_they_fit() {
        let mut widths = vec![10, 10];
        let minimum = vec![3, 3];
        fit_col_widths(&mut widths, &minimum, 100);
        assert_eq!(widths, vec![10, 10]);
    }

    #[test]
    fn fit_col_widths_shrinks_proportionally_within_limits() {
        let mut widths = vec![20, 20, 20];
        let minimum = vec![5, 5, 5];
        fit_col_widths(&mut widths, &minimum, 40);
        let total: usize = widths.iter().sum();
        assert!(total + widths.len() < 40, "total {} too wide", total);
        assert!(widths.iter().all(|&w| w >= 5), "columns shrank below minimum");
    }

    #[test]
    fn fit_col_widths_respects_minimum_when_space_is_scarce() {
        let mut widths = vec![30, 30];
        let minimum = vec![10, 10];
        fit_col_widths(&mut widths, &minimum, 15);
        assert_eq!(widths, vec![10, 10]);
    }

    #[test]
    fn table_respects_theme_max_width() {
        let md = "| Header one | Header two |\n|---|---|\n| a very long cell value | another long value |\n";
        let mut theme = MarkdownTheme::answer();
        theme.max_width = Some(40);
        let lines = render_markdown(md, &theme);
        let border = lines
            .iter()
            .find_map(|line| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                text.starts_with('┌').then_some(text)
            })
            .expect("table top border");
        assert!(
            border.chars().count() <= 40,
            "border width {} exceeds max_width",
            border.chars().count()
        );
    }

    #[test]
    fn table_renders_wrapped_cells_on_aligned_lines() {
        let md = "| left | right |\n|---|---|\n| alpha beta gamma delta | x |\n";
        let mut theme = MarkdownTheme::answer();
        theme.max_width = Some(28);
        let lines = render_markdown(md, &theme);

        let row_lines: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .filter(|text| text.starts_with('│'))
            .collect();

        assert!(
            row_lines.len() >= 3,
            "expected header plus wrapped data lines, got {:?}",
            row_lines
        );

        let width = row_lines[0].chars().count();
        assert!(
            row_lines.iter().all(|l| l.chars().count() == width),
            "all rendered row lines must share the same width: {:?}",
            row_lines
        );
    }

    /// Flattens a rendered line back into its raw text.
    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    /// Collects the character positions of every vertical border (`│`) on a
    /// rendered line. The list is empty when the line carries no border.
    fn vertical_bar_positions(line: &Line<'static>) -> Vec<usize> {
        line_text(line)
            .chars()
            .enumerate()
            .filter(|(_, ch)| *ch == '│')
            .map(|(index, _)| index)
            .collect()
    }

    #[test]
    fn table_long_cells_keep_vertical_borders_aligned() {
        let md = "| Header A | Header B |\n|---|---|\n| a very long cell that will wrap over several lines | short |\n";
        let mut theme = MarkdownTheme::answer();
        theme.max_width = Some(30);
        let lines = render_markdown(md, &theme);

        let row_positions: Vec<Vec<usize>> = lines
            .iter()
            .map(vertical_bar_positions)
            .filter(|positions| !positions.is_empty())
            .collect();

        assert!(
            row_positions.len() >= 3,
            "the long cell must wrap into multiple physical rows, got {:?}",
            row_positions
        );

        let expected = &row_positions[0];
        assert_eq!(
            expected.len(),
            3,
            "each row must render two columns separated by three borders"
        );
        for positions in &row_positions {
            assert_eq!(
                positions, expected,
                "vertical borders must stay on identical columns across wrapped rows"
            );
        }
    }

    #[test]
    fn table_preserves_bold_and_inline_code_styles_through_wrapping() {
        let md = "| Column |\n|---|\n| **bold words** and `inline code` content |\n";
        let mut theme = MarkdownTheme::answer();
        theme.max_width = Some(20);
        let lines = render_markdown(md, &theme);

        let all_spans: Vec<&Span<'static>> =
            lines.iter().flat_map(|line| line.spans.iter()).collect();

        let has_bold = all_spans.iter().any(|span| {
            span.style.add_modifier.contains(Modifier::BOLD) && span.content.contains("bold")
        });
        assert!(
            has_bold,
            "bold text must retain the BOLD modifier after wrapping"
        );

        let code_fg = theme.inline_code.fg;
        let has_inline_code = all_spans.iter().any(|span| {
            span.style.fg == code_fg
                && (span.content.contains("inline") || span.content.contains("code"))
        });
        assert!(
            has_inline_code,
            "inline code must retain its style after wrapping"
        );
    }

    #[test]
    fn table_honors_mixed_column_alignment() {
        let md = "| Left | Center | Right |\n|:---|:---:|---:|\n| a | b | c |\n";
        let theme = MarkdownTheme::answer();
        let lines = render_markdown(md, &theme);

        let data_line = lines
            .iter()
            .find(|line| {
                let text = line_text(line);
                text.starts_with('│') && text.contains('a') && text.contains('c')
            })
            .expect("data row line");

        let cells: Vec<String> = line_text(data_line)
            .split('│')
            .map(|segment| segment.to_string())
            .collect();

        assert!(
            cells[1].starts_with(" a"),
            "left-aligned cell: {:?}",
            cells[1]
        );
        assert!(
            cells[2].starts_with("   b"),
            "center-aligned cell: {:?}",
            cells[2]
        );
        assert!(
            cells[3].starts_with("      c"),
            "right-aligned cell: {:?}",
            cells[3]
        );
    }

    #[test]
    fn table_expands_to_use_available_max_width() {
        let md = "| ID | Description |\n|---|---|\n| 01 | this is a very long descriptive text that should comfortably expand across the entire available screen width instead of stopping at an arbitrary cap |\n";
        let mut theme = MarkdownTheme::answer();
        theme.max_width = Some(100);
        let lines = render_markdown(md, &theme);

        let border = lines
            .iter()
            .find_map(|line| {
                let text = line_text(line);
                text.starts_with('┌').then_some(text)
            })
            .expect("table top border");

        assert_eq!(
            border.chars().count(),
            100,
            "table border width {} should exactly match max_width 100",
            border.chars().count()
        );

        let row_positions: Vec<Vec<usize>> = lines
            .iter()
            .map(vertical_bar_positions)
            .filter(|positions| !positions.is_empty())
            .collect();

        assert!(!row_positions.is_empty());
        for positions in &row_positions {
            assert_eq!(positions, &row_positions[0]);
        }
    }

    #[test]
    fn table_compresses_wide_columns_within_max_width() {
        let md = "| Column One | Column Two | Column Three |\n|---|---|---|\n| aaaa bbbb cccc dddd eeee ffff | gggg hhhh iiii jjjj | kkkk llll mmmm nnnn oooo |\n";
        let mut theme = MarkdownTheme::answer();
        theme.max_width = Some(40);
        let lines = render_markdown(md, &theme);

        let max_width = 40;
        for line in &lines {
            let text = line_text(line);
            assert!(
                text.chars().count() <= max_width,
                "rendered line {:?} exceeds max width {}",
                text,
                max_width
            );
        }

        let row_positions: Vec<Vec<usize>> = lines
            .iter()
            .map(vertical_bar_positions)
            .filter(|positions| !positions.is_empty())
            .collect();

        assert!(
            row_positions.len() > 2,
            "wide cells must wrap into extra physical rows"
        );
        for positions in &row_positions {
            assert_eq!(
                positions, &row_positions[0],
                "compressed columns must keep borders aligned"
            );
        }
    }

    #[test]
    fn table_renders_horizontal_separators_between_multiple_data_rows() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n";
        let theme = MarkdownTheme::answer();
        let lines = render_markdown(md, &theme);
        let sep_count = lines
            .iter()
            .filter(|l| line_text(l).starts_with('├'))
            .count();
        // One separator between header and row 1, and one separator between row 1 and row 2
        assert_eq!(sep_count, 2);
    }
}
