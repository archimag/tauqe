use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::edits::anchor::decode_anchor;
use crate::edits::fuzzy::{scan_lines, LineSpan};
use crate::edits::staging::AnchorEdit;
use crate::providers::StreamEvent;

pub mod discussion;
pub mod json;
pub mod xml;

pub use discussion::DiscussionStreamFilter;
pub use json::{JsonFieldStreamFilter, JsonStreamFilter};
pub use xml::XmlStreamFilter;

/// Common interface for streaming parsers that intercept code edits in real time.
pub trait EditStreamFilter: Send {
    /// Pushes a delta chunk from the provider and returns any generated stream events.
    fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent>;

    /// Flushes any buffered content at the end of generation.
    fn finish(self: Box<Self>) -> Vec<StreamEvent>;

    /// Discards everything the filter buffered so far.
    ///
    /// The transport voids an interrupted attempt and re-issues the request, so
    /// a filter that already observed that attempt must forget it. Its partial
    /// fragment would otherwise be completed by the bytes of the replacement
    /// attempt and reach the preview the client already reset.
    fn reset(&mut self);
}

pub(crate) fn resolve_path(candidate: &str, editable_paths: &[String], is_create: bool) -> String {
    crate::edits::protocol::utils::resolve_target_path_for_op(candidate, editable_paths, is_create)
        .unwrap_or_else(|_| {
            candidate
                .trim()
                .trim_matches('`')
                .trim_start_matches("./")
                .trim_start_matches('/')
                .to_string()
        })
}

pub(crate) fn current_file_content(
    staged: &HashMap<String, String>,
    repo_root: &Path,
    path: &str,
) -> Result<String, String> {
    if let Some(content) = staged.get(path) {
        return Ok(content.clone());
    }
    let full_path = repo_root.join(path);
    crate::blocking::run_blocking(|| std::fs::read_to_string(&full_path))
        .map_err(|e| format!("Cannot read file '{}': {}", path, e))
}

// Streaming filters normalize hunks exactly as the final disk parser does, so
// the protocol node normalizer is the single implementation for both paths.
pub(crate) use crate::edits::protocol::utils::normalize_content as normalize_hunk;

/// Diagnostic a streaming filter reports for an edit the response never closed.
///
/// A filter announces a file as soon as its header arrives, which is earlier
/// than the authoritative decoder accepts anything. When the response ends
/// inside that edit, the decoder refuses or drops it, so the terminal preview
/// event reports this diagnostic instead of certifying a change the turn will
/// not deliver.
pub(crate) const TRUNCATED_EDIT_DIAGNOSTIC: &str =
    "edit block truncated before its closing tag: the response ended inside the edit";
/// Real line coordinates of one streamed hunk.
///
/// A unified diff names the lines a hunk covers on both sides of the change —
/// `@@ -old_start,old_lines +new_start,new_lines @@ hint` — so a diff surface
/// prints real line numbers instead of counting the rows of its own rendering.
/// A start of `0` states that the position is unknown, and a count is only
/// meaningful beside a known start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HunkCoordinates {
    /// One-based first line of the hunk in the file before the change.
    pub(crate) old_start: usize,
    /// Number of source lines the hunk covers.
    pub(crate) old_lines: usize,
    /// One-based first line of the hunk in the file after the change.
    pub(crate) new_start: usize,
    /// Number of result lines the hunk covers.
    pub(crate) new_lines: usize,
    /// Section the hunk belongs to, as a diff header names it after its ranges.
    pub(crate) context_hint: Option<String>,
}

/// Longest section hint a streamed hunk carries, in characters.
const MAX_CONTEXT_HINT_CHARS: usize = 120;

impl HunkCoordinates {
    /// Coordinates of a payload that replaces a whole file.
    ///
    /// A created file has nothing to replace, so its old side stays absent. An
    /// overwritten file is replaced from its first line to its last one.
    pub(crate) fn whole_file(old_text: Option<&str>, new_text: &str) -> Self {
        let (old_start, old_lines) = match old_text {
            Some(text) if !text.is_empty() => (1, line_count(text)),
            _ => (0, 0),
        };
        Self {
            old_start,
            old_lines,
            new_start: 1,
            new_lines: line_count(new_text),
            context_hint: None,
        }
    }

