use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::color::RgbColor;

mod cache;
mod code;
mod latex;
mod table;

pub use cache::MarkdownBodyCache;
pub use latex::convert_latex_to_unicode;

use code::render_code_block;
use latex::render_text_spans;
use table::render_table;

/// Applies the configured color-depth override; `Auto` restores detection.
///
/// The authoritative depth state lives in the theme engine; this keeps the
/// historical markdown-side entry point delegating to it.
pub fn set_color_depth_override(depth: crate::config::ColorDepth) {
    crate::theme::engine::set_color_depth(depth);
}

/// Whether the active color depth is 24-bit truecolor.
pub fn supports_truecolor() -> bool {
    matches!(
        crate::theme::engine::current_depth(),
        crate::theme::color::ColorDepth::Truecolor
    )
}

/// Projects an 8-bit-per-channel color onto the xterm 256-color cube.
pub fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    RgbColor::new(r, g, b).to_ansi256()
}

/// Projects a raw RGB color onto the active terminal color depth.
///
/// The projection is delegated to the theme engine, so raw syntax-highlighting
/// colors follow the same single path as the semantic palette
/// (`crates/tui/Conventions.md` §8): truecolor terminals receive the exact RGB
/// value, 256-color terminals the nearest protected-cube index, and the
/// 16-color fallback degrades to the default foreground.
pub fn safe_rgb(r: u8, g: u8, b: u8) -> Color {
    crate::theme::engine::current_engine().color(RgbColor::new(r, g, b))
}

/// Clickable label of the code-block copy button.
///
/// Kept ASCII so its display width is deterministic across terminals; emoji
/// glyphs such as `📋` occupy an ambiguous number of columns.
pub const COPY_BUTTON_LABEL: &str = "[ Copy ]";

/// Inclusive display-column span `(start, end)` of the clickable copy button on
/// a code-block header line that renders `lang`.
///
/// The header is `"  {lang} [ Copy ] "`. Deriving the span from
/// [`COPY_BUTTON_LABEL`] keeps mouse hit-testing and rendering on one source of
/// truth instead of a hand-tuned width.
pub fn copy_button_span(lang: &str) -> (u16, u16) {
    let lang_width = if lang.trim().is_empty() {
        4
    } else {
        lang.trim().width()
    };
    let start = (2 + lang_width + 1) as u16;
    let end = start + (COPY_BUTTON_LABEL.width() + 1) as u16;
    (start, end)
}

#[derive(Debug, Clone, PartialEq)]
pub struct MarkdownTheme {
    pub text_style: Style,
    pub bold_style: Style,
    pub italic_style: Style,
    pub h1: Style,
    pub h2: Style,
    pub h3: Style,
    pub h4: Style,
    pub h5: Style,
    pub h6: Style,
    pub inline_code: Style,
    pub code_border: Style,
    pub code_lang: Style,
    pub code_text: Style,
    pub code_bg: Color,
    pub blockquote_bar: Style,
    pub blockquote_text: Style,
    pub list_bullet: Style,
    pub link: Style,
    pub hr: Style,
    pub table_border: Style,
    pub table_head: Style,
    pub table_cell: Style,
    pub math_style: Style,
    pub compact: bool,
    pub hard_breaks: bool,
    pub line_prefix: Option<Span<'static>>,
    pub max_width: Option<usize>,
    /// Whether the active palette targets a light terminal background.
    ///
    /// The syntax-highlighting theme is selected from this explicit flag
    /// instead of inferring the palette from individual color values.
    pub is_light: bool,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        Self::answer()
    }
}

impl MarkdownTheme {
    /// Builds the answer-rendering Markdown theme from the active semantic
    /// palette, projecting every role through the theme engine so headings,
    /// quotes, lists, tables, and code blocks stay legible at each color depth.
    pub fn answer_themed(mode: crate::config::ThemeMode) -> Self {
        Self::from_semantic(&crate::theme::engine::active_engine(mode), false)
    }

