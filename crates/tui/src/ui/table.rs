//! Generic table layout and rendering for terminal surfaces.
//!
//! Column widths are measured with [`Span::width`], so Cyrillic, CJK, and other
//! wide glyphs occupy exactly the terminal columns they paint instead of the
//! scalar count of their text. The module is theme agnostic: a cell carries its
//! own styled spans and the caller passes the border style, so one layout serves
//! every palette and dialog.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Blank space separating borderless columns.
const COLUMN_GAP: &str = "  ";

/// Inner padding between a boxed cell and its border.
const CELL_PADDING: &str = " ";

/// Horizontal placement of a cell's content inside its column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColumnAlign {
    /// Content starts at the column's left edge.
    #[default]
    Left,
    /// Content is centered inside the column.
    Center,
    /// Content ends at the column's right edge.
    Right,
}

/// Frame styling of a rendered [`UiTable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TableBorderStyle {
    /// Columns separated by blank space with a horizontal rule under the header.
    #[default]
    Lightweight,
    /// Box-drawing frame around the table and between its columns.
    Boxed,
    /// Columns separated by blank space without a frame or a rule.
    Borderless,
}

/// One cell of a [`UiTable`]: styled content plus an optional alignment override.
#[derive(Debug, Clone)]
pub struct TableCell {
    spans: Vec<Span<'static>>,
    align: Option<ColumnAlign>,
}

impl TableCell {
    /// Creates a plain unstyled cell.
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            spans: vec![Span::raw(content.into())],
            align: None,
        }
    }

    /// Creates a cell from already styled spans.
    pub fn styled(spans: Vec<Span<'static>>) -> Self {
        Self { spans, align: None }
    }

    /// Aligns this cell independently of its column default.
    pub fn with_align(mut self, align: ColumnAlign) -> Self {
        self.align = Some(align);
        self
    }

    /// Display width of the content in terminal columns.
    fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }
}

/// A grid of cells rendered as aligned terminal lines.
#[derive(Debug, Clone)]
pub struct UiTable {
    header: Option<Vec<TableCell>>,
    rows: Vec<Vec<TableCell>>,
    border_style: TableBorderStyle,
    column_aligns: Vec<ColumnAlign>,
}

impl UiTable {
    /// Creates an empty table rendered in `border_style`.
    pub fn new(border_style: TableBorderStyle) -> Self {
        Self {
            header: None,
            rows: Vec::new(),
            border_style,
            column_aligns: Vec::new(),
        }
    }

    /// Sets the header row drawn above every data row.
    pub fn header(&mut self, cells: Vec<TableCell>) {
        self.header = Some(cells);
    }

    /// Sets the default alignment of each column.
    ///
    /// A cell aligned through [`TableCell::with_align`] keeps its own
    /// alignment, so the column default never overrides an explicit choice.
    pub fn column_aligns(&mut self, aligns: Vec<ColumnAlign>) {
        self.column_aligns = aligns;
    }

    /// Appends one data row.
    pub fn push_row(&mut self, cells: Vec<TableCell>) {
        self.rows.push(cells);
    }

    /// Renders the table, drawing every border glyph with `border`.
    pub fn render(&self, border: Style) -> Vec<Line<'static>> {
        let widths = self.column_widths();
        if widths.is_empty() {
            return Vec::new();
        }

        let boxed = self.border_style == TableBorderStyle::Boxed;
        let mut lines: Vec<Line<'static>> = Vec::new();

        if boxed {
            lines.push(border_line(boxed_rule(&widths, '┌', '┬', '┐'), border));
        }

        if let Some(header) = &self.header {
            if !header.is_empty() {
                lines.push(self.render_row(header, &widths, border));
                match self.border_style {
                    TableBorderStyle::Boxed => {
                        lines.push(border_line(boxed_rule(&widths, '├', '┼', '┤'), border));
                    }
                    TableBorderStyle::Lightweight => {
                        lines.push(border_line(flat_rule(&widths), border));
                    }
                    TableBorderStyle::Borderless => {}
                }
            }
        }

        for row in &self.rows {
            lines.push(self.render_row(row, &widths, border));
        }

        if boxed {
            lines.push(border_line(boxed_rule(&widths, '└', '┴', '┘'), border));
        }