    /// Coordinates of a located search/replace attempt.
    ///
    /// The coordinates are resolved against the content the patch was applied
    /// to, because that is the document the old side of the hunk describes. A
    /// patch the fuzzy matcher accepted without an exact match is located by its
    /// first line instead, and a patch whose position cannot be named reports
    /// unknown coordinates rather than an invented line.
    pub(crate) fn replacement(current: &str, old_text: &str, new_text: &str) -> Self {
        let lines = scan_lines(current);
        let Some(offset) = locate_replacement(current, &lines, old_text) else {
            return Self::unknown();
        };
        let start = line_index_at(&lines, offset);
        Self {
            old_start: start,
            old_lines: line_count(old_text),
            new_start: start,
            new_lines: line_count(new_text),
            context_hint: section_hint(current, &lines, start.saturating_sub(1)),
        }
    }

    /// Coordinates of one anchored range resolved against the file snapshot.
    ///
    /// The anchors name the lines the context block rendered, so the range keeps
    /// the indices it decoded to on the old side, while the new side counts the
    /// lines the replacement introduces from the very same line.
    pub(crate) fn anchored(
        original: &str,
        lines: &[LineSpan],
        first: usize,
        last: usize,
        replacement: &str,
    ) -> Self {
        Self {
            old_start: first + 1,
            old_lines: last.saturating_sub(first) + 1,
            new_start: first + 1,
            new_lines: line_count(replacement),
            context_hint: section_hint(original, lines, first),
        }
    }

    /// Coordinates a staging simulator could not resolve.
    pub(crate) fn unknown() -> Self {
        Self {
            old_start: 0,
            old_lines: 0,
            new_start: 0,
            new_lines: 0,
            context_hint: None,
        }
    }
}

/// One anchored range resolved into the preview a streaming consumer reports.
#[derive(Debug)]
pub(crate) struct AnchorPreview {
    /// Snapshot text of the addressed range.
    pub(crate) replaced: String,
    /// Coordinates of the range on both sides of the change.
    pub(crate) coordinates: HunkCoordinates,
}

/// Builds the streamed hunk event of one applied change.
///
/// Every contour reports a hunk through this constructor, so the coordinates a
/// diff surface reads always travel beside the texts they describe.
pub(crate) fn hunk_event(
    path: &str,
    hunk_index: usize,
    coordinates: HunkCoordinates,
    old_text: String,
    new_text: String,
) -> StreamEvent {
    StreamEvent::EditHunk {
        path: path.to_string(),
        hunk_index,
        old_start: coordinates.old_start,
        old_lines: coordinates.old_lines,
        new_start: coordinates.new_start,
        new_lines: coordinates.new_lines,
        context_hint: coordinates.context_hint,
        old_text,
        new_text,
    }
}

/// Number of lines a text occupies, as a diff counts the lines of a hunk.
fn line_count(text: &str) -> usize {
    scan_lines(text).len()
}

/// One-based index of the line that holds `offset`.
fn line_index_at(lines: &[LineSpan], offset: usize) -> usize {
    lines.iter().take_while(|span| span.start < offset).count() + 1
}

/// Locates the first byte a search/replace pattern addresses.
///
/// An exact match is the offset the pattern starts at. A patch the fuzzy matcher
/// accepted by trailing whitespace or indentation has no exact match, so its
/// first non-empty line names the place instead: the first line of the file whose
/// trimmed content equals that line is where the matched block begins.
fn locate_replacement(current: &str, lines: &[LineSpan], old_text: &str) -> Option<usize> {
    if let Some(offset) = current.find(old_text) {
        return Some(offset);
    }
    let pattern = scan_lines(old_text);
    let first = pattern.iter().find(|span| {
        old_text
            .get(span.start..span.content_end)
            .is_some_and(|line| !line.trim().is_empty())
    })?;
    let needle = old_text.get(first.start..first.content_end)?.trim();
    lines.iter().find_map(|span| {
        let line = current.get(span.start..span.content_end)?;
        (line.trim() == needle).then_some(span.start)
    })
}

/// Nearest non-empty line above a hunk, as a diff header renders its section.
///
/// The hint is read from the document itself, so it names the construct the
/// change sits in without asking the model to describe it.
fn section_hint(text: &str, lines: &[LineSpan], before_line: usize) -> Option<String> {
    lines.iter().take(before_line).rev().find_map(|span| {
        let line = text.get(span.start..span.content_end)?.trim();
        if line.is_empty() {
            return None;
        }
        Some(line.chars().take(MAX_CONTEXT_HINT_CHARS).collect())
    })
}