    fn from_semantic(engine: &crate::theme::engine::ThemeEngine, reasoning: bool) -> Self {
        if reasoning {
            Self::from_palette_reasoning(engine)
        } else {
            Self::from_palette_answer(engine)
        }
    }

    fn from_palette_answer(engine: &crate::theme::engine::ThemeEngine) -> Self {
        let palette = engine.palette();
        let body = engine.fg(palette.text_primary);
        let accent = engine.fg(palette.status_info);
        Self {
            text_style: body,
            bold_style: body.add_modifier(Modifier::BOLD),
            italic_style: body.add_modifier(Modifier::ITALIC),
            h1: engine
                .fg(palette.button_active_bg)
                .add_modifier(Modifier::BOLD),
            h2: accent.add_modifier(Modifier::BOLD),
            h3: engine
                .fg(palette.status_success)
                .add_modifier(Modifier::BOLD),
            h4: engine
                .fg(palette.status_warning)
                .add_modifier(Modifier::BOLD),
            h5: body.add_modifier(Modifier::BOLD),
            h6: engine.fg(palette.text_muted).add_modifier(Modifier::BOLD),
            inline_code: engine
                .fg(palette.status_warning)
                .bg(engine.color(palette.surface))
                .add_modifier(Modifier::BOLD),
            code_border: engine.fg(palette.border),
            code_lang: engine
                .fg(palette.status_warning)
                .add_modifier(Modifier::BOLD),
            code_text: body,
            code_bg: engine.color(palette.code_bg),
            blockquote_bar: accent,
            blockquote_text: engine.fg(palette.text_secondary),
            list_bullet: accent.add_modifier(Modifier::BOLD),
            link: accent.add_modifier(Modifier::UNDERLINED),
            hr: engine.fg(palette.text_muted),
            table_border: engine.fg(palette.border),
            table_head: accent.add_modifier(Modifier::BOLD),
            table_cell: body,
            math_style: accent.add_modifier(Modifier::ITALIC),
            compact: false,
            hard_breaks: false,
            line_prefix: None,
            max_width: None,
            is_light: engine.is_light(),
        }
    }

    fn from_palette_reasoning(engine: &crate::theme::engine::ThemeEngine) -> Self {
        let palette = engine.palette();
        let body = engine.fg(palette.text_reasoning);
        let accent = engine.fg(palette.status_info);
        Self {
            text_style: body,
            bold_style: engine.fg(palette.text_primary).add_modifier(Modifier::BOLD),
            italic_style: body.add_modifier(Modifier::ITALIC),
            h1: accent.add_modifier(Modifier::BOLD),
            h2: accent.add_modifier(Modifier::BOLD),
            h3: accent.add_modifier(Modifier::BOLD),
            h4: accent.add_modifier(Modifier::BOLD),
            h5: accent.add_modifier(Modifier::BOLD),
            h6: accent.add_modifier(Modifier::BOLD),
            inline_code: engine
                .fg(palette.text_primary)
                .bg(engine.color(palette.code_bg)),
            code_border: engine.fg(palette.border),
            code_lang: accent,
            code_text: body,
            code_bg: engine.color(palette.code_bg),
            blockquote_bar: engine.fg(palette.border),
            blockquote_text: body,
            list_bullet: accent,
            link: accent.add_modifier(Modifier::UNDERLINED),
            hr: engine.fg(palette.border),
            table_border: engine.fg(palette.border),
            table_head: accent,
            table_cell: body,
            math_style: body.add_modifier(Modifier::ITALIC),
            compact: true,
            hard_breaks: false,
            line_prefix: None,
            max_width: None,
            is_light: engine.is_light(),
        }
    }

    pub fn answer() -> Self {
        Self::answer_themed(crate::config::ThemeMode::Auto)
    }

    /// Builds the subdued reasoning-rendering Markdown theme from the active
    /// semantic palette, keeping the thinking transcript legible while visually
    /// distinct from the answer body.
    pub fn reasoning_themed(mode: crate::config::ThemeMode) -> Self {
        Self::from_semantic(&crate::theme::engine::active_engine(mode), true)
    }

