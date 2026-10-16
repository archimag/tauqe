use std::sync::LazyLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::theme::color::RgbColor;

mod latex;
pub use latex::convert_latex_to_unicode;
use latex::render_text_spans;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

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
/// Truecolor terminals receive the exact RGB value, 256-color terminals the
/// nearest protected-cube index, and the 16-color fallback degrades to the
/// default foreground so text stays legible.
pub fn safe_rgb(r: u8, g: u8, b: u8) -> Color {
    if supports_truecolor() {
        Color::Rgb(r, g, b)
    } else if crate::theme::engine::current_depth() == crate::theme::color::ColorDepth::Ansi16 {
        Color::Reset
    } else {
        Color::Indexed(rgb_to_ansi256(r, g, b))
    }
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
        lang.trim().chars().count()
    };
    let start = (2 + lang_width + 1) as u16;
    let end = start + (COPY_BUTTON_LABEL.len() + 1) as u16;
    (start, end)
}

#[derive(Debug, Clone)]
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
        let body = engine.fg(palette.text_secondary);
        let accent = engine.fg(palette.status_info);
        Self {
            text_style: body,
            bold_style: engine
                .fg(palette.text_primary)
                .add_modifier(Modifier::BOLD),
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

fn resolve_syntax(token: &str) -> &'static syntect::parsing::SyntaxReference {
    let token_trimmed = token.trim();
    let token_lower = token_trimmed.to_lowercase();
    let lookup_candidates: &[&str] = match token_lower.as_str() {
        "ts" | "typescript" | "tsx" => &["JavaScript", "js", "ts"],
        "js" | "javascript" | "jsx" | "node" => &["JavaScript", "js"],
        "rs" | "rust" => &["Rust", "rust", "rs"],
        "py" | "python" | "python3" => &["Python", "python", "py"],
        "sh" | "bash" | "zsh" | "shell" => &["Bourne Again Shell (bash)", "bash", "sh"],
        "yml" | "yaml" => &["YAML", "yaml", "yml"],
        "json" => &["JSON", "json"],
        "toml" => &["TOML", "toml"],
        "md" | "markdown" => &["Markdown", "markdown", "md"],
        "c" => &["C", "c"],
        "cpp" | "c++" | "cc" | "cxx" => &["C++", "cpp", "c++"],
        "cs" | "c#" | "csharp" => &["C#", "cs"],
        "go" | "golang" => &["Go", "go"],
        "html" | "htm" => &["HTML", "html"],
        "css" => &["CSS", "css"],
        "sql" => &["SQL", "sql"],
        "xml" => &["XML", "xml"],
        _ => &[],
    };

    for &cand in lookup_candidates {
        if let Some(s) = SYNTAX_SET
            .find_syntax_by_name(cand)
            .or_else(|| SYNTAX_SET.find_syntax_by_token(cand))
            .or_else(|| SYNTAX_SET.find_syntax_by_extension(cand))
        {
            return s;
        }
    }

    if !token_trimmed.is_empty() {
        if let Some(s) = SYNTAX_SET
            .find_syntax_by_token(token_trimmed)
            .or_else(|| SYNTAX_SET.find_syntax_by_name(token_trimmed))
            .or_else(|| SYNTAX_SET.find_syntax_by_extension(token_trimmed))
        {
            return s;
        }
    }

    SYNTAX_SET.find_syntax_plain_text()
}

