use std::borrow::Cow;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

fn tokenize(s: &str) -> Vec<(&str, bool)> {
    let mut tokens = Vec::new();
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    if chars.is_empty() {
        return tokens;
    }
    let mut i = 0;
    while i < chars.len() {
        let is_ws = chars[i].1.is_whitespace();
        let start = chars[i].0;
        while i < chars.len() && chars[i].1.is_whitespace() == is_ws {
            i += 1;
        }
        let end = if i < chars.len() { chars[i].0 } else { s.len() };
        tokens.push((&s[start..end], is_ws));
    }
    tokens
}

/// Materializes a borrowed line into an owned one.
///
/// The wrapping routines accept borrowed input while their output stays
/// `Line<'static>`, so a caller can wrap a body it still owns — a confirmation
/// card budgets its height from the wrapped rows and then draws the very same
/// lines.
fn into_static_line(line: Line<'_>) -> Line<'static> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .into_iter()
            .map(|span| Span::styled(span.content.into_owned(), span.style))
            .collect(),
    }
}

/// Whether `c` belongs to the visual frame of a body rather than to its text.
///
/// A run of these characters is reproduced as a hanging indent when a line
/// wraps, and a line that carries nothing else never counts as content.
fn is_gutter_or_whitespace(c: char) -> bool {
    c.is_whitespace() || matches!(c, '│' | '▌' | '|' | '║')
}

/// Whether `content` is a non-empty run of gutter and whitespace characters.
fn is_gutter_only(content: &str) -> bool {
    !content.is_empty() && content.chars().all(is_gutter_or_whitespace)
}

/// What the wrap walk already knows about one span it appends.
///
/// The walk classifies every span once, so the rendering sink and the counting
/// sink agree on what counts as a word and as visible content without either of
/// them re-scanning the text.
#[derive(Debug, Clone, Copy)]
struct SpanFacts {
    /// The span holds a non-whitespace character.
    has_word: bool,
    /// The span carries more than whitespace or gutter glyphs.
    visible: bool,
}

impl SpanFacts {
    /// Classifies the content of one span.
    fn of(content: &str) -> Self {
        let has_word = !content.trim().is_empty();
        Self {
            has_word,
            visible: has_word && content.chars().any(|c| !is_gutter_or_whitespace(c)),
        }
    }
}

/// Receives the physical lines the wrap walk produces.
///
/// The walk in [`walk_wrapped_line`] owns every break decision, so a sink only
/// accumulates what it is handed. A rendering sink materializes owned lines,
/// while a counting sink reproduces the identical number of physical lines
/// without allocating one.
trait WrapSink {
    /// Appends one span to the current physical line.
    fn push_span(&mut self, content: &str, style: Style, facts: SpanFacts);

    /// Seals the current physical line and opens the next one.
    fn break_line(&mut self);

    /// Whether the current physical line already holds a non-blank word.
    fn has_words(&self) -> bool;

    /// Number of spans on the current physical line.
    fn line_spans(&self) -> usize;

    /// Whether the current physical line carries visible content.
    fn line_has_visible_content(&self) -> bool;

    /// Number of physical lines sealed so far.
    fn sealed_lines(&self) -> usize;
}

/// Rendering sink: materializes the wrapped lines as owned spans.
#[derive(Default)]
struct LineSink {
    /// Physical lines sealed so far.
    lines: Vec<Line<'static>>,
    /// Spans of the line being built.
    current: Vec<Span<'static>>,
    /// Whether the current line already holds a non-blank word.
    has_word: bool,
    /// Whether the current line carries more than a gutter.
    visible: bool,
}

impl WrapSink for LineSink {
    fn push_span(&mut self, content: &str, style: Style, facts: SpanFacts) {
        self.has_word |= facts.has_word;
        self.visible |= facts.visible;
        self.current.push(Span::styled(content.to_string(), style));
    }

    fn break_line(&mut self) {
        self.lines
            .push(Line::from(std::mem::take(&mut self.current)));
        self.has_word = false;
        self.visible = false;
    }

    fn has_words(&self) -> bool {
        self.has_word
    }

    fn line_spans(&self) -> usize {
        self.current.len()
    }

    fn line_has_visible_content(&self) -> bool {
        self.visible
    }

    fn sealed_lines(&self) -> usize {
        self.lines.len()
    }
}

/// Counting sink: reproduces the break decisions without materializing a line.
#[derive(Default)]
struct CountingSink {
    /// Physical lines sealed so far.
    lines: usize,
    /// Number of spans on the line being built.
    spans: usize,
    /// Whether the current line already holds a non-blank word.
    has_word: bool,
    /// Whether the current line carries more than a gutter.
    visible: bool,
}

impl WrapSink for CountingSink {
    fn push_span(&mut self, _content: &str, _style: Style, facts: SpanFacts) {
        self.spans += 1;
        self.has_word |= facts.has_word;
        self.visible |= facts.visible;
    }

    fn break_line(&mut self) {
        self.lines += 1;
        self.spans = 0;
        self.has_word = false;
        self.visible = false;
    }