    pub fn reasoning() -> Self {
        Self::reasoning_themed(crate::config::ThemeMode::Auto)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct StyleModifier {
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) strikethrough: bool,
}

impl StyleModifier {
    pub(crate) fn to_style(self, base: Style, theme: &MarkdownTheme) -> Style {
        let mut s = base;
        if self.bold {
            s = s.patch(theme.bold_style);
        }
        if self.italic {
            s = s.patch(theme.italic_style);
        }
        if self.strikethrough {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        s
    }
}

struct ListState {
    ordered: bool,
    next_num: u64,
}

struct CodeBlockState {
    lang: String,
    content: String,
}

#[derive(Debug, Clone)]
pub struct ExtractedCodeBlock {
    pub id: usize,
    pub lang: String,
    pub content: String,
    pub start_line: usize,
    pub end_line: usize,
}

struct TableCellData {
    spans: Vec<Span<'static>>,
    text_len: usize,
}

struct TableState {
    alignments: Vec<Alignment>,
    headers: Vec<TableCellData>,
    rows: Vec<Vec<TableCellData>>,
    current_row: Vec<TableCellData>,
    current_cell_spans: Vec<Span<'static>>,
    current_cell_len: usize,
    in_head: bool,
}

fn push_styled_line(
    theme: &MarkdownTheme,
    out_lines: &mut Vec<Line<'static>>,
    mut spans: Vec<Span<'static>>,
) {
    if let Some(prefix) = &theme.line_prefix {
        spans.insert(0, prefix.clone());
    }
    out_lines.push(Line::from(spans));
}

fn push_empty_line(theme: &MarkdownTheme, out_lines: &mut Vec<Line<'static>>) {
    if let Some(prefix) = &theme.line_prefix {
        out_lines.push(Line::from(vec![prefix.clone()]));
    } else {
        out_lines.push(Line::raw(""));
    }
}

fn flush_line(
    theme: &MarkdownTheme,
    current_spans: &mut Vec<Span<'static>>,
    out_lines: &mut Vec<Line<'static>>,
) {
    if !current_spans.is_empty() {
        let spans = std::mem::take(current_spans);
        push_styled_line(theme, out_lines, spans);
    }
}

fn ensure_line_prefix(
    theme: &MarkdownTheme,
    current_spans: &mut Vec<Span<'static>>,
    quote_depth: usize,
    list_stack: &mut [ListState],
    item_started: &mut bool,
) {
    if current_spans.is_empty() {
        if quote_depth > 0 {
            current_spans.push(Span::styled("▌ ".repeat(quote_depth), theme.blockquote_bar));
        }
        if *item_started {
            let depth = list_stack.len().saturating_sub(1);
            if let Some(last_list) = list_stack.last_mut() {
                let indent = "  ".repeat(depth);
                if last_list.ordered {
                    current_spans.push(Span::styled(
                        format!("{}{}. ", indent, last_list.next_num),
                        theme.list_bullet,
                    ));
                    last_list.next_num += 1;
                } else {
                    let bullet = match depth {
                        0 => "• ",
                        1 => "◦ ",
                        _ => "▪ ",
                    };
                    current_spans.push(Span::styled(
                        format!("{}{}", indent, bullet),
                        theme.list_bullet,
                    ));
                }
                *item_started = false;
            }
        } else if !list_stack.is_empty() {
            let depth = list_stack.len().saturating_sub(1);
            let indent = "  ".repeat(depth);
            current_spans.push(Span::raw(format!("{}  ", indent)));
        }
    }
}

pub fn render_markdown(text: &str, theme: &MarkdownTheme) -> Vec<Line<'static>> {
    render_markdown_with_blocks(text, theme, None).0
}

pub fn render_markdown_with_blocks(
    text: &str,
    theme: &MarkdownTheme,
    flashing_block_id: Option<usize>,
) -> (Vec<Line<'static>>, Vec<ExtractedCodeBlock>) {
    if text.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);

    let parser = Parser::new_ext(text, options);

