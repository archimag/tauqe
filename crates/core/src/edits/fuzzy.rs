//! Line-based fuzzy replacement and indentation alignment.
//!
//! A search block reaches these routines only after an exact match fails. They
//! re-anchor a pattern whose trailing whitespace, blank-line padding or leading
//! indentation drifted from the source, and report ambiguity rather than
//! guessing. Keeping this logic in one module makes the tolerance rules easy to
//! audit: a unique exact match always wins over any fuzzy one.

use super::EditError;

#[derive(Debug, Clone, Copy)]
struct LineSpan {
    start: usize,
    content_end: usize,
    end: usize,
}

fn scan_lines(text: &str) -> Vec<LineSpan> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    let len = bytes.len();

    while start < len {
        let mut i = start;
        while i < len && bytes[i] != b'\n' {
            i += 1;
        }
        if i < len {
            let end = i + 1;
            let content_end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            lines.push(LineSpan { start, content_end, end });
            start = end;
        } else {
            lines.push(LineSpan { start, content_end: len, end: len });
            break;
        }
    }
    lines
}

fn leading_whitespace(s: &str) -> &str {
    let non_ws = s.find(|c: char| c != ' ' && c != '\t').unwrap_or(s.len());
    &s[..non_ws]
}

enum IndentShift {
    Add(String),
    Strip(String),
    None,
}

fn compute_indent_shift(
    current: &str,
    pattern: &str,
    file_spans: &[LineSpan],
    pattern_spans: &[LineSpan],
) -> Option<IndentShift> {
    let mut first_non_empty = None;
    for (k, (f_span, p_span)) in file_spans.iter().zip(pattern_spans.iter()).enumerate() {
        let f_line = &current[f_span.start..f_span.content_end];
        let p_line = &pattern[p_span.start..p_span.content_end];
        if !f_line.trim().is_empty() {
            first_non_empty = Some((k, f_line, p_line));
            break;
        }
    }

    let (_, f_first, p_first) = first_non_empty?;
    let f_ws = leading_whitespace(f_first);
    let p_ws = leading_whitespace(p_first);

    let shift = if f_ws == p_ws {
        IndentShift::None
    } else if let Some(prefix) = f_ws.strip_suffix(p_ws) {
        IndentShift::Add(prefix.to_string())
    } else {
        let prefix = p_ws.strip_suffix(f_ws)?;
        IndentShift::Strip(prefix.to_string())
    };

    for (f_span, p_span) in file_spans.iter().zip(pattern_spans.iter()) {
        let f_line = &current[f_span.start..f_span.content_end];
        let p_line = &pattern[p_span.start..p_span.content_end];
        if f_line.trim().is_empty() && p_line.trim().is_empty() {
            continue;
        }
        let cur_f_ws = leading_whitespace(f_line);
        let cur_p_ws = leading_whitespace(p_line);
        match &shift {
            IndentShift::None => {
                if cur_f_ws != cur_p_ws {
                    return None;
                }
            }
            IndentShift::Add(prefix) => {
                if !cur_f_ws.starts_with(prefix) || &cur_f_ws[prefix.len()..] != cur_p_ws {
                    return None;
                }
            }
            IndentShift::Strip(prefix) => {
                if !cur_p_ws.starts_with(prefix) || &cur_p_ws[prefix.len()..] != cur_f_ws {
                    return None;
                }
            }
        }
    }

    Some(shift)
}

fn apply_indent_shift(text: &str, shift: &IndentShift) -> String {
    match shift {
        IndentShift::None => text.to_string(),
        IndentShift::Add(prefix) => {
            let spans = scan_lines(text);
            let mut result = String::with_capacity(text.len() + spans.len() * prefix.len());
            for span in &spans {
                let content = &text[span.start..span.content_end];
                let line_ending = &text[span.content_end..span.end];
                if !content.trim().is_empty() {
                    result.push_str(prefix);
                }
                result.push_str(content);
                result.push_str(line_ending);
            }
            result
        }
        IndentShift::Strip(prefix) => {
            let spans = scan_lines(text);
            let mut result = String::with_capacity(text.len());
            for span in &spans {
                let content = &text[span.start..span.content_end];
                let line_ending = &text[span.content_end..span.end];
                let stripped = content.strip_prefix(prefix).unwrap_or(content);
                result.push_str(stripped);
                result.push_str(line_ending);
            }
            result
        }
    }
}

