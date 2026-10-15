//! Turn-marker support for the XML edit protocol: unpredictable marker
//! generation, exact marker matching, and normalization of marker-suffixed tags
//! into the plain dialect.

/// Generates a process-unique, unpredictable alphanumeric turn marker.
///
/// The marker binds structural protocol tags to a single turn so that literal
/// source tags are never mistaken for protocol commands. That guarantee holds
/// only while the marker cannot be guessed: repository content, including a
/// previous turn's transcript, must not be able to pre-embed the next marker and
/// forge protocol tags. The value is therefore drawn from the operating system
/// CSPRNG instead of a time-seeded counter, which was monotonic and guessable.
pub fn generate_turn_marker() -> String {
    let mut bytes = [0u8; 8];
    match getrandom::getrandom(&mut bytes) {
        Ok(()) => format!("{:016X}", u64::from_be_bytes(bytes)),
        Err(err) => {
            // CSPRNG failure is not expected on supported platforms. Degrade to a
            // time-seeded marker rather than panicking, so a turn can still run.
            tracing::warn!(
                "OS random number generator unavailable ({}); falling back to a time-seeded turn marker",
                err
            );
            fallback_turn_marker()
        }
    }
}

/// Produces a time-seeded, monotonic fallback marker.
///
/// Used only when the OS CSPRNG is unavailable. It is weaker than the primary
/// path, but keeps the daemon alive instead of aborting the turn.
fn fallback_turn_marker() -> String {
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
    "context_request", "context_drop", "user_language", "verify", "overwrite", "doc_request",
    "plan", "concept", "concept_item", "item", "description", "details", "title",
    "scratchpad", "success", "error", "blocked",
];

const PLAN_CHILD_TAGS: &[&str] = &[
    "concept", "concept_item", "item", "description", "details", "title", "summary",
];

fn literal_prefix(marker: &str) -> String {
    format!("\u{e000}{}\u{e001}", marker)
}

pub(crate) fn restore_xml_literals(text: &str, marker: &str) -> String {
    text.replace(&literal_prefix(marker), "<")
}

/// Case-insensitive exact marker matching.
///
/// The marker suffix must equal the active marker exactly, modulo ASCII case,
/// which does not reduce the entropy an attacker must guess. Tolerating
/// near-misses (an edit distance of one) would let content that merely resembles
/// the marker turn literal source tags into protocol commands, weakening the
/// anti-injection guarantee.
fn marker_matches(candidate: &str, marker: &str) -> bool {
    candidate.eq_ignore_ascii_case(marker)
}

/// Converts only active structural tags to the legacy parser's dialect, protecting
/// literal '<' characters from its delimiter searches. Used by BOTH final and stream
/// parsing. An incomplete trailing token is withheld so the returned prefix is stable.
/// Unsuffixed tags in source code are protected as literal content, never closing active blocks.
pub(crate) fn normalize_marked_xml(text: &str, marker: &str, finished: bool) -> String {
    let mut raw = text.to_string();
    let mut stack = Vec::new();
    normalize_marked_xml_incremental(&mut raw, marker, &mut stack, finished)
}