    let mut out_lines: Vec<Line<'static>> = Vec::new();
    let mut extracted_blocks = Vec::new();
    let mut block_counter = 0;
    let mut style_mod = StyleModifier::default();
    let mut link_url: Option<String> = None;
    let mut current_heading: Option<HeadingLevel> = None;
    let mut quote_depth: usize = 0;
    let mut list_stack: Vec<ListState> = Vec::new();
    let mut item_started = false;
    let mut code_block: Option<CodeBlockState> = None;
    let mut table_state: Option<TableState> = None;
    let mut current_spans: Vec<Span<'static>> = Vec::new();

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                flush_line(theme, &mut current_spans, &mut out_lines);
                if !out_lines.is_empty() && !theme.compact {
                    push_empty_line(theme, &mut out_lines);
                }
                current_heading = Some(level);
            }
            Event::End(TagEnd::Heading(_)) => {
                let level = current_heading.take().unwrap_or(HeadingLevel::H1);
                let (prefix, h_style) = match level {
                    HeadingLevel::H1 => ("# ", theme.h1),
                    HeadingLevel::H2 => ("## ", theme.h2),
                    HeadingLevel::H3 => ("### ", theme.h3),
                    HeadingLevel::H4 => ("#### ", theme.h4),
                    HeadingLevel::H5 => ("##### ", theme.h5),
                    HeadingLevel::H6 => ("###### ", theme.h6),
                };
                let mut heading_spans = vec![Span::styled(prefix, h_style)];
                for span in current_spans.drain(..) {
                    heading_spans.push(Span::styled(span.content, h_style));
                }
                push_styled_line(theme, &mut out_lines, heading_spans);
                if !theme.compact {
                    push_empty_line(theme, &mut out_lines);
                }
            }
            Event::Start(Tag::BlockQuote(..)) => {
                flush_line(theme, &mut current_spans, &mut out_lines);
                quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(..)) => {
                flush_line(theme, &mut current_spans, &mut out_lines);
                quote_depth = quote_depth.saturating_sub(1);
            }
            Event::Start(Tag::CodeBlock(ref kind)) => {
                flush_line(theme, &mut current_spans, &mut out_lines);
                if !out_lines.is_empty() && !theme.compact {
                    push_empty_line(theme, &mut out_lines);
                }
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code_block = Some(CodeBlockState {
                    lang,
                    content: String::new(),
                });
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(cb) = code_block.take() {
                    let id = block_counter;
                    block_counter += 1;
                    let is_flashing = flashing_block_id == Some(id);
                    let start_line = out_lines.len();
                    render_code_block(theme, &cb, &mut out_lines, false, is_flashing);
                    let end_line = out_lines.len().saturating_sub(1);
                    extracted_blocks.push(ExtractedCodeBlock {
                        id,
                        lang: cb.lang,
                        content: cb.content,
                        start_line,
                        end_line,
                    });
                }
            }
            Event::Start(Tag::List(start_num)) => {
                flush_line(theme, &mut current_spans, &mut out_lines);
                list_stack.push(ListState {
                    ordered: start_num.is_some(),
                    next_num: start_num.unwrap_or(1),
                });
            }
            Event::End(TagEnd::List(..)) => {
                flush_line(theme, &mut current_spans, &mut out_lines);
                list_stack.pop();
            }
            Event::Start(Tag::Item) => {
                flush_line(theme, &mut current_spans, &mut out_lines);
                item_started = true;
            }
            Event::End(TagEnd::Item) => {
                flush_line(theme, &mut current_spans, &mut out_lines);
                item_started = false;
            }
            Event::Start(Tag::Table(alignments)) => {
                flush_line(theme, &mut current_spans, &mut out_lines);
                if !out_lines.is_empty() && !theme.compact {
                    push_empty_line(theme, &mut out_lines);
                }
                table_state = Some(TableState {
                    alignments,
                    headers: Vec::new(),
                    rows: Vec::new(),
                    current_row: Vec::new(),
                    current_cell_spans: Vec::new(),
                    current_cell_len: 0,
                    in_head: false,
                });
            }
            Event::End(TagEnd::Table) => {
                if let Some(ts) = table_state.take() {
                    render_table(theme, ts, &mut out_lines, false);
                }
            }
            Event::Start(Tag::TableHead) => {
                if let Some(ts) = table_state.as_mut() {
                    ts.in_head = true;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(ts) = table_state.as_mut() {
                    ts.in_head = false;
                    if !ts.current_row.is_empty() {
                        ts.headers = std::mem::take(&mut ts.current_row);
                    }
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(ts) = table_state.as_mut() {
                    ts.current_row.clear();
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(ts) = table_state.as_mut() {
                    if ts.in_head {
                        if !ts.current_row.is_empty() {
                            ts.headers = std::mem::take(&mut ts.current_row);
                        }
                    } else {
                        let row = std::mem::take(&mut ts.current_row);
                        ts.rows.push(row);
                    }
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(ts) = table_state.as_mut() {
                    ts.current_cell_spans.clear();
                    ts.current_cell_len = 0;
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(ts) = table_state.as_mut() {
                    let cell = TableCellData {
                        spans: std::mem::take(&mut ts.current_cell_spans),
                        text_len: ts.current_cell_len,
                    };
                    ts.current_row.push(cell);
                    ts.current_cell_len = 0;
                }
            }
            Event::Start(Tag::Paragraph) => {
                flush_line(theme, &mut current_spans, &mut out_lines);
            }
            Event::End(TagEnd::Paragraph) => {
                flush_line(theme, &mut current_spans, &mut out_lines);
                push_empty_line(theme, &mut out_lines);
            }
            Event::Start(Tag::Strong) => {
                style_mod.bold = true;
            }
            Event::End(TagEnd::Strong) => {
                style_mod.bold = false;
            }
            Event::Start(Tag::Emphasis) => {
                style_mod.italic = true;
            }
            Event::End(TagEnd::Emphasis) => {
                style_mod.italic = false;
            }
            Event::Start(Tag::Strikethrough) => {
                style_mod.strikethrough = true;
            }
            Event::End(TagEnd::Strikethrough) => {
                style_mod.strikethrough = false;
            }
            Event::Start(Tag::Link { ref dest_url, .. }) => {
                link_url = Some(dest_url.to_string());
            }
            Event::End(TagEnd::Link) => {
                if let Some(url) = link_url.take() {
                    let span = Span::styled(format!(" ({})", url), theme.code_border);
                    if let Some(ts) = table_state.as_mut() {
                        ts.current_cell_len += span.width();
                        ts.current_cell_spans.push(span);
                    } else {
                        current_spans.push(span);
                    }
                }
            }
            Event::Code(code) => {
                let code_span = Span::styled(format!(" {} ", code), theme.inline_code);
                if let Some(ts) = table_state.as_mut() {
                    ts.current_cell_len += code_span.width();
                    ts.current_cell_spans.push(code_span);
                } else {
                    ensure_line_prefix(
                        theme,
                        &mut current_spans,
                        quote_depth,
                        &mut list_stack,
                        &mut item_started,
                    );
                    current_spans.push(code_span);
                }
            }
            Event::Text(t) => {
                if let Some(cb) = code_block.as_mut() {
                    cb.content.push_str(&t);
                } else {
                    let mut base_style = theme.text_style;
                    if quote_depth > 0 {
                        base_style = theme.blockquote_text;
                    }
                    if link_url.is_some() {
                        base_style = theme.link;
                    }
                    let spans = render_text_spans(&t, base_style, theme, style_mod);

                    if let Some(ts) = table_state.as_mut() {
                        for span in spans {
                            ts.current_cell_len += span.width();
                            ts.current_cell_spans.push(span);
                        }
                    } else {
                        ensure_line_prefix(
                            theme,
                            &mut current_spans,
                            quote_depth,
                            &mut list_stack,
                            &mut item_started,
                        );
                        current_spans.extend(spans);
                    }
                }
            }
            Event::TaskListMarker(checked) => {
                let engine = crate::theme::engine::current_engine();
                let palette = engine.palette();
                let marker = if checked {
                    Span::styled(
                        "✔ ",
                        engine
                            .fg(palette.status_success)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled("☐ ", theme.code_border)
                };
                if let Some(ts) = table_state.as_mut() {
                    ts.current_cell_len += marker.width();
                    ts.current_cell_spans.push(marker);
                } else {
                    ensure_line_prefix(
                        theme,
                        &mut current_spans,
                        quote_depth,
                        &mut list_stack,
                        &mut item_started,
                    );
                    current_spans.push(marker);
                }
            }
            Event::SoftBreak => {
                if code_block.is_none() && table_state.is_none() {
                    if theme.hard_breaks {
                        flush_line(theme, &mut current_spans, &mut out_lines);
                    } else {
                        current_spans.push(Span::raw(" "));
                    }
                }
            }
            Event::HardBreak => {
                if code_block.is_none() && table_state.is_none() {
                    flush_line(theme, &mut current_spans, &mut out_lines);
                }
            }
            Event::Rule => {
                flush_line(theme, &mut current_spans, &mut out_lines);
                push_styled_line(
                    theme,
                    &mut out_lines,
                    vec![Span::styled(
                        "  ────────────────────────────────────────",
                        theme.hr,
                    )],
                );
            }
            _ => {}
        }
    }

    if let Some(cb) = code_block {
        let id = block_counter;
        let is_flashing = flashing_block_id == Some(id);
        let start_line = out_lines.len();
        render_code_block(theme, &cb, &mut out_lines, true, is_flashing);
        let end_line = out_lines.len().saturating_sub(1);
        extracted_blocks.push(ExtractedCodeBlock {
            id,
            lang: cb.lang,
            content: cb.content,
            start_line,
            end_line,
        });
    }
    if let Some(ts) = table_state {
        render_table(theme, ts, &mut out_lines, true);
    }
    flush_line(theme, &mut current_spans, &mut out_lines);

    while let Some(last) = out_lines.last() {
        let is_empty = last.spans.is_empty()
            || (last.spans.iter().all(|s| s.style.bg.is_none())
                && (last.spans.iter().all(|s| s.content.trim().is_empty())
                    || (last.spans.len() == 1
                        && theme
                            .line_prefix
                            .as_ref()
                            .is_some_and(|prefix| last.spans[0].content == prefix.content))));
        if is_empty {
            out_lines.pop();
        } else {
            break;
        }
    }

    (out_lines, extracted_blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_paragraphs_have_a_separator_without_trailing_blank_lines() {
        let lines = render_markdown(
            "Первый абзац.\n\nВторой абзац.",
            &MarkdownTheme::reasoning(),
        );
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert_eq!(text, vec!["Первый абзац.", "", "Второй абзац."]);
    }

    #[test]
    fn test_hard_breaks_preserves_single_newlines() {
        let mut theme = MarkdownTheme::answer();
        theme.hard_breaks = true;
        let lines = render_markdown("Первая строка\nВторая строка\nТретья строка", &theme);
        assert_eq!(lines.len(), 3);
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(
            text,
            vec!["Первая строка", "Вторая строка", "Третья строка"]
        );
    }

    #[test]
    fn answer_paragraph_spacing_is_unchanged() {
        let lines = render_markdown("Первый абзац.\n\nВторой абзац.", &MarkdownTheme::answer());
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert_eq!(text, vec!["Первый абзац.", "", "Второй абзац."]);
    }

    #[test]
    fn copy_button_span_matches_the_ascii_header_layout() {
        // The header line renders as "  {lang} [ Copy ] ".
        assert!(COPY_BUTTON_LABEL.is_ascii());
        let (start, end) = copy_button_span("rust");
        assert_eq!(start, 2 + 4 + 1);
        assert_eq!(end, start + 9);

        // An empty language falls back to the four-column "code" label.
        let (start, end) = copy_button_span("   ");
        assert_eq!(start, 2 + 4 + 1);
        assert_eq!(end, start + 9);

        // A wide (CJK) label shifts the button by its display width, not by
        // its scalar count, so hit-testing follows what is actually painted.
        let (start, _) = copy_button_span("日本語");
        assert_eq!(start, 2 + 6 + 1);
    }
}