/// Shared in-memory staging simulator for the streaming edit filters.
///
/// Both the XML and JSON filters must report edit status exactly as the final
/// disk application will resolve it. This simulator owns the staged file
/// contents plus the repository root and exposes the mutation primitives
/// common to both parsers: in-memory hunk application, anchored range batches,
/// move registration, and full-content overwrite. Keeping this logic in one
/// place guarantees the streaming UI status never diverges between protocols.
pub(crate) struct StagingSimulator {
    staged_contents: HashMap<String, String>,
    /// The content every file had before the change set touched it.
    ///
    /// Anchors name the lines the context block rendered, which is the file as it
    /// looked before the change set, so a batch of anchored ranges resolves
    /// against this snapshot while `staged_contents` keeps the cumulative result.
    original_contents: HashMap<String, String>,
    repo_root: PathBuf,
}

impl StagingSimulator {
    pub(crate) fn new(staged_contents: HashMap<String, String>, repo_root: PathBuf) -> Self {
        Self {
            original_contents: staged_contents.clone(),
            staged_contents,
            repo_root,
        }
    }

    /// Replaces the staged file contents while preserving the repository root.
    ///
    /// The supplied contents describe the files the change set starts from, so
    /// they also become the snapshot anchored ranges are resolved against.
    pub(crate) fn set_staged_contents(&mut self, staged_contents: HashMap<String, String>) {
        self.original_contents = staged_contents.clone();
        self.staged_contents = staged_contents;
    }

    /// Restores the staged contents to the snapshot the change set starts from.
    ///
    /// A retried attempt re-proposes the whole change set against the same
    /// rendered context, so the preview must discard the hunks of the voided
    /// attempt instead of applying the replacement on top of them. The snapshot
    /// itself is preserved, because the anchors still name the lines it held.
    pub(crate) fn reset(&mut self) {
        self.staged_contents = self.original_contents.clone();
    }

    /// Returns the content a file had before the change set touched it.
    ///
    /// A file the change set already knows carries its snapshot from the seeded
    /// contents; a file it created or moved during the stream is read on first
    /// use, exactly as the current content would be.
    fn original_content(&mut self, path: &str) -> Result<String, String> {
        if let Some(content) = self.original_contents.get(path) {
            return Ok(content.clone());
        }
        let content = current_file_content(&self.staged_contents, &self.repo_root, path)?;
        self.original_contents
            .insert(path.to_string(), content.clone());
        Ok(content)
    }
    /// Applies one batch of anchored ranges against the original snapshot.
    ///
    /// Every anchor of a file names a line the context block rendered, so the
    /// whole batch of one file is validated and applied at once by the
    /// authoritative staging primitive: an anchor outside the snapshot, a
    /// reversed range, and two overlapping ranges are rejected with the very
    /// error the disk application would produce, and a rejected batch leaves the
    /// staged contents untouched. The returned previews describe the addressed
    /// ranges in batch order — the snapshot text the live preview shows as
    /// removed, together with the coordinates the hunk header states — so the
    /// stream reports a real replacement at a real place.
    ///
    /// A file's anchored ranges therefore arrive as one batch: the staged
    /// content becomes the applied result rather than an accumulation of
    /// batches.
    pub(crate) fn apply_anchor_batch(
        &mut self,
        path: &str,
        edits: &[AnchorEdit<'_>],
    ) -> Result<Vec<AnchorPreview>, String> {
        if edits.is_empty() {
            return Ok(Vec::new());
        }
        let original = self.original_content(path)?;
        let updated = crate::edits::apply_anchor_edits(&original, edits, path)
            .map_err(|err| err.to_string())?;
        let lines = scan_lines(&original);
        let mut previews = Vec::with_capacity(edits.len());
        for edit in edits {
            previews.push(anchor_preview(&original, &lines, path, edit)?);
        }
        self.staged_contents.insert(path.to_string(), updated);
        Ok(previews)
    }

    /// Returns the current content from staged memory or, failing that, from disk.
    pub(crate) fn current_content(&self, path: &str) -> Result<String, String> {
        current_file_content(&self.staged_contents, &self.repo_root, path)
    }

    /// Applies a replace hunk in memory and records the updated file content.
    ///
    /// Reuses the same resolution as the final disk application so the
    /// streaming UI status never diverges from the applied result. The parity
    /// covers the classification of a rejection as well: a replacement that
    /// addresses no line range is refused with the typed
    /// [`crate::edits::EditError::ReplaceWithoutRange`], exactly as the staging
    /// engine refuses it, so the preview explains the defect in the vocabulary
    /// of the active patch strategy instead of reporting a search/replace miss.
    ///
    /// The coordinates are resolved against the content the patch was applied
    /// to, because that is the document the old side of the hunk describes, and
    /// they travel back to the caller so the streamed hunk names the lines it
    /// really changes.
    pub(crate) fn apply_hunk(
        &mut self,
        path: &str,
        old_text: &str,
        new_text: &str,
    ) -> Result<HunkCoordinates, String> {
        if old_text.trim().is_empty() {
            let rejection = crate::edits::EditError::ReplaceWithoutRange {
                path: path.to_string(),
            };
            return Err(rejection.to_string());
        }
        let current_content = self.current_content(path)?;
        let updated = crate::edits::apply_replace(&current_content, old_text, new_text, path)
            .map_err(|err| err.to_string())?;
        let coordinates = HunkCoordinates::replacement(&current_content, old_text, new_text);
        self.staged_contents.insert(path.to_string(), updated);
        Ok(coordinates)
    }

    /// Registers a move by relocating staged or on-disk content to the new path.
    ///
    /// The original snapshot travels with the content, because the anchors of the
    /// destination still name the lines the file had before the change set.
    pub(crate) fn apply_move(&mut self, from: &str, to: &str) {
        if let Some(content) = self.staged_contents.remove(from) {
            self.staged_contents.insert(to.to_string(), content);
        } else {
            let full_from = self.repo_root.join(from);
            let read = crate::blocking::run_blocking(|| std::fs::read_to_string(&full_from));
            if let Ok(content) = read {
                self.staged_contents.insert(to.to_string(), content);
            }
        }
        if let Some(original) = self.original_contents.remove(from) {
            self.original_contents.insert(to.to_string(), original);
        }
    }

    /// Records full file content for a create or overwrite operation.
    pub(crate) fn overwrite(&mut self, path: &str, content: String) {
        self.staged_contents.insert(path.to_string(), content);
    }
}

/// One anchored line range a change set has addressed so far.
///
/// The entries of one file arrive separately while a payload streams, yet every
/// anchor names a line the context block rendered before the change set ran.
/// Keeping the whole batch lets it be re-applied to the original snapshot as it
/// grows, so an earlier range is never lost to a later one.
pub(crate) struct OwnedAnchor {
    pub(crate) start: String,
    pub(crate) end: String,
    pub(crate) content: String,
}

impl OwnedAnchor {
    pub(crate) fn as_edit(&self) -> AnchorEdit<'_> {
        AnchorEdit {
            start: &self.start,
            end: &self.end,
            content: &self.content,
        }
    }
}

