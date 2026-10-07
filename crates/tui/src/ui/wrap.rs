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

    let indent = if let Some(first_span) = line.spans.first() {
        let leading_spaces = first_span.content.chars().take_while(|c| *c == ' ').count();
        if leading_spaces > 0 {
            " ".repeat(leading_spaces.min(8))
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let mut wrapped_lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;

    for span in line.spans {
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
                if current_spans.is_empty() && !wrapped_lines.is_empty() && !indent.is_empty() {
                    let indent_span = Span::raw(indent.clone());
                    let indent_w = indent_span.width();
                    current_spans.push(indent_span);
                    current_width += indent_w;
                }

                if current_width + token_width <= max_width {
                    current_spans.push(token_span);
                    current_width += token_width;
                } else {
                    let has_words = current_spans.iter().any(|s| !s.content.trim().is_empty());
                    if has_words {
                        wrapped_lines.push(Line::from(std::mem::take(&mut current_spans)));
                        current_width = 0;
                        if !indent.is_empty() {
                            let indent_span = Span::raw(indent.clone());
                            let indent_w = indent_span.width();
                            current_spans.push(indent_span);
                            current_width += indent_w;
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
                                if !indent.is_empty() {
                                    let indent_span = Span::raw(indent.clone());
                                    let indent_w = indent_span.width();
                                    current_spans.push(indent_span);
                                    current_width += indent_w;
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
        let has_content = current_spans.iter().any(|s| !s.content.trim().is_empty());
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
}
