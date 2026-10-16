//! Syntax-highlighted code-block rendering.
//!
//! Extracted from the Markdown renderer so the pulldown-cmark event state
//! machine and the table layout stay in focused modules (Conventions.md §3.5).

use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

use super::{
    push_empty_line, push_styled_line, safe_rgb, CodeBlockState, MarkdownTheme, COPY_BUTTON_LABEL,
};

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

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

pub(super) fn render_code_block(
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

    let engine = crate::theme::engine::current_engine();
    let palette = engine.palette();

    if is_streaming {
        header_spans.push(Span::styled("⠋ streaming", theme.code_lang));
    } else if is_flashing {
        header_spans.push(Span::styled(
            " [ Copied ] ",
            engine
                .bg(palette.status_success)
                .fg(engine.color(palette.text_inverse))
                .bold(),
        ));
    } else {
        header_spans.push(Span::styled(
            format!(" {} ", COPY_BUTTON_LABEL),
            engine
                .bg(palette.button_bg)
                .fg(engine.color(palette.button_fg))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::render_markdown;

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
}