    fn has_words(&self) -> bool {
        self.has_word
    }

    fn line_spans(&self) -> usize {
        self.spans
    }

    fn line_has_visible_content(&self) -> bool {
        self.visible
    }

    fn sealed_lines(&self) -> usize {
        self.lines
    }
}

/// Appends the continuation prefix in front of a wrapped line of `sink`.
fn push_continuation<S: WrapSink>(
    sink: &mut S,
    prefix: &[(Cow<'_, str>, Style)],
    width: &mut usize,
) {
    for (content, style) in prefix {
        let content = content.as_ref();
        *width += Span::raw(content).width();
        sink.push_span(content, *style, SpanFacts::of(content));
    }
}

/// Reproduces the wrap of one line into `sink`.
///
/// The walk owns every break decision — token widths, the continuation prefix,
/// and the character-by-character split of a word wider than the line — and
/// hands the sink only the spans it decides to append. A rendering sink
/// materializes them; a counting sink reproduces the identical number of
/// physical lines without allocating an owned line.
fn walk_wrapped_line<'a, S: WrapSink>(line: &'a Line<'_>, max_width: usize, mut sink: S) -> S {
    let mut is_bullet_prefix = false;
    let mut continuation_prefix: Vec<(Cow<'a, str>, Style)> = Vec::new();
    if let Some(first_span) = line.spans.first() {
        if is_gutter_only(&first_span.content) {
            continuation_prefix
                .push((Cow::Borrowed(first_span.content.as_ref()), first_span.style));
            if let Some(second_span) = line.spans.get(1) {
                let is_bullet = second_span.content.chars().all(|c| {
                    c == '•' || c == '◦' || c == '▪' || c == '✔' || c == '☐' || c.is_whitespace()
                }) && second_span.width() <= 4;
                if is_bullet {
                    is_bullet_prefix = true;
                    continuation_prefix.push((
                        Cow::Owned(" ".repeat(second_span.width())),
                        Style::default(),
                    ));
                }
            }
        } else {
            let leading_spaces = first_span.content.chars().take_while(|c| *c == ' ').count();
            if leading_spaces > 0 {
                continuation_prefix.push((
                    Cow::Owned(" ".repeat(leading_spaces.min(max_width / 2))),
                    Style::default(),
                ));
            }
        }
    }

    let mut prefix_width = 0usize;
    for (content, _) in &continuation_prefix {
        prefix_width += Span::raw(content.as_ref()).width();
    }
    // A prefix that consumes the whole line width leaves nothing to wrap into,
    // so it is dropped and the row wraps without a hanging indent.
    if prefix_width >= max_width {
        continuation_prefix.clear();
    }

    let has_atomic_gutter = !continuation_prefix.is_empty()
        && line
            .spans
            .first()
            .is_some_and(|span| is_gutter_only(&span.content));

    let skip_count = if has_atomic_gutter {
        if is_bullet_prefix {
            2
        } else {
            1
        }
    } else {
        0
    };

    let mut current_width = 0usize;
    let mut spans = line.spans.iter();
    if skip_count > 0 {
        if let Some(span) = spans.next() {
            current_width += span.width();
            sink.push_span(
                span.content.as_ref(),
                span.style,
                SpanFacts::of(span.content.as_ref()),
            );
        }
        if skip_count > 1 {
            if let Some(span) = spans.next() {
                current_width += span.width();
                sink.push_span(
                    span.content.as_ref(),
                    span.style,
                    SpanFacts::of(span.content.as_ref()),
                );
            }
        }
    }

    for span in spans {
        let style = span.style;
        for (token, is_whitespace) in tokenize(span.content.as_ref()) {
            let token_width = Span::raw(token).width();

            if is_whitespace {
                if sink.line_spans() == 0 && sink.sealed_lines() > 0 {
                    continue;
                }
                if current_width + token_width <= max_width {
                    sink.push_span(token, style, SpanFacts::of(token));
                    current_width += token_width;
                } else if sink.line_spans() > 0 {
                    sink.break_line();
                    current_width = 0;
                }
                continue;
            }

            if sink.line_spans() == 0 && sink.sealed_lines() > 0 && !continuation_prefix.is_empty()
            {
                push_continuation(&mut sink, &continuation_prefix, &mut current_width);
            }

            if current_width + token_width <= max_width {
                sink.push_span(token, style, SpanFacts::of(token));
                current_width += token_width;
                continue;
            }

            if sink.has_words() {
                sink.break_line();
                current_width = 0;
                if !continuation_prefix.is_empty() {
                    push_continuation(&mut sink, &continuation_prefix, &mut current_width);
                }
            }

            if current_width + token_width <= max_width {
                sink.push_span(token, style, SpanFacts::of(token));
                current_width += token_width;
                continue;
            }

            // Word is longer than available line width: split character by character
            let mut chunk_start = 0usize;
            let mut chunk_chars = 0usize;
            for (offset, ch) in token.char_indices() {
                let ch_width = Span::raw(ch.to_string()).width();
                if current_width + ch_width > max_width && chunk_chars > 0 {
                    let chunk = &token[chunk_start..offset];
                    sink.push_span(chunk, style, SpanFacts::of(chunk));
                    sink.break_line();
                    current_width = 0;
                    if !continuation_prefix.is_empty() {
                        push_continuation(&mut sink, &continuation_prefix, &mut current_width);
                    }
                    chunk_start = offset;
                    chunk_chars = 0;
                }
                chunk_chars += 1;
                current_width += ch_width;
            }
            if chunk_chars > 0 {
                let chunk = &token[chunk_start..];
                sink.push_span(chunk, style, SpanFacts::of(chunk));
            }
        }
    }

    if sink.line_spans() > 0 {
        let has_content =
            sink.line_spans() > continuation_prefix.len() || sink.line_has_visible_content();
        if has_content || sink.sealed_lines() == 0 {
            sink.break_line();
        }
    }

    sink
}

pub fn wrap_line(line: Line<'_>, max_width: usize) -> Vec<Line<'static>> {
    if max_width == 0 || line.width() <= max_width {
        return vec![into_static_line(line)];
    }

    let rendered = walk_wrapped_line(&line, max_width, LineSink::default());
    if rendered.lines.is_empty() {
        vec![Line::raw("")]
    } else {
        rendered.lines
    }
}

/// Number of physical lines `line` occupies when wrapped to `max_width`.
///
/// This is the borrowed counterpart of [`wrap_line`]: it runs the same break
/// decisions through a counting sink, so a caller that only needs the height of
/// a line — measuring a tree row before scrolling to it — never allocates the
/// owned lines it would immediately discard.
pub fn wrapped_line_count(line: &Line<'_>, max_width: usize) -> usize {
    if max_width == 0 || line.width() <= max_width {
        return 1;
    }

    walk_wrapped_line(line, max_width, CountingSink::default())
        .lines
        .max(1)
}

pub fn wrap_lines(lines: Vec<Line<'_>>, max_width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        out.extend(wrap_line(line, max_width));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_lines_basic() {
        let line = Line::raw("Hello world from Tauqe AI system");
        let wrapped = wrap_line(line, 15);
        assert!(wrapped.len() > 1);
        for l in &wrapped {
            assert!(l.width() <= 15);
        }
    }

    #[test]
    fn test_wrap_line_long_word() {
        let line = Line::raw("Supercalifragilisticexpialidocious");
        let wrapped = wrap_line(line, 10);
        assert!(wrapped.len() >= 3);
        for l in &wrapped {
            assert!(l.width() <= 10);
        }
    }

    #[test]
    fn test_wrap_line_preserves_gutter_prefix() {
        let line = Line::from(vec![
            Span::raw("    │ "),
            Span::raw("This is a long description line that must wrap nicely across columns"),
        ]);
        let wrapped = wrap_line(line, 35);
        assert!(wrapped.len() > 1);
        for l in &wrapped {
            assert!(l.width() <= 35);
            let first = l.spans.first().map(|s| s.content.as_ref()).unwrap_or("");
            assert_eq!(first, "    │ ");
        }
    }

    #[test]
    fn test_wrap_line_hanging_indent_for_bullet() {
        let line = Line::from(vec![
            Span::raw("    │ "),
            Span::raw("• "),
            Span::raw("Bullet list item text that is very long and wraps to subsequent lines"),
        ]);
        let wrapped = wrap_line(line, 35);
        assert!(wrapped.len() > 1);
        let first_line_text: String = wrapped[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(first_line_text.starts_with("    │ • "));
        let second_line_text: String = wrapped[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(second_line_text.starts_with("    │   "));
    }
    /// The borrowed counter must reproduce the number of lines the rendering
    /// walk produces for every shape the wrapper special-cases: a plain
    /// paragraph, an over-long word, a gutter-prefixed body with a hanging
    /// bullet indent, a line that already fits, and a zero wrap width.
    #[test]
    fn test_wrapped_line_count_matches_rendered_line_count() {
        let cases: Vec<(Line<'static>, usize)> = vec![
            (Line::raw("Hello world from Tauqe AI system"), 15),
            (Line::raw("Supercalifragilisticexpialidocious"), 10),
            (
                Line::from(vec![
                    Span::raw("    │ "),
                    Span::raw(
                        "This is a long description line that must wrap nicely across columns",
                    ),
                ]),
                35,
            ),
            (
                Line::from(vec![
                    Span::raw("    │ "),
                    Span::raw("• "),
                    Span::raw(
                        "Bullet list item text that is very long and wraps to subsequent lines",
                    ),
                ]),
                35,
            ),
            (Line::raw("Fits already"), 40),
            (Line::raw("Zero width"), 0),
            (
                Line::from(vec![
                    Span::raw("  "),
                    Span::raw("     "),
                    Span::raw("Wide gutter prefix"),
                ]),
                8,
            ),
        ];

        for (line, width) in cases {
            assert_eq!(
                wrapped_line_count(&line, width),
                wrap_line(line.clone(), width).len(),
                "the measured count must match the rendered lines of {line:?} at {width}"
            );
        }
    }
}
