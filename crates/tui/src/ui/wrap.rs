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

pub fn wrap_line(line: Line<'static>, max_width: usize) -> Vec<Line<'static>> {
    if max_width == 0 || line.width() <= max_width {
        return vec![line];
    }

    let mut is_bullet_prefix = false;
    let continuation_prefix: Vec<Span<'static>> = if let Some(first_span) = line.spans.first() {
        let is_gutter_prefix = !first_span.content.is_empty()
            && first_span
                .content
                .chars()
                .all(|c| c.is_whitespace() || c == '│' || c == '▌' || c == '|' || c == '║');

        if is_gutter_prefix {
            let mut prefix = vec![first_span.clone()];
            if let Some(second_span) = line.spans.get(1) {
                let is_bullet = second_span.content.chars().all(|c| {
                    c == '•' || c == '◦' || c == '▪' || c == '✔' || c == '☐' || c.is_whitespace()
                }) && second_span.width() <= 4;
                if is_bullet {
                    is_bullet_prefix = true;
                    prefix.push(Span::raw(" ".repeat(second_span.width())));
                }
            }
            prefix
        } else {
            let leading_spaces = first_span.content.chars().take_while(|c| *c == ' ').count();
            if leading_spaces > 0 {
                vec![Span::raw(" ".repeat(leading_spaces.min(max_width / 2)))]
            } else {
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let prefix_width: usize = continuation_prefix.iter().map(|s| s.width()).sum();
    let continuation_prefix = if prefix_width >= max_width {
        Vec::new()
    } else {
        continuation_prefix
    };

    let push_continuation = |current_spans: &mut Vec<Span<'static>>, current_width: &mut usize| {
        for p in &continuation_prefix {
            *current_width += p.width();
            current_spans.push(p.clone());
        }
    };

    let mut wrapped_lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;

    let has_atomic_gutter = !continuation_prefix.is_empty()
        && line.spans.first().is_some_and(|s| {
            !s.content.is_empty()
                && s.content
                    .chars()
                    .all(|c| c.is_whitespace() || c == '│' || c == '▌' || c == '|' || c == '║')
        });

    let skip_count = if has_atomic_gutter {
        if is_bullet_prefix {
            2
        } else {
            1
        }
    } else {
        0
    };

    let mut spans_iter = line.spans.into_iter();
    if skip_count > 0 {
        if let Some(s0) = spans_iter.next() {
            current_width += s0.width();
            current_spans.push(s0);
        }
        if skip_count > 1 {
            if let Some(s1) = spans_iter.next() {
                current_width += s1.width();
                current_spans.push(s1);
            }
        }
    }

    for span in spans_iter {
        let style = span.style;
        let content = span.content.into_owned();
        let tokens = tokenize(&content);

        for (token_text, is_ws) in tokens {
            let token_span = Span::styled(token_text.to_string(), style);
            let token_width = token_span.width();

            if is_ws {
                if current_spans.is_empty() && !wrapped_lines.is_empty() {
                    continue;
                }
                if current_width + token_width <= max_width {
                    current_spans.push(token_span);
                    current_width += token_width;
                } else if !current_spans.is_empty() {
                    wrapped_lines.push(Line::from(std::mem::take(&mut current_spans)));
                    current_width = 0;
                }
            } else {
                if current_spans.is_empty() && !wrapped_lines.is_empty() && !continuation_prefix.is_empty() {
                    push_continuation(&mut current_spans, &mut current_width);
                }

                if current_width + token_width <= max_width {
                    current_spans.push(token_span);
                    current_width += token_width;
                } else {
                    let has_words = current_spans.iter().any(|s| !s.content.trim().is_empty());
                    if has_words {
                        wrapped_lines.push(Line::from(std::mem::take(&mut current_spans)));
                        current_width = 0;
                        if !continuation_prefix.is_empty() {
                            push_continuation(&mut current_spans, &mut current_width);
                        }
                    }

                    if current_width + token_width <= max_width {
                        current_spans.push(token_span);
                        current_width += token_width;
                    } else {
                        // Word is longer than available line width: split character by character
                        let mut sub = String::new();
                        for ch in token_text.chars() {
                            let ch_w = Span::raw(ch.to_string()).width();
                            if current_width + ch_w > max_width && !sub.is_empty() {
                                current_spans.push(Span::styled(std::mem::take(&mut sub), style));
                                wrapped_lines.push(Line::from(std::mem::take(&mut current_spans)));
                                current_width = 0;
                                if !continuation_prefix.is_empty() {
                                    push_continuation(&mut current_spans, &mut current_width);
                                }
                            }
                            sub.push(ch);
                            current_width += ch_w;
                        }
                        if !sub.is_empty() {
                            current_spans.push(Span::styled(sub, style));
                        }
                    }
                }
            }
        }
    }

    if !current_spans.is_empty() {
        let has_content = current_spans.len() > continuation_prefix.len()
            || current_spans.iter().any(|s| {
                !s.content.trim().is_empty()
                    && !s.content.chars().all(|c| c.is_whitespace() || c == '│' || c == '▌' || c == '|' || c == '║')
            });
        if has_content || wrapped_lines.is_empty() {
            wrapped_lines.push(Line::from(current_spans));
        }
    }

    if wrapped_lines.is_empty() {
        vec![Line::raw("")]
    } else {
        wrapped_lines
    }
}

pub fn wrap_lines(lines: Vec<Line<'static>>, max_width: usize) -> Vec<Line<'static>> {
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
        let first_line_text: String = wrapped[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first_line_text.starts_with("    │ • "));
        let second_line_text: String = wrapped[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(second_line_text.starts_with("    │   "));
    }
}