fn syntect_style_to_ratatui(style: SynStyle, fallback_bg: Color) -> Style {
    let mut s = Style::default().fg(safe_rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    s = s.bg(fallback_bg);
    if style.font_style.contains(syntect::highlighting::FontStyle::BOLD) {
        s = s.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(syntect::highlighting::FontStyle::ITALIC) {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(syntect::highlighting::FontStyle::UNDERLINE) {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    s
}

fn render_code_block(
    theme: &MarkdownTheme,
    cb: &CodeBlockState,
    out_lines: &mut Vec<Line<'static>>,
    is_streaming: bool,
    _block_id: usize,
    is_flashing: bool,
) {
    let raw_lines: Vec<&str> = cb.content.lines().collect();
    let lines: Vec<&str> = if raw_lines.is_empty() {
        vec![""]
    } else {
        raw_lines
    };
    let max_len = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);

    let lang_trimmed = cb.lang.trim();
    let content_width = max_len.max(36);
    let block_width = content_width + 4;

    let lang_label = if lang_trimmed.is_empty() {
        "code"
    } else {
        lang_trimmed
    };

    let mut header_spans = Vec::new();
    header_spans.push(Span::styled(format!("  {} ", lang_label), theme.code_lang));

    if is_streaming {
        header_spans.push(Span::styled("⠋ streaming", theme.code_lang));
    } else if is_flashing {
        header_spans.push(Span::styled(
            " [ Copied ] ",
            Style::default().bg(Color::Green).fg(Color::Black).bold(),
        ));
    } else {
        header_spans.push(Span::styled(
            format!(" {} ", COPY_BUTTON_LABEL),
            Style::default()
                .bg(safe_rgb(35, 45, 65))
                .fg(safe_rgb(170, 200, 240))
                .bold(),
        ));
    }

    push_styled_line(theme, out_lines, header_spans);

    // Top vertical padding line with background
    push_styled_line(
        theme,
        out_lines,
        vec![Span::styled(
            " ".repeat(block_width),
            Style::default().bg(theme.code_bg),
        )],
    );

    let syntax = resolve_syntax(lang_trimmed);

    let theme_name = if theme.is_light {
        "base16-ocean.light"
    } else {
        "base16-ocean.dark"
    };
    let syn_theme = THEME_SET
        .themes
        .get(theme_name)
        .or_else(|| THEME_SET.themes.values().next());

    let mut highlighter = syn_theme.map(|t| HighlightLines::new(syntax, t));

    for line in &lines {
        let mut spans = vec![Span::styled("  ", Style::default().bg(theme.code_bg))];
        let mut line_char_count = 0;

        if line.is_empty() {
            spans.push(Span::styled(
                " ".repeat(content_width + 2),
                Style::default().bg(theme.code_bg),
            ));
            push_styled_line(theme, out_lines, spans);
            continue;
        }

        if let Some(h) = highlighter.as_mut() {
            let line_with_nl = format!("{}\n", line);
            if let Ok(ranges) = h.highlight_line(&line_with_nl, &SYNTAX_SET) {
                for (syn_style, text) in ranges {
                    let clean = text.trim_end_matches(['\r', '\n']);
                    if !clean.is_empty() {
                        line_char_count += clean.chars().count();
                        spans.push(Span::styled(
                            clean.to_string(),
                            syntect_style_to_ratatui(syn_style, theme.code_bg),
                        ));
                    }
                }
            } else {
                line_char_count += line.chars().count();
                spans.push(Span::styled(
                    line.to_string(),
                    theme.code_text.bg(theme.code_bg),
                ));
            }
        } else {
            line_char_count += line.chars().count();
            spans.push(Span::styled(
                line.to_string(),
                theme.code_text.bg(theme.code_bg),
            ));
        }

        let pad_len = content_width.saturating_sub(line_char_count) + 2;
        spans.push(Span::styled(
            " ".repeat(pad_len),
            Style::default().bg(theme.code_bg),
        ));

        push_styled_line(theme, out_lines, spans);
    }

    // Bottom vertical padding line with background
    push_styled_line(
        theme,
        out_lines,
        vec![Span::styled(
            " ".repeat(block_width),
            Style::default().bg(theme.code_bg),
        )],
    );

    if !theme.compact && !is_streaming {
        push_empty_line(theme, out_lines);
    }
}

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

fn render_table(
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
                    render_code_block(theme, &cb, &mut out_lines, false, id, is_flashing);
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
                        ts.current_cell_len += span.content.chars().count();
                        ts.current_cell_spans.push(span);
                    } else {
                        current_spans.push(span);
                    }
                }
            }
            Event::Code(code) => {
                let code_span = Span::styled(format!(" {} ", code), theme.inline_code);
                if let Some(ts) = table_state.as_mut() {
                    ts.current_cell_len += code_span.content.chars().count();
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
                            ts.current_cell_len += span.content.chars().count();
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
                let marker = if checked {
                    Span::styled(
                        "✔ ",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled("☐ ", theme.code_border)
                };
                if let Some(ts) = table_state.as_mut() {
                    ts.current_cell_len += marker.content.chars().count();
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
        render_code_block(theme, &cb, &mut out_lines, true, id, is_flashing);
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
        assert_eq!(text, vec!["Первая строка", "Вторая строка", "Третья строка"]);
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
    fn test_code_block_empty_lines_and_typescript_highlighting() {
        let ts_code = "```typescript\nconst x: number = 42;\n\nfunction test(): void {}\n```";
        let theme = MarkdownTheme::answer();
        let lines = render_markdown(ts_code, &theme);
        let default_code_fg = safe_rgb(220, 225, 235);
        let has_highlighting = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.style.fg.is_some_and(|fg| fg != default_code_fg)
            })
        });
        assert!(has_highlighting, "TypeScript code must have syntax highlighting");

        let code_bg = theme.code_bg;
        // lines[0] is lang header ("typescript")
        // lines[1] is top padding
        assert_eq!(lines[1].spans[0].style.bg, Some(code_bg), "Top padding must have code_bg");
        // lines[2] is const x: number = 42;
        assert_eq!(lines[2].spans[0].style.bg, Some(code_bg), "Code line must have code_bg");
        // lines[3] is the empty line inside the code block
        assert_eq!(lines[3].spans[0].style.bg, Some(code_bg), "Empty line inside code block must have code_bg");
        // lines[4] is function test(): void {}
        assert_eq!(lines[4].spans[0].style.bg, Some(code_bg), "Code line must have code_bg");
        // lines[5] is bottom padding
        assert_eq!(lines[5].spans[0].style.bg, Some(code_bg), "Bottom padding must have code_bg");
    }

    #[test]
    fn code_block_renders_borderless_with_padding_and_background() {
        let md = "```rust\nfn main() {}\n```";
        let theme = MarkdownTheme::answer();
        let lines = render_markdown(md, &theme);
        assert!(lines.len() >= 4);

        let lang_line = &lines[0];
        let top_pad = &lines[1];
        let code_line = &lines[2];
        let bot_pad = &lines[3];

        let lang_text: String = lang_line.spans.iter().map(|s| s.content.as_ref()).collect();
        let top_text: String = top_pad.spans.iter().map(|s| s.content.as_ref()).collect();
        let code_text: String = code_line.spans.iter().map(|s| s.content.as_ref()).collect();
        let bot_text: String = bot_pad.spans.iter().map(|s| s.content.as_ref()).collect();

        assert!(lang_text.contains("rust"));
        assert!(code_text.contains("fn main() {}"));

        // Frame characters should not be present
        for text in [&lang_text, &top_text, &code_text, &bot_text] {
            assert!(!text.contains('┌') && !text.contains('┐') && !text.contains('│') && !text.contains('└') && !text.contains('┘'));
        }

        // Top, code, and bottom padding lines must have identical uniform block width
        assert_eq!(top_text.chars().count(), bot_text.chars().count());
        assert_eq!(top_text.chars().count(), code_text.chars().count());

        // Background should be applied across the entire block
        assert_eq!(top_pad.spans[0].style.bg, Some(theme.code_bg));
        assert_eq!(code_line.spans[0].style.bg, Some(theme.code_bg));
        assert_eq!(code_line.spans.last().unwrap().style.bg, Some(theme.code_bg));
        assert_eq!(bot_pad.spans[0].style.bg, Some(theme.code_bg));
    }

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
    }
}
