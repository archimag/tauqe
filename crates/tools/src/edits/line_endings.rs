//! The single owner of line-terminator semantics for the edit appliers.
//!
//! Every applier edits text a model authored, and that text carries the line
//! endings the model typed rather than the ones the target file uses. Two
//! questions follow from that, and both of them have to be answered in one
//! place, because an applier that answers them again answers them differently:
//! how authored text converges onto the ending style of the file it edits, and
//! what terminator a fragment needs when it becomes a line of its own.
//!
//! Both answers are plain scans over ASCII delimiters, so they hold for any
//! UTF-8 input without a dependency.
//!
//! What this module deliberately does not own: where the edits come from, which
//! ranges they address, and how the resulting text is spliced into a snapshot.
//! Those belong to the appliers that call it.

use super::fuzzy::LineSpan;

/// Aligns authored text with the line-ending style of the file it edits.
///
/// A CRLF file renders its lines without their terminators, so the text a model
/// writes comes back with bare line feeds and has to be widened to the ending
/// the file already uses. The reverse direction keeps a change set authored with
/// Windows endings byte-consistent inside a Unix file.
///
/// Widening runs per character and widens only a line feed that follows no
/// carriage return, which makes the alignment idempotent and converging: text
/// that already carries CRLF never grows a `\r\r\n`, and text that mixes both
/// endings leaves with one style instead of staying mixed.
pub(crate) fn align_line_endings(text: &str, reference: &str) -> String {
    if reference.contains("\r\n") {
        widen_bare_line_feeds(text)
    } else if text.contains("\r\n") {
        text.replace("\r\n", "\n")
    } else {
        text.to_string()
    }
}

/// Widens every bare line feed of a text to CRLF.
///
/// A line feed that follows a carriage return already terminates its line and is
/// copied as it is, so widening the same text twice adds nothing the second
/// time.
fn widen_bare_line_feeds(text: &str) -> String {
    let mut widened = String::with_capacity(text.len() + text.matches('\n').count());
    let mut previous = None;
    for symbol in text.chars() {
        if symbol == '\n' && previous != Some('\r') {
            widened.push('\r');
        }
        widened.push(symbol);
        previous = Some(symbol);
    }
    widened
}

/// Terminates a fragment that becomes its own line at an insertion point.
///
/// An anchored range that narrows to no line at all is applied as a pure
/// insertion in front of the line that follows the addressed range. A fragment
/// that ends without a newline would therefore concatenate its last line with
/// that surviving line, while the untrimmed replacement of the same edit would
/// have kept the two apart. The fragment borrows the terminator of the last
/// addressed line instead — the very byte the untrimmed replacement would have
/// preserved — so the narrowed application stays byte-identical to the untrimmed
/// one, even when the surviving line itself is unterminated.
///
/// A fragment that is empty, already terminated, or inserted at the end of the
/// file is returned unchanged: it is either a no-op or the new final line, and
/// the file must not gain a terminator it never had.
pub(crate) fn terminate_inserted_text(
    text: &str,
    snapshot: &str,
    last_addressed: &LineSpan,
    inserts_before_a_line: bool,
) -> String {
    if text.is_empty() || text.ends_with('\n') || !inserts_before_a_line {
        return text.to_string();
    }
    let terminator = snapshot
        .get(last_addressed.content_end..last_addressed.end)
        .unwrap_or_default();
    let mut terminated = String::with_capacity(text.len() + terminator.len());
    terminated.push_str(text);
    terminated.push_str(terminator);
    terminated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::fuzzy::scan_lines;

    #[test]
    fn authored_bare_line_feeds_are_widened_to_a_crlf_reference() {
        assert_eq!(
            align_line_endings("alpha\nbeta\n", "x\r\ny\r\n"),
            "alpha\r\nbeta\r\n"
        );
    }

    #[test]
    fn widening_is_idempotent_on_text_that_already_carries_crlf() {
        let widened = align_line_endings("alpha\r\nbeta\r\n", "x\r\n");
        assert_eq!(widened, "alpha\r\nbeta\r\n");
        assert_eq!(align_line_endings(&widened, "x\r\n"), widened);
    }

    #[test]
    fn mixed_endings_converge_to_the_style_of_the_reference() {
        assert_eq!(
            align_line_endings("alpha\r\nbeta\ngamma\r\n", "x\r\n"),
            "alpha\r\nbeta\r\ngamma\r\n"
        );
        assert_eq!(
            align_line_endings("alpha\r\nbeta\ngamma\n", "x\ny\n"),
            "alpha\nbeta\ngamma\n"
        );
    }

    #[test]
    fn a_crlf_text_is_narrowed_to_an_lf_reference() {
        assert_eq!(
            align_line_endings("alpha\r\nbeta\r\n", "x\n"),
            "alpha\nbeta\n"
        );
    }

    #[test]
    fn empty_and_newline_free_texts_are_untouched() {
        assert_eq!(align_line_endings("", "x\r\n"), "");
        assert_eq!(align_line_endings("alpha", "x\r\n"), "alpha");
        assert_eq!(align_line_endings("", "x\n"), "");
    }

    #[test]
    fn every_widened_text_is_free_of_doubled_carriage_returns() {
        for text in ["a\nb", "a\r\nb", "a\r\nb\nc\r\n", "\n\r\n\n"] {
            let widened = align_line_endings(text, "x\r\n");
            assert!(
                !widened.contains("\r\r"),
                "{text:?} widened into {widened:?}"
            );
            assert_eq!(align_line_endings(&widened, "x\r\n"), widened);
        }
    }

    /// Terminates a fragment in front of the line that follows the addressed
    /// range, borrowing the terminator of the span that range ends on.
    fn terminate_at(
        text: &str,
        snapshot: &str,
        addressed_line: usize,
        inserts_before_a_line: bool,
    ) -> String {
        let lines = scan_lines(snapshot);
        let span = lines
            .get(addressed_line)
            .expect("the addressed line exists");
        terminate_inserted_text(text, snapshot, span, inserts_before_a_line)
    }

    #[test]
    fn an_unterminated_fragment_borrows_an_lf_terminator() {
        assert_eq!(
            terminate_at("DELTA", "alpha\nbeta\ngamma\n", 1, true),
            "DELTA\n"
        );
    }

    #[test]
    fn an_unterminated_fragment_borrows_a_crlf_terminator() {
        assert_eq!(
            terminate_at("DELTA", "alpha\r\nbeta\r\ngamma\r\n", 1, true),
            "DELTA\r\n"
        );
    }

    #[test]
    fn terminated_empty_and_final_fragments_are_returned_unchanged() {
        let snapshot = "alpha\nbeta\n";
        assert_eq!(terminate_at("DELTA\n", snapshot, 0, true), "DELTA\n");
        assert_eq!(terminate_at("", snapshot, 0, true), "");
        assert_eq!(terminate_at("DELTA", snapshot, 1, false), "DELTA");
        // The unterminated final line has no terminator to lend.
        assert_eq!(terminate_at("DELTA", "alpha\nbeta", 1, true), "DELTA");
    }
}