/// Resolves one anchored range into the preview a streaming consumer reports.
///
/// The batch reaches this helper only after the authoritative staging primitive
/// accepted every range, so the anchors decode and the addressed lines exist.
/// The helper therefore describes the replacement instead of validating it: it
/// names the snapshot text the preview shows as removed together with the
/// coordinates the hunk header states, so the stream places the change at the
/// lines the context block rendered.
fn anchor_preview(
    original: &str,
    lines: &[LineSpan],
    path: &str,
    edit: &AnchorEdit<'_>,
) -> Result<AnchorPreview, String> {
    let first = decode_anchor(path, edit.start.trim()).map_err(|err| err.to_string())?;
    let last = decode_anchor(path, edit.end.trim()).map_err(|err| err.to_string())?;
    let replaced = addressed_text(original, lines, path, edit, first, last)?;
    let coordinates = HunkCoordinates::anchored(original, lines, first, last, edit.content);
    Ok(AnchorPreview {
        replaced,
        coordinates,
    })
}

/// Slices the text one anchored range held in the decoded snapshot lines.
fn addressed_text(
    original: &str,
    lines: &[LineSpan],
    path: &str,
    edit: &AnchorEdit<'_>,
    first: usize,
    last: usize,
) -> Result<String, String> {
    let first_start = lines.get(first).map(|span| span.start).ok_or_else(|| {
        format!(
            "In file '{path}': anchor '{}' names no line of the file",
            edit.start
        )
    })?;
    let last_end = lines
        .get(last)
        .map(|span| span.content_end)
        .ok_or_else(|| {
            format!(
                "In file '{path}': anchor '{}' names no line of the file",
                edit.end
            )
        })?;
    original
        .get(first_start..last_end)
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "In file '{path}': the range '{}'..'{}' addresses no text",
                edit.start, edit.end
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{AnchorEdit, StagingSimulator};
    use crate::edits::anchor::encode_anchor;
    use crate::test_paths::scratch_dir;
    use std::collections::HashMap;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reads_files_and_moves_without_stalling_the_runtime() {
        let repo_root = scratch_dir("stream-blocking");
        std::fs::write(repo_root.join("source.txt"), "hello world").expect("write source file");

        let mut simulator = StagingSimulator::new(HashMap::new(), repo_root.clone());

        assert_eq!(
            simulator.current_content("source.txt").unwrap(),
            "hello world"
        );
        simulator
            .apply_hunk("source.txt", "hello", "goodbye")
            .expect("the replacement applies");
        assert_eq!(
            simulator.current_content("source.txt").unwrap(),
            "goodbye world"
        );

        simulator.apply_move("source.txt", "moved.txt");
        assert_eq!(
            simulator.current_content("moved.txt").unwrap(),
            "goodbye world"
        );

        std::fs::remove_dir_all(&repo_root).ok();
    }
    /// An anchored range whose anchors and content are owned by the test.
    struct Anchored {
        start: String,
        end: String,
        content: String,
    }

    impl Anchored {
        fn new(path: &str, start: u16, end: u16, content: &str) -> Self {
            Self {
                start: encode_anchor(path, start),
                end: encode_anchor(path, end),
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
    fn anchored_batch_reports_the_replaced_ranges_of_the_snapshot() {
        let repo_root = scratch_dir("stream-blocking");
        std::fs::write(repo_root.join("source.txt"), "alpha\nbeta\ngamma\n")
            .expect("write source file");
        let mut simulator = StagingSimulator::new(HashMap::new(), repo_root.clone());

        // The upper range covers two lines and the lower one a single line, and
        // both name the rendered snapshot whatever order the proposal lists them
        // in.
        let upper = Anchored::new("source.txt", 0, 1, "ALPHA\nBETA\n");
        let lower = Anchored::new("source.txt", 2, 2, "GAMMA\n");
        let previews = simulator
            .apply_anchor_batch("source.txt", &[upper.as_edit(), lower.as_edit()])
            .expect("the batch applies");

        let replaced: Vec<&str> = previews
            .iter()
            .map(|preview| preview.replaced.as_str())
            .collect();
        assert_eq!(replaced, vec!["alpha\nbeta", "gamma"]);

        // The anchors name the rendered lines, so the preview states the real
        // coordinates of both ranges instead of counting its own rows.
        assert_eq!(previews[0].coordinates.old_start, 1);
        assert_eq!(previews[0].coordinates.old_lines, 2);
        assert_eq!(previews[0].coordinates.new_start, 1);
        assert_eq!(previews[0].coordinates.new_lines, 2);
        assert_eq!(previews[1].coordinates.old_start, 3);
        assert_eq!(previews[1].coordinates.old_lines, 1);
        assert_eq!(previews[1].coordinates.new_lines, 1);
        assert_eq!(
            simulator.current_content("source.txt").unwrap(),
            "ALPHA\nBETA\nGAMMA\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo_root.join("source.txt")).unwrap(),
            "alpha\nbeta\ngamma\n",
            "an anchored batch stays in memory"
        );

        std::fs::remove_dir_all(&repo_root).ok();
    }

    #[test]
    fn a_rejected_anchored_batch_leaves_the_staged_content_untouched() {
        let repo_root = scratch_dir("stream-blocking");
        std::fs::write(repo_root.join("source.txt"), "alpha\nbeta\n").expect("write source file");
        let mut simulator = StagingSimulator::new(HashMap::new(), repo_root.clone());

        let beyond = Anchored::new("source.txt", 0, 4, "x\n");
        let error = simulator
            .apply_anchor_batch("source.txt", &[beyond.as_edit()])
            .expect_err("the range runs past the end of the file");
        assert!(error.contains("names no line"), "{error}");

        let overlapping = [
            Anchored::new("source.txt", 0, 1, "x\n"),
            Anchored::new("source.txt", 1, 1, "y\n"),
        ];
        let edits: Vec<AnchorEdit<'_>> = overlapping.iter().map(Anchored::as_edit).collect();
        let error = simulator
            .apply_anchor_batch("source.txt", &edits)
            .expect_err("two ranges share a line");
        assert!(error.contains("overlaps"), "{error}");

        assert_eq!(
            simulator.current_content("source.txt").unwrap(),
            "alpha\nbeta\n",
            "a rejected batch must not reach the staged contents"
        );

        std::fs::remove_dir_all(&repo_root).ok();
    }

    #[test]
    fn a_seeded_snapshot_wins_over_the_file_on_disk() {
        let repo_root = scratch_dir("stream-blocking");
        std::fs::write(repo_root.join("source.txt"), "alpha\nbeta\ngamma\n")
            .expect("write source file");
        // The change set starts from content an earlier attempt already staged,
        // so those two lines are the snapshot the anchors name.
        let mut simulator = StagingSimulator::new(
            HashMap::from([("source.txt".to_string(), "alpha\nbeta\n".to_string())]),
            repo_root.clone(),
        );

        let second = Anchored::new("source.txt", 1, 1, "BETA\n");
        let previews = simulator
            .apply_anchor_batch("source.txt", &[second.as_edit()])
            .expect("the seeded snapshot has the addressed line");
        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0].replaced, "beta");
        assert_eq!(previews[0].coordinates.old_start, 2);

        // The snapshot stays the address space of every later batch of the file.
        let third = Anchored::new("source.txt", 2, 2, "GAMMA\n");
        let error = simulator
            .apply_anchor_batch("source.txt", &[third.as_edit()])
            .expect_err("the seeded snapshot has no third line");
        assert!(error.contains("names no line"), "{error}");
    }

    #[test]
    fn a_rangeless_replace_is_rejected_with_the_staging_classification() {
        let repo_root = scratch_dir("stream-blocking");
        std::fs::write(repo_root.join("source.txt"), "alpha\nbeta\n").expect("write source file");
        let mut simulator = StagingSimulator::new(HashMap::new(), repo_root.clone());

        // A replacement that echoes no text addresses no line of the file, so
        // the preview reports the very classification the staging engine
        // produces instead of a search/replace miss that names `old_text`.
        let error = simulator
            .apply_hunk("source.txt", "  \n\t ", "x\n")
            .expect_err("a rangeless replacement must be rejected");
        assert!(error.contains("addressed no replacement range"), "{error}");
        assert!(!error.contains("old_text"), "{error}");
        assert!(!error.contains("Search block"), "{error}");
        assert_eq!(
            simulator.current_content("source.txt").unwrap(),
            "alpha\nbeta\n",
            "a rejected hunk must not reach the staged contents"
        );

        std::fs::remove_dir_all(&repo_root).ok();
    }
    #[test]
    fn a_replacement_reports_the_lines_it_addresses() {
        let repo_root = scratch_dir("stream-blocking");
        std::fs::write(repo_root.join("source.txt"), "alpha\nbeta\ngamma\n")
            .expect("write source file");
        let mut simulator = StagingSimulator::new(HashMap::new(), repo_root.clone());

        let coordinates = simulator
            .apply_hunk("source.txt", "beta\n", "BETA\nBETA2\n")
            .expect("the replacement applies");

        // The match sits on the second line and yields two lines, so the hunk
        // names exactly those coordinates on both sides of the change.
        assert_eq!(coordinates.old_start, 2);
        assert_eq!(coordinates.old_lines, 1);
        assert_eq!(coordinates.new_start, 2);
        assert_eq!(coordinates.new_lines, 2);
        assert_eq!(coordinates.context_hint.as_deref(), Some("alpha"));

        std::fs::remove_dir_all(&repo_root).ok();
    }

    #[test]
    fn an_indentation_shifted_replacement_reports_the_lines_it_addresses() {
        let repo_root = scratch_dir("stream-blocking");
        std::fs::write(
            repo_root.join("source.txt"),
            "impl Test {\n        fn run() {\n            let a = 1;\n        }\n}\n",
        )
        .expect("write source file");
        let mut simulator = StagingSimulator::new(HashMap::new(), repo_root.clone());

        // The authored block carries the indentation of its own level, so the
        // exact match misses and the indentation-shifted match locates the
        // change by its first line instead.
        let coordinates = simulator
            .apply_hunk(
                "source.txt",
                "fn run() {\n    let a = 1;\n}\n",
                "fn run() {\n    let a = 2;\n}\n",
            )
            .expect("the indentation-shifted replacement applies");

        assert_eq!(coordinates.old_start, 2);
        assert_eq!(coordinates.old_lines, 3);
        assert_eq!(coordinates.new_start, 2);
        assert_eq!(coordinates.new_lines, 3);
        assert_eq!(coordinates.context_hint.as_deref(), Some("impl Test {"));

        std::fs::remove_dir_all(&repo_root).ok();
    }
}
