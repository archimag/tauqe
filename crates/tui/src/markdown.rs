use std::sync::LazyLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

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
    pub compact: bool,
    pub line_prefix: Option<Span<'static>>,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        Self::answer()
    }
}

impl MarkdownTheme {
    pub fn answer() -> Self {
        Self {
            text_style: Style::default().fg(Color::White),
            bold_style: Style::default().add_modifier(Modifier::BOLD),
            italic_style: Style::default().add_modifier(Modifier::ITALIC),
            h1: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            h2: Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
            h3: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            h4: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            h5: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            h6: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
            inline_code: Style::default()
                .bg(Color::Rgb(36, 40, 52))
                .fg(Color::Rgb(240, 205, 120))
                .add_modifier(Modifier::BOLD),
            code_border: Style::default().fg(Color::Rgb(75, 90, 115)),
            code_lang: Style::default()
                .fg(Color::Rgb(245, 205, 100))
                .add_modifier(Modifier::BOLD),
            code_text: Style::default().fg(Color::Rgb(220, 225, 235)),
            code_bg: Color::Rgb(24, 27, 36),
            blockquote_bar: Style::default().fg(Color::Cyan),
            blockquote_text: Style::default().fg(Color::Gray),
            list_bullet: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            link: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
            hr: Style::default().fg(Color::DarkGray),
            table_border: Style::default().fg(Color::DarkGray),
            table_head: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            table_cell: Style::default().fg(Color::White),
            compact: false,
            line_prefix: None,
        }
    }