        lines
    }

    fn render_row(&self, cells: &[TableCell], widths: &[usize], border: Style) -> Line<'static> {
        let boxed = self.border_style == TableBorderStyle::Boxed;
        let mut spans: Vec<Span<'static>> = Vec::new();

        if boxed {
            spans.push(Span::styled("│", border));
        }

        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                if boxed {
                    spans.push(Span::styled("│", border));
                } else {
                    spans.push(Span::raw(COLUMN_GAP));
                }
            }
            if boxed {
                spans.push(Span::raw(CELL_PADDING));
            }
            let cell = cells.get(index);
            push_cell(&mut spans, cell, *width, self.resolve_align(index, cell));
            if boxed {
                spans.push(Span::raw(CELL_PADDING));
            }
        }

        if boxed {
            spans.push(Span::styled("│", border));
        }

        Line::from(spans)
    }

    fn resolve_align(&self, index: usize, cell: Option<&TableCell>) -> ColumnAlign {
        cell.and_then(|cell| cell.align)
            .or_else(|| self.column_aligns.get(index).copied())
            .unwrap_or_default()
    }

    fn column_widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = Vec::new();
        for cells in self.rows_iter() {
            if widths.len() < cells.len() {
                widths.resize(cells.len(), 0);
            }
            for (index, cell) in cells.iter().enumerate() {
                widths[index] = widths[index].max(cell.width());
            }
        }
        widths
    }

    fn rows_iter(&self) -> impl Iterator<Item = &[TableCell]> + '_ {
        self.header
            .iter()
            .map(Vec::as_slice)
            .chain(self.rows.iter().map(Vec::as_slice))
    }
}

/// Wraps a border glyph run into a styled line.
fn border_line(text: String, border: Style) -> Line<'static> {
    Line::from(Span::styled(text, border))
}

/// Builds the horizontal rule drawn under a lightweight header.
fn flat_rule(widths: &[usize]) -> String {
    let columns: usize = widths.iter().sum();
    let gaps = COLUMN_GAP.len() * widths.len().saturating_sub(1);
    "─".repeat(columns + gaps)
}

/// Builds one horizontal frame segment of a boxed table.
fn boxed_rule(widths: &[usize], left: char, cross: char, right: char) -> String {
    let segment = 2 * CELL_PADDING.len();
    let mut rule = String::new();
    rule.push(left);
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            rule.push(cross);
        }
        rule.push_str(&"─".repeat(width + segment));
    }
    rule.push(right);
    rule
}

