//! Bottom-to-top application of anchored line-range edits.
//!
//! A change set that uses opaque line anchors names the lines it replaces
//! instead of echoing their text, so the engine resolves both anchors of every
//! range back into line indices, validates the ranges, and applies them to the
//! in-memory snapshot. The ranges are applied from the bottom of the file
//! upwards: replacing a lower range first leaves the byte offsets of every
//! higher range exactly where the snapshot put them, so no cumulative line shift
//! is ever computed.

use super::anchor::decode_anchor;
use super::fuzzy::scan_lines;
use super::EditError;

/// One replacement of an inclusive line range, named by opaque anchors.
pub(crate) struct AnchorEdit<'a> {
    /// Anchor of the first replaced line.
    pub start: &'a str,
    /// Anchor of the last replaced line.
    pub end: &'a str,
    /// Complete replacement text of the addressed lines.
    pub content: &'a str,
}

/// One validated range, resolved to the byte span of the snapshot.
struct ResolvedRange<'a> {
    start_line: usize,
    end_line: usize,
    start_byte: usize,
    end_byte: usize,
    text: String,
    start_anchor: &'a str,
    end_anchor: &'a str,
}

/// Applies anchored line-range replacements to a file snapshot.
///
/// The anchors name the lines the context block rendered, so they are resolved
/// against the snapshot as a whole before a single byte moves: a range must stay
/// inside the file, must not run backwards, and no two ranges may overlap. The
/// validated ranges are then applied from the bottom of the file upwards, which
/// keeps the offsets of every range above a replacement untouched.
pub(crate) fn apply_anchor_edits(
    current: &str,
    edits: &[AnchorEdit<'_>],
    path: &str,
) -> Result<String, EditError> {
    if edits.is_empty() {
        return Ok(current.to_string());
    }

    let lines = scan_lines(current);
    let mut ranges = Vec::with_capacity(edits.len());
    for edit in edits {
        let start_line = decode_line(edit.start, path)?;
        let end_line = decode_line(edit.end, path)?;
        if start_line > end_line {
            return Err(EditError::ReversedAnchorRange {
                path: path.to_string(),
                start: edit.start.to_string(),
                end: edit.end.to_string(),
            });
        }

        let start_span = lines
            .get(start_line)
            .ok_or_else(|| EditError::AnchorOutOfRange {
                path: path.to_string(),
                anchor: edit.start.to_string(),
                lines: lines.len(),
            })?;
        let end_span = lines
            .get(end_line)
            .ok_or_else(|| EditError::AnchorOutOfRange {
                path: path.to_string(),
                anchor: edit.end.to_string(),
                lines: lines.len(),
            })?;

        // The replacement carries the content of the addressed lines, not their
        // terminators, so the terminator of the last addressed line survives a
        // replacement that ends without a newline. A replacement that ends with
        // one — and a deletion, which replaces the lines with nothing — takes
        // that terminator with it instead of leaving a blank line behind.
        let text = align_line_endings(edit.content, current);
        let end_byte = if text.is_empty() || text.ends_with('\n') {
            end_span.end
        } else {
            end_span.content_end
        };
        ranges.push(ResolvedRange {
            start_line,
            end_line,
            start_byte: start_span.start,
            end_byte,
            text,
            start_anchor: edit.start,
            end_anchor: edit.end,
        });
    }

    // Two ranges that share a line would make the result depend on the order of
    // application, so the whole change set is validated before a byte moves.
    ranges.sort_by_key(|range| range.start_line);
    for pair in ranges.windows(2) {
        let [first, second] = pair else {
            continue;
        };
        if second.start_line <= first.end_line {
            return Err(EditError::OverlappingAnchorRanges {
                path: path.to_string(),
                start: second.start_anchor.to_string(),
                end: second.end_anchor.to_string(),
                other_start: first.start_anchor.to_string(),
                other_end: first.end_anchor.to_string(),
            });
        }
    }

    // Bottom to top. Every offset comes from scanning `current` itself, so each
    // boundary is a character boundary of the snapshot by construction, and a
    // replacement never reaches the bytes an earlier range addressed.
    let mut updated = current.to_string();
    for range in ranges.iter().rev() {
        updated.replace_range(range.start_byte..range.end_byte, &range.text);
    }
    Ok(updated)
}

/// Decodes one anchor into the line index it names.
///
/// The anchor decodes only under the repository path it was rendered for, so
/// the staging engine passes the normalized path of the file being edited.
fn decode_line(anchor: &str, path: &str) -> Result<usize, EditError> {
    decode_anchor(path, anchor.trim()).map_err(|source| EditError::InvalidAnchor {
        path: path.to_string(),
        source,
    })
}

/// Aligns the replacement text with the line terminators of the file.
///
/// A CRLF file renders its lines without terminators, so the replacement the
/// model writes comes back with plain `\n` and has to be widened to the ending
/// the file already uses. The reverse direction keeps a change set authored with
/// Windows endings byte-consistent inside a Unix file.
fn align_line_endings(text: &str, current: &str) -> String {
    if current.contains("\r\n") && !text.contains("\r\n") {
        text.replace('\n', "\r\n")
    } else if !current.contains("\r\n") && text.contains("\r\n") {
        text.replace("\r\n", "\n")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::anchor::encode_anchor;

    /// An anchored range whose anchors are owned by the test fixture.
    struct Anchored {
        start: String,
        end: String,
        content: String,
    }

    impl Anchored {
        fn new(start: u16, end: u16, content: &str) -> Self {
            Self {
                start: encode_anchor("a.rs", start),
                end: encode_anchor("a.rs", end),
                content: content.to_string(),
            }
        }

        fn as_edit(&self) -> AnchorEdit<'_> {
            AnchorEdit {
                start: &self.start,
                end: &self.end,
                content: &self.content,
            }
        }
    }

    #[test]
    fn a_single_range_is_replaced_in_place() {
        let current = "alpha\nbeta\ngamma\n";
        let edit = Anchored::new(1, 1, "BETA\n");
        let updated = apply_anchor_edits(current, &[edit.as_edit()], "a.rs").unwrap();
        assert_eq!(updated, "alpha\nBETA\ngamma\n");
    }

    #[test]
    fn a_replacement_without_a_newline_keeps_the_following_line() {
        let current = "alpha\nbeta\ngamma\n";
        let edit = Anchored::new(1, 1, "BETA");
        let updated = apply_anchor_edits(current, &[edit.as_edit()], "a.rs").unwrap();
        assert_eq!(updated, "alpha\nBETA\ngamma\n");
    }

    #[test]
    fn the_proposal_order_never_changes_the_result() {
        let current = "l0\nl1\nl2\nl3\nl4\n";
        let bottom = Anchored::new(3, 3, "L3\n");
        let top = Anchored::new(1, 2, "MID\n");

        let forward =
            apply_anchor_edits(current, &[bottom.as_edit(), top.as_edit()], "a.rs").unwrap();
        let backward =
            apply_anchor_edits(current, &[top.as_edit(), bottom.as_edit()], "a.rs").unwrap();

        // The lower range absorbs two lines while the higher one is still
        // addressed by the line indices of the original snapshot.
        assert_eq!(forward, "l0\nMID\nL3\nl4\n");
        assert_eq!(backward, forward);
    }

    #[test]
    fn an_empty_replacement_deletes_the_terminator_of_the_range() {
        let current = "a\nb\nc\n";
        let edit = Anchored::new(0, 0, "");
        let updated = apply_anchor_edits(current, &[edit.as_edit()], "a.rs").unwrap();
        assert_eq!(updated, "b\nc\n");
    }

    #[test]
    fn deleting_the_unterminated_final_line_leaves_no_trailing_newline() {
        let current = "a\nb";
        let edit = Anchored::new(1, 1, "");
        let updated = apply_anchor_edits(current, &[edit.as_edit()], "a.rs").unwrap();
        assert_eq!(updated, "a\n");
    }

    #[test]
    fn a_crlf_file_keeps_its_terminators() {
        let current = "a\r\nb\r\nc\r\n";
        let edit = Anchored::new(1, 1, "B\n");
        let updated = apply_anchor_edits(current, &[edit.as_edit()], "a.rs").unwrap();
        assert_eq!(updated, "a\r\nB\r\nc\r\n");
    }

    #[test]
    fn anchors_address_exactly_the_rendered_lines() {
        let current = "a\nb";
        // The rendered block shows two lines, so the last addressable index is
        // one and the next one names no line of the file.
        assert!(apply_anchor_edits(current, &[Anchored::new(1, 1, "B").as_edit()], "a.rs").is_ok());
        assert!(matches!(
            apply_anchor_edits(current, &[Anchored::new(2, 2, "C").as_edit()], "a.rs"),
            Err(EditError::AnchorOutOfRange { lines: 2, .. })
        ));
    }

    #[test]
    fn malformed_ranges_are_rejected_before_any_byte_moves() {
        let current = "a\nb\nc\n";

        assert!(matches!(
            apply_anchor_edits(current, &[Anchored::new(9, 9, "x").as_edit()], "a.rs"),
            Err(EditError::AnchorOutOfRange { lines: 3, .. })
        ));

        assert!(matches!(
            apply_anchor_edits(current, &[Anchored::new(2, 1, "x").as_edit()], "a.rs"),
            Err(EditError::ReversedAnchorRange { .. })
        ));

        assert!(matches!(
            apply_anchor_edits(
                current,
                &[
                    Anchored::new(0, 1, "x\n").as_edit(),
                    Anchored::new(1, 1, "y\n").as_edit(),
                ],
                "a.rs"
            ),
            Err(EditError::OverlappingAnchorRanges { .. })
        ));

        // Adjacent ranges share no line, so they are accepted.
        let adjacent = apply_anchor_edits(
            current,
            &[
                Anchored::new(0, 0, "A\n").as_edit(),
                Anchored::new(1, 2, "REST\n").as_edit(),
            ],
            "a.rs",
        )
        .unwrap();
        assert_eq!(adjacent, "A\nREST\n");
    }

    #[test]
    fn an_invalid_anchor_names_the_file_and_the_codec_rejection() {
        let current = "a\n";
        for token in ["zzzz", "ab"] {
            let edit = AnchorEdit {
                start: token,
                end: token,
                content: "x",
            };
            match apply_anchor_edits(current, &[edit], "a.rs") {
                Err(EditError::InvalidAnchor { path, .. }) => assert_eq!(path, "a.rs"),
                other => panic!("expected an invalid anchor rejection, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_empty_change_set_reproduces_the_snapshot() {
        let current = "a\nb\n";
        assert_eq!(apply_anchor_edits(current, &[], "a.rs").unwrap(), current);
    }
}