    pub fn reasoning() -> Self {
        let base_blue = Style::default()
            .fg(Color::Rgb(125, 160, 205))
            .add_modifier(Modifier::DIM);

        Self {
            text_style: base_blue,
            bold_style: Style::default().fg(Color::Rgb(150, 185, 230)),
            italic_style: Style::default().add_modifier(Modifier::ITALIC),
            h1: Style::default().fg(Color::Rgb(155, 195, 240)),
            h2: Style::default().fg(Color::Rgb(140, 180, 225)),
            h3: Style::default().fg(Color::Rgb(130, 170, 215)),
            h4: Style::default().fg(Color::Rgb(120, 160, 205)),
            h5: Style::default().fg(Color::Rgb(110, 150, 195)),
            h6: Style::default().fg(Color::Rgb(95, 130, 175)),
            inline_code: Style::default()
                .fg(Color::Rgb(165, 200, 240))
                .bg(Color::Rgb(30, 42, 60)),
            code_border: Style::default()
                .fg(Color::Rgb(60, 85, 120))
                .add_modifier(Modifier::DIM),
            code_lang: Style::default()
                .fg(Color::Rgb(130, 170, 220))
                .add_modifier(Modifier::DIM),
            code_text: base_blue,
            code_bg: Color::Rgb(18, 25, 38),
            blockquote_bar: Style::default().fg(Color::Rgb(70, 95, 130)),
            blockquote_text: base_blue,
            list_bullet: Style::default().fg(Color::Rgb(100, 145, 195)),
            link: Style::default()
                .fg(Color::Rgb(130, 170, 225))
                .add_modifier(Modifier::UNDERLINED),
            hr: Style::default()
                .fg(Color::Rgb(60, 85, 120))
                .add_modifier(Modifier::DIM),
            table_border: Style::default()
                .fg(Color::Rgb(60, 85, 120))
                .add_modifier(Modifier::DIM),
            table_head: Style::default().fg(Color::Rgb(145, 185, 230)),
            table_cell: base_blue,
            compact: true,
            line_prefix: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct StyleModifier {
    bold: bool,
    italic: bool,
    strikethrough: bool,
}

impl StyleModifier {
    fn to_style(self, base: Style, theme: &MarkdownTheme) -> Style {
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
    let mut s = Style::default().fg(Color::Rgb(
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
            " [✓ Copied!] ",
            Style::default().bg(Color::Green).fg(Color::Black).bold(),
        ));
    } else {
        header_spans.push(Span::styled(
            " [📋 Copy] ",
            Style::default()
                .bg(Color::Rgb(35, 45, 65))
                .fg(Color::Rgb(170, 200, 240))
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

    let syn_theme = THEME_SET
        .themes
        .get("base16-ocean.dark")
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

    let mut col_widths = vec![8; col_count];
    for (i, h) in ts.headers.iter().enumerate() {
        if i < col_count {
            col_widths[i] = col_widths[i].max(h.text_len + 2);
        }
    }
    for row in &ts.rows {
        for (i, cell) in row.iter().enumerate() {
            if i < col_count {
                col_widths[i] = col_widths[i].max(cell.text_len + 2);
            }
        }
    }

    for w in &mut col_widths {
        *w = (*w).min(45);
    }

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

    if !ts.headers.is_empty() {
        let mut head_line: Vec<Span<'static>> = Vec::new();
        head_line.push(Span::styled("│", theme.table_border));
        for (i, &w) in col_widths.iter().enumerate() {
            let align = ts.alignments.get(i).copied().unwrap_or(Alignment::None);
            let cell = ts.headers.get(i);
            let cell_len = cell.map_or(0, |c| c.text_len);
            let pad = w.saturating_sub(cell_len);
            let (left_pad, right_pad) = match align {
                Alignment::Right => (pad.saturating_sub(1), 1),
                Alignment::Center => (pad / 2, pad - (pad / 2)),
                _ => (1, pad.saturating_sub(1)),
            };

            head_line.push(Span::raw(" ".repeat(left_pad)));
            if let Some(c) = cell {
                for span in &c.spans {
                    head_line.push(Span::styled(span.content.clone(), theme.table_head));
                }
            }
            head_line.push(Span::raw(" ".repeat(right_pad)));
            head_line.push(Span::styled("│", theme.table_border));
        }
        push_styled_line(theme, out_lines, head_line);

        let mut sep = String::from("├");
        for (i, &w) in col_widths.iter().enumerate() {
            sep.push_str(&"─".repeat(w));
            if i + 1 < col_count {
                sep.push('┼');
            } else {
                sep.push('┤');
            }
        }
        push_styled_line(
            theme,
            out_lines,
            vec![Span::styled(sep, theme.table_border)],
        );
    }

    for row in &ts.rows {
        let mut row_line: Vec<Span<'static>> = Vec::new();
        row_line.push(Span::styled("│", theme.table_border));
        for (i, &w) in col_widths.iter().enumerate() {
            let align = ts.alignments.get(i).copied().unwrap_or(Alignment::None);
            let cell = row.get(i);
            let cell_len = cell.map_or(0, |c| c.text_len);
            let pad = w.saturating_sub(cell_len);
            let (left_pad, right_pad) = match align {
                Alignment::Right => (pad.saturating_sub(1), 1),
                Alignment::Center => (pad / 2, pad - (pad / 2)),
                _ => (1, pad.saturating_sub(1)),
            };

            row_line.push(Span::raw(" ".repeat(left_pad)));
            if let Some(c) = cell {
                for span in &c.spans {
                    row_line.push(span.clone());
                }
            }
            row_line.push(Span::raw(" ".repeat(right_pad)));
            row_line.push(Span::styled("│", theme.table_border));
        }
        push_styled_line(theme, out_lines, row_line);
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
                    let span = Span::styled(t.to_string(), style_mod.to_style(base_style, theme));

                    if let Some(ts) = table_state.as_mut() {
                        ts.current_cell_len += span.content.chars().count();
                        ts.current_cell_spans.push(span);
                    } else {
                        ensure_line_prefix(
                            theme,
                            &mut current_spans,
                            quote_depth,
                            &mut list_stack,
                            &mut item_started,
                        );
                        current_spans.push(span);
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
                    current_spans.push(Span::raw(" "));
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
        let lines = render_markdown(ts_code, &MarkdownTheme::answer());
        let has_highlighting = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                if let Some(Color::Rgb(r, g, b)) = s.style.fg {
                    !(r == 220 && g == 225 && b == 235)
                } else {
                    false
                }
            })
        });
        assert!(has_highlighting, "TypeScript code must have syntax highlighting");

        let code_bg = MarkdownTheme::answer().code_bg;
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
        let lines = render_markdown(md, &MarkdownTheme::answer());
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
        assert_eq!(top_pad.spans[0].style.bg, Some(MarkdownTheme::answer().code_bg));
        assert_eq!(code_line.spans[0].style.bg, Some(MarkdownTheme::answer().code_bg));
        assert_eq!(code_line.spans.last().unwrap().style.bg, Some(MarkdownTheme::answer().code_bg));
        assert_eq!(bot_pad.spans[0].style.bg, Some(MarkdownTheme::answer().code_bg));
    }
}