/// Pushes one padded cell into the row span buffer.
fn push_cell(
    spans: &mut Vec<Span<'static>>,
    cell: Option<&TableCell>,
    width: usize,
    align: ColumnAlign,
) {
    let content_width = cell.map_or(0, TableCell::width);
    let pad = width.saturating_sub(content_width);
    let left_pad = match align {
        ColumnAlign::Left => 0,
        ColumnAlign::Center => pad / 2,
        ColumnAlign::Right => pad,
    };
    let right_pad = pad - left_pad;

    if left_pad > 0 {
        spans.push(Span::raw(" ".repeat(left_pad)));
    }
    if let Some(cell) = cell {
        spans.extend(cell.spans.iter().cloned());
    }
    if right_pad > 0 {
        spans.push(Span::raw(" ".repeat(right_pad)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Modifier, Style};

    fn lines_text(table: &UiTable) -> Vec<String> {
        table
            .render(Style::default())
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn assert_uniform_width(table: &UiTable) {
        let widths: Vec<usize> = table
            .render(Style::default())
            .iter()
            .map(Line::width)
            .collect();
        assert!(!widths.is_empty(), "table rendered no lines");
        for width in &widths {
            assert_eq!(
                *width,
                widths[0],
                "ragged table lines: {:?}",
                lines_text(table)
            );
        }
    }

    #[test]
    fn wide_glyphs_are_measured_in_terminal_columns() {
        let mut table = UiTable::new(TableBorderStyle::Boxed);
        table.header(vec![TableCell::new("指标"), TableCell::new("Value")]);
        table.push_row(vec![TableCell::new("a"), TableCell::new("1")]);
        assert_uniform_width(&table);
    }

    #[test]
    fn cyrillic_labels_keep_every_column_aligned() {
        let mut table = UiTable::new(TableBorderStyle::Lightweight);
        table.header(vec![TableCell::new("Метрика"), TableCell::new("Значение")]);
        table.push_row(vec![
            TableCell::new("Токены подсказки"),
            TableCell::new("1200"),
        ]);
        table.push_row(vec![TableCell::new("Стоимость"), TableCell::new("0.01")]);

        assert_uniform_width(&table);
        let rendered = lines_text(&table);
        assert!(rendered[0].starts_with("Метрика"), "{:?}", rendered[0]);
        assert!(rendered[2].contains("1200"), "{:?}", rendered[2]);
        assert!(rendered[3].contains("0.01"), "{:?}", rendered[3]);
    }

    #[test]
    fn column_aligns_place_content_left_center_and_right() {
        let mut table = UiTable::new(TableBorderStyle::Borderless);
        table.header(vec![
            TableCell::new("name"),
            TableCell::new("mid"),
            TableCell::new("right"),
        ]);
        table.column_aligns(vec![
            ColumnAlign::Left,
            ColumnAlign::Center,
            ColumnAlign::Right,
        ]);
        table.push_row(vec![
            TableCell::new("a"),
            TableCell::new("b"),
            TableCell::new("c"),
        ]);

        let rendered = lines_text(&table);
        assert_eq!(rendered.len(), 2);
        assert!(rendered[1].starts_with("a   "), "{:?}", rendered[1]);
        assert!(rendered[1].contains(" b "), "{:?}", rendered[1]);
        assert!(rendered[1].ends_with("    c"), "{:?}", rendered[1]);
    }

    #[test]
    fn a_cell_alignment_overrides_its_column() {
        let mut table = UiTable::new(TableBorderStyle::Borderless);
        table.header(vec![TableCell::new("wide")]);
        table.column_aligns(vec![ColumnAlign::Left]);
        table.push_row(vec![TableCell::new("1").with_align(ColumnAlign::Right)]);

        let rendered = lines_text(&table);
        assert_eq!(rendered[1], "   1");
    }

    #[test]
    fn right_alignment_pads_wide_glyphs_by_display_width() {
        let mut table = UiTable::new(TableBorderStyle::Borderless);
        table.header(vec![TableCell::new("abcdef")]);
        table.column_aligns(vec![ColumnAlign::Right]);
        table.push_row(vec![TableCell::new("指标")]);

        let rendered = lines_text(&table);
        assert_eq!(rendered[0], "abcdef");
        assert_eq!(rendered[1], "  指标");
    }

    #[test]
    fn boxed_style_draws_a_full_frame() {
        let mut table = UiTable::new(TableBorderStyle::Boxed);
        table.header(vec![TableCell::new("A"), TableCell::new("B")]);
        table.push_row(vec![TableCell::new("1"), TableCell::new("2")]);

        let rendered = lines_text(&table);
        assert_eq!(rendered.len(), 5, "{:?}", rendered);
        assert!(rendered[0].starts_with('┌') && rendered[0].ends_with('┐'));
        assert!(rendered[1].starts_with('│') && rendered[1].ends_with('│'));
        assert!(rendered[2].starts_with('├') && rendered[2].ends_with('┤'));
        assert!(rendered[3].starts_with('│') && rendered[3].ends_with('│'));
        assert!(rendered[4].starts_with('└') && rendered[4].ends_with('┘'));
        assert_uniform_width(&table);
    }

    #[test]
    fn lightweight_style_rules_under_the_header_only() {
        let mut table = UiTable::new(TableBorderStyle::Lightweight);
        table.header(vec![TableCell::new("Metric"), TableCell::new("Value")]);
        table.push_row(vec![TableCell::new("Tokens"), TableCell::new("42")]);
        table.push_row(vec![TableCell::new("Cost"), TableCell::new("0.01")]);

        let rendered = lines_text(&table);
        assert_eq!(rendered.len(), 4, "{:?}", rendered);
        assert!(rendered[1].chars().all(|ch| ch == '─'), "{:?}", rendered[1]);
        assert_eq!(rendered[1].chars().count(), rendered[0].chars().count());
        assert!(!rendered[2].contains('─'), "{:?}", rendered[2]);
        assert!(!rendered[3].contains('─'), "{:?}", rendered[3]);
        assert_uniform_width(&table);
    }

    #[test]
    fn borderless_style_draws_no_frame_or_rule() {
        let mut table = UiTable::new(TableBorderStyle::Borderless);
        table.header(vec![TableCell::new("A"), TableCell::new("B")]);
        table.push_row(vec![TableCell::new("1"), TableCell::new("2")]);

        let rendered = lines_text(&table);
        assert_eq!(rendered.len(), 2, "{:?}", rendered);
        assert!(!rendered
            .iter()
            .any(|line| line.contains('│') || line.contains('─')));
    }

    #[test]
    fn styled_cells_keep_their_spans() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut table = UiTable::new(TableBorderStyle::Borderless);
        table.push_row(vec![TableCell::styled(vec![Span::styled("bold", bold)])]);

        let lines = table.render(Style::default());
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.content == "bold"
                    && span.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn rows_shorter_than_the_header_are_padded() {
        let mut table = UiTable::new(TableBorderStyle::Boxed);
        table.header(vec![TableCell::new("A"), TableCell::new("B")]);
        table.push_row(vec![TableCell::new("1")]);
        assert_uniform_width(&table);
    }

    #[test]
    fn an_empty_table_renders_nothing() {
        let table = UiTable::new(TableBorderStyle::Boxed);
        assert!(table.render(Style::default()).is_empty());
    }

    /// Display column at which `value` ends inside a rendered line.
    fn end_column(line: &Line<'_>, value: &str) -> usize {
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        let end = text.find(value).expect("rendered value") + value.len();
        Span::raw(text[..end].to_string()).width()
    }

    #[test]
    fn cyrillic_glyphs_are_measured_in_terminal_columns() {
        // Seven Cyrillic scalars occupy seven columns, not their fourteen UTF-8 bytes.
        assert_eq!(Span::raw("Метрика").width(), 7);

        let mut table = UiTable::new(TableBorderStyle::Borderless);
        table.header(vec![TableCell::new("Метрика"), TableCell::new("Value")]);
        table.column_aligns(vec![ColumnAlign::Left, ColumnAlign::Right]);
        table.push_row(vec![TableCell::new("Итого"), TableCell::new("42")]);
        table.push_row(vec![TableCell::new("Токены"), TableCell::new("1500")]);

        assert_uniform_width(&table);

        let lines = table.render(Style::default());
        assert_eq!(lines.len(), 3);
        let width = lines[0].width();
        assert_eq!(width, 14, "{:?}", lines_text(&table));
        for (index, value) in ["Value", "42", "1500"].iter().enumerate() {
            assert_eq!(
                end_column(&lines[index], value),
                width,
                "row {index} must end its value on the shared right edge"
            );
        }
    }

    #[test]
    fn right_aligned_numbers_share_one_edge_across_digit_widths() {
        let mut table = UiTable::new(TableBorderStyle::Lightweight);
        table.header(vec![TableCell::new("Metric"), TableCell::new("Value")]);
        table.column_aligns(vec![ColumnAlign::Left, ColumnAlign::Right]);
        table.push_row(vec![TableCell::new("tokens"), TableCell::new("7")]);
        table.push_row(vec![TableCell::new("cost"), TableCell::new("1500")]);
        table.push_row(vec![TableCell::new("duration"), TableCell::new("42.5")]);

        assert_uniform_width(&table);

        let lines = table.render(Style::default());
        let edge = lines[0].width();
        let cells = [(0, "Value"), (2, "7"), (3, "1500"), (4, "42.5")];
        for (index, value) in cells {
            assert_eq!(
                end_column(&lines[index], value),
                edge,
                "row {index} left the shared right edge"
            );
        }
    }

    #[test]
    fn boxed_frame_stays_uniform_with_cyrillic_cells() {
        let mut table = UiTable::new(TableBorderStyle::Boxed);
        table.header(vec![TableCell::new("Метрика"), TableCell::new("Значение")]);
        table.push_row(vec![TableCell::new("Токены"), TableCell::new("1500")]);

        assert_uniform_width(&table);

        let rendered = lines_text(&table);
        assert_eq!(rendered.len(), 5, "{rendered:?}");
        assert!(rendered[0].starts_with('┌') && rendered[0].ends_with('┐'));
        assert!(rendered[2].starts_with('├') && rendered[2].ends_with('┤'));
        assert!(rendered[4].starts_with('└') && rendered[4].ends_with('┘'));
    }
}