fn try_fuzzy_replace(
    current: &str,
    effective_old: &str,
    effective_new: &str,
    path: &str,
) -> Result<Option<String>, EditError> {
    if effective_old.trim().is_empty() {
        return Ok(None);
    }

    let file_lines = scan_lines(current);
    let pattern_lines = scan_lines(effective_old);

    if pattern_lines.is_empty() || pattern_lines.len() > file_lines.len() {
        return Ok(None);
    }

    let p_len = pattern_lines.len();
    let old_ends_with_newline = effective_old.ends_with('\n');

    // Compute the highest valid start index once. The guard above already
    // ensures `file_lines.len() >= p_len`, but a `checked_sub` keeps the
    // subtraction underflow-safe if that guard is ever relaxed.
    let Some(max_start) = file_lines.len().checked_sub(p_len) else {
        return Ok(None);
    };

    // 1. Level 1: Trim-End matching (trailing whitespace and empty line normalization)
    let mut trim_end_matches = Vec::new();
    for i in 0..=max_start {
        let mut matched = true;
        for k in 0..p_len {
            let file_line = &current[file_lines[i + k].start..file_lines[i + k].content_end];
            let pat_line = &effective_old[pattern_lines[k].start..pattern_lines[k].content_end];
            if file_line.trim_end() != pat_line.trim_end() {
                matched = false;
                break;
            }
        }
        if matched {
            trim_end_matches.push(i);
        }
    }

    if trim_end_matches.len() > 1 {
        return Err(EditError::AmbiguousMatch {
            path: path.to_string(),
            count: trim_end_matches.len(),
        });
    }

    if trim_end_matches.len() == 1 {
        let idx = trim_end_matches[0];
        let start_byte = file_lines[idx].start;
        let end_byte = if old_ends_with_newline {
            file_lines[idx + p_len - 1].end
        } else {
            file_lines[idx + p_len - 1].content_end
        };
        let mut updated = String::with_capacity(
            current.len() + effective_new.len().saturating_sub(end_byte - start_byte),
        );
        updated.push_str(&current[..start_byte]);
        updated.push_str(effective_new);
        updated.push_str(&current[end_byte..]);
        return Ok(Some(updated));
    }

    // 2. Level 2: Indentation shift matching
    let mut indent_matches = Vec::new();
    for i in 0..=max_start {
        let mut matched = true;
        for k in 0..p_len {
            let file_line = &current[file_lines[i + k].start..file_lines[i + k].content_end];
            let pat_line = &effective_old[pattern_lines[k].start..pattern_lines[k].content_end];
            if file_line.trim() != pat_line.trim() {
                matched = false;
                break;
            }
        }
        if !matched {
            continue;
        }

        if let Some(shift) = compute_indent_shift(current, effective_old, &file_lines[i..i + p_len], &pattern_lines) {
            indent_matches.push((i, shift));
        }
    }

    if indent_matches.len() > 1 {
        return Err(EditError::AmbiguousMatch {
            path: path.to_string(),
            count: indent_matches.len(),
        });
    }

    if indent_matches.len() == 1 {
        let (idx, shift) = &indent_matches[0];
        let start_byte = file_lines[*idx].start;
        let end_byte = if old_ends_with_newline {
            file_lines[*idx + p_len - 1].end
        } else {
            file_lines[*idx + p_len - 1].content_end
        };

        let adjusted_new = apply_indent_shift(effective_new, shift);
        let mut updated = String::with_capacity(
            current.len() + adjusted_new.len().saturating_sub(end_byte - start_byte),
        );
        updated.push_str(&current[..start_byte]);
        updated.push_str(&adjusted_new);
        updated.push_str(&current[end_byte..]);
        return Ok(Some(updated));
    }

    Ok(None)
}

pub(crate) fn apply_replace(
    current: &str,
    old_text: &str,
    new_text: &str,
    path: &str,
) -> Result<String, EditError> {
    let (effective_old, effective_new) =
        if current.contains("\r\n") && !old_text.contains("\r\n") {
            (old_text.replace('\n', "\r\n"), new_text.replace('\n', "\r\n"))
        } else if !current.contains("\r\n") && old_text.contains("\r\n") {
            (old_text.replace("\r\n", "\n"), new_text.replace("\r\n", "\n"))
        } else {
            (old_text.to_string(), new_text.to_string())
        };

    // 1. Exact match (fast path)
    let matches: Vec<(usize, &str)> = current.match_indices(&effective_old).collect();
    if matches.len() == 1 {
        let match_idx = matches[0].0;
        let mut updated = String::with_capacity(
            current.len() + effective_new.len().saturating_sub(effective_old.len()),
        );
        updated.push_str(&current[..match_idx]);
        updated.push_str(&effective_new);
        updated.push_str(&current[match_idx + effective_old.len()..]);
        return Ok(updated);
    }
    if matches.len() > 1 {
        return Err(EditError::AmbiguousMatch {
            path: path.to_string(),
            count: matches.len(),
        });
    }

    // 2. Fuzzy match fallback
    if let Some(updated) = try_fuzzy_replace(current, &effective_old, &effective_new, path)? {
        return Ok(updated);
    }

    Err(EditError::NoMatch {
        path: path.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_replace_tolerates_trailing_whitespace_and_blank_line_spaces() {
        let current = "fn foo() {   \n    \n    bar();\n}\n";
        let old = "fn foo() {\n\n    bar();\n}\n";
        let new = "fn foo() {\n    baz();\n}\n";
        let updated = apply_replace(current, old, new, "code.rs").unwrap();
        assert_eq!(updated, "fn foo() {\n    baz();\n}\n");
    }

    #[test]
    fn fuzzy_replace_adjusts_leading_indentation_shift() {
        let current = "impl Test {\n        fn run() {\n            let a = 1;\n            let b = 2;\n        }\n}\n";
        let old = "fn run() {\n    let a = 1;\n    let b = 2;\n}\n";
        let new = "fn run() {\n    let a = 1;\n    let b = 42;\n}\n";
        let updated = apply_replace(current, old, new, "indent.rs").unwrap();
        assert_eq!(
            updated,
            "impl Test {\n        fn run() {\n            let a = 1;\n            let b = 42;\n        }\n}\n"
        );
    }

    #[test]
    fn fuzzy_replace_rejects_ambiguous_matches() {
        let current = "fn a() {\n    foo();\n}\nfn b() {\n    foo();\n}\n";
        let result = apply_replace(current, "    foo();  \n", "    bar();\n", "dup.rs");
        assert!(matches!(result, Err(EditError::AmbiguousMatch { count: 2, .. })));
    }
}
