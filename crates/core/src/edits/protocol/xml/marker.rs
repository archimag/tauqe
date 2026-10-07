//! Turn-marker support for the XML edit protocol: marker generation, fuzzy matching
//! and normalization of marker-suffixed tags into the plain dialect.

/// Generates a process-unique alphanumeric marker without an additional dependency.
pub fn generate_turn_marker() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQUENCE: OnceLock<AtomicU64> = OnceLock::new();
    let sequence = SEQUENCE.get_or_init(|| {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        AtomicU64::new(seed)
    });
    format!("{:016X}", sequence.fetch_add(1, Ordering::Relaxed))
}

pub(super) const XML_TAG_BASES: &[&str] = &[
    "tauqe_edits", "edit", "search", "replace", "create", "delete", "move", "with", "summary",
    "context_request", "user_language", "verify", "overwrite",
];

fn literal_prefix(marker: &str) -> String {
    format!("\u{e000}{}\u{e001}", marker)
}

pub(crate) fn restore_xml_literals(text: &str, marker: &str) -> String {
    text.replace(&literal_prefix(marker), "<")
}

/// Case-insensitive marker matching with at most one insertion, deletion or substitution.
/// Base tag names remain exact: fuzziness must not turn arbitrary source tags into edits.
fn marker_matches(candidate: &str, marker: &str) -> bool {
    let a = candidate.to_ascii_lowercase();
    let b = marker.to_ascii_lowercase();
    if a == b { return true; }
    if a.len().abs_diff(b.len()) > 1 || b.len() < 4 { return false; }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j, mut errors) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else {
            errors += 1;
            if errors > 1 { return false; }
            if a.len() >= b.len() { i += 1; }
            if b.len() >= a.len() { j += 1; }
        }
    }
    errors + (a.len() - i) + (b.len() - j) <= 1
}

/// Converts only active structural tags to the legacy parser's dialect, protecting
/// literal '<' characters from its delimiter searches. Used by BOTH final and stream
/// parsing. An incomplete trailing token is withheld so the returned prefix is stable.
/// Unsuffixed tags in source code are protected as literal content, never closing active blocks.
pub(crate) fn normalize_marked_xml(text: &str, marker: &str, finished: bool) -> String {
    let mut output = String::new();
    let mut stack: Vec<&str> = Vec::new();
    let protection = literal_prefix(marker);
    let mut pos = 0;
    while pos < text.len() {
        let Some(offset) = text[pos..].find('<') else {
            output.push_str(&text[pos..]);
            break;
        };
        let start = pos + offset;
        output.push_str(&text[pos..start]);
        let closing = text[start..].starts_with("</");
        let name_start = start + if closing { 2 } else { 1 };
        let name_len = text[name_start..].bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
            .count();
        let name_end = name_start + name_len;
        if name_end == text.len() && !finished { break; }
        let name = &text[name_start..name_end];
        let active = XML_TAG_BASES.iter().copied().find(|base| {
            name.strip_prefix(base)
                .and_then(|rest| rest.strip_prefix('_').or_else(|| rest.strip_prefix('-')))
                .map(|suffix| marker_matches(suffix, marker))
                .unwrap_or(false)
        });
        let boundary = text[name_end..].chars().next()
            .map(|c| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(false);
        if let (true, Some(base)) = (boundary, active) {
            // Find the end of the header without mistaking quoted '>' for its delimiter.
            let mut quote = None;
            let mut end = None;
            for (idx, ch) in text[name_end..].char_indices() {
                if let Some(q) = quote {
                    if ch == q { quote = None; }
                } else if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                } else if ch == '>' {
                    end = Some(name_end + idx + 1);
                    break;
                }
            }
            let Some(end) = end else {
                if !finished { break; }
                output.push_str(&protection);
                pos = start + 1;
                continue;
            };
            let accepted = !closing || stack.last().copied() == Some(base);
            if accepted {
                output.push('<');
                if closing { output.push('/'); }
                output.push_str(base);
                output.push_str(&text[name_end..end]);
                if closing {
                    stack.pop();
                } else if !text[name_end..end].trim_end().ends_with("/>") {
                    stack.push(base);
                }
                pos = end;
                continue;
            }
        }
        output.push_str(&protection);
        pos = start + 1;
    }
    output
}