/// Incrementally normalizes raw XML input in place, draining fully processed bytes
/// from `raw` and returning only newly produced normalized output. Retaining the tag
/// `stack` across chunks avoids O(n^2) rescanning during streaming.
pub(crate) fn normalize_marked_xml_incremental(
    raw: &mut String,
    marker: &str,
    stack: &mut Vec<&'static str>,
    finished: bool,
) -> String {
    let mut output = String::new();
    let protection = literal_prefix(marker);
    let mut pos = 0;
    while pos < raw.len() {
        let Some(offset) = raw[pos..].find('<') else {
            output.push_str(&raw[pos..]);
            pos = raw.len();
            break;
        };
        let start = pos + offset;
        output.push_str(&raw[pos..start]);
        let closing = raw[start..].starts_with("</");
        let name_start = start + if closing { 2 } else { 1 };
        let name_len = raw[name_start..].bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
            .count();
        let name_end = name_start + name_len;
        if name_end == raw.len() && !finished {
            pos = start;
            break;
        }
        let name = &raw[name_start..name_end];
        let mut active = XML_TAG_BASES.iter().copied().find(|base| {
            name.strip_prefix(base)
                .and_then(|rest| rest.strip_prefix('_').or_else(|| rest.strip_prefix('-')))
                .map(|suffix| marker_matches(suffix, marker))
                .unwrap_or(false)
        });
        if active.is_none()
            && stack
                .iter()
                .any(|s| *s == "plan" || *s == "concept" || *s == "concept_item")
        {
            if let Some(base) = PLAN_CHILD_TAGS.iter().copied().find(|&base| name == base) {
                active = Some(base);
            }
        }
        let boundary = raw[name_end..].chars().next()
            .map(|c| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(false);
        if let (true, Some(base)) = (boundary, active) {
            // Find the end of the header without mistaking quoted '>' for its delimiter.
            let mut quote = None;
            let mut end = None;
            for (idx, ch) in raw[name_end..].char_indices() {
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
                if !finished {
                    pos = start;
                    break;
                }
                output.push_str(&protection);
                pos = start + 1;
                continue;
            };
            let accepted = if closing {
                if let Some(p) = stack.iter().rposition(|&s| s == base) {
                    stack.truncate(p);
                    true
                } else {
                    false
                }
            } else {
                true
            };
            if accepted {
                output.push('<');
                if closing { output.push('/'); }
                output.push_str(base);
                output.push_str(&raw[name_end..end]);
                if !closing && !raw[name_end..end].trim_end().ends_with("/>") {
                    stack.push(base);
                }
                pos = end;
                continue;
            }
        }
        output.push_str(&protection);
        pos = start + 1;
    }
    raw.drain(..pos);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_marked_plan_with_unsuffixed_items() {
        let marker = "M123456";
        let raw = format!(
            "Some conversational text\n\
            <plan_{m} action=\"update\" id=\"my-plan\">\n\
              <item id=\"2\" status=\"todo\" />\n\
            </plan_{m}>\n\
            Trailing text",
            m = marker
        );
        let normalized = normalize_marked_xml(&raw, marker, true);
        assert!(normalized.contains("<plan action=\"update\" id=\"my-plan\">"));
        assert!(normalized.contains("<item id=\"2\" status=\"todo\" />"));
        assert!(normalized.contains("</plan>"));
    }

    #[test]
    fn test_normalize_marked_plan_with_concept_and_items() {
        let marker = "M123456";
        let raw = format!(
            "<plan_{m} action=\"save\" id=\"concept-plan\">\n\
              <concept_{m}>\n\
                <concept_item_{m} id=\"c-1\" title=\"Core Invariant\" />\n\
              </concept_{m}>\n\
              <item id=\"1\" title=\"Step\" status=\"todo\" />\n\
            </plan_{m}>",
            m = marker
        );
        let normalized = normalize_marked_xml(&raw, marker, true);
        assert!(normalized.contains("<plan action=\"save\" id=\"concept-plan\">"));
        assert!(normalized.contains("<concept>"));
        assert!(normalized.contains("<concept_item id=\"c-1\" title=\"Core Invariant\" />"));
        assert!(normalized.contains("</concept>"));
        assert!(normalized.contains("<item id=\"1\" title=\"Step\" status=\"todo\" />"));
        assert!(normalized.contains("</plan>"));
    }

    #[test]
    fn test_generate_turn_marker_is_alphanumeric_and_unique() {
        let first = generate_turn_marker();
        let second = generate_turn_marker();
        assert!(!first.is_empty());
        assert!(first.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_ne!(first, second);
    }

    #[test]
    fn test_marker_matches_is_exact_and_case_insensitive() {
        assert!(marker_matches("K7Q2ZX", "K7Q2ZX"));
        assert!(marker_matches("k7q2zx", "K7Q2ZX"));
        assert!(marker_matches("K7Q2ZX", "k7q2zx"));
        // A single-character difference must not match: near-misses would let
        // resembling content forge marker-bound tags.
        assert!(!marker_matches("K7Q2ZY", "K7Q2ZX"));
        assert!(!marker_matches("K7Q2Z", "K7Q2ZX"));
        assert!(!marker_matches("K7Q2ZXX", "K7Q2ZX"));
        assert!(!marker_matches("", "K7Q2ZX"));
    }
}
