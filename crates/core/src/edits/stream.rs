use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::edits::anchor::decode_anchor;
use crate::edits::fuzzy::scan_lines;
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
    /// staged contents untouched. The returned texts are the snapshot content of
    /// the addressed ranges in batch order, so the live preview can show the
    /// replacement instead of a bare insertion.
    ///
    /// A file's anchored ranges therefore arrive as one batch: the staged
    /// content becomes the applied result rather than an accumulation of
    /// batches.
    pub(crate) fn apply_anchor_batch(
        &mut self,
        path: &str,
        edits: &[AnchorEdit<'_>],
    ) -> Result<Vec<String>, String> {
        if edits.is_empty() {
            return Ok(Vec::new());
        }
        let original = self.original_content(path)?;
        let updated = crate::edits::apply_anchor_edits(&original, edits, path)
            .map_err(|err| err.to_string())?;
        let mut replaced = Vec::with_capacity(edits.len());
        for edit in edits {
            replaced.push(original_range_text(&original, path, edit.start, edit.end)?);
        }
        self.staged_contents.insert(path.to_string(), updated);
        Ok(replaced)
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
    pub(crate) fn apply_hunk(
        &mut self,
        path: &str,
        old_text: &str,
        new_text: &str,
    ) -> Result<(), String> {
        if old_text.trim().is_empty() {
            let rejection = crate::edits::EditError::ReplaceWithoutRange {
                path: path.to_string(),
            };
            return Err(rejection.to_string());
        }
        let current_content = self.current_content(path)?;
        let updated = crate::edits::apply_replace(&current_content, old_text, new_text, path)
            .map_err(|err| err.to_string())?;
        self.staged_contents.insert(path.to_string(), updated);
        Ok(())
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

/// Slices the text one anchored range held in the original snapshot.
///
/// The batch reaches this helper only after the authoritative staging primitive
/// accepted every range, so the anchors decode and the addressed lines exist.
/// The helper therefore describes the replacement instead of validating it: it
/// names the snapshot text the preview shows as replaced.
fn original_range_text(
    original: &str,
    path: &str,
    start: &str,
    end: &str,
) -> Result<String, String> {
    let lines = scan_lines(original);
    let first = decode_anchor(path, start.trim()).map_err(|err| err.to_string())?;
    let last = decode_anchor(path, end.trim()).map_err(|err| err.to_string())?;
    let first_start = lines
        .get(first)
        .map(|span| span.start)
        .ok_or_else(|| format!("In file '{path}': anchor '{start}' names no line of the file"))?;
    let last_end = lines
        .get(last)
        .map(|span| span.content_end)
        .ok_or_else(|| format!("In file '{path}': anchor '{end}' names no line of the file"))?;
    original
        .get(first_start..last_end)
        .map(str::to_string)
        .ok_or_else(|| format!("In file '{path}': the range '{start}'..'{end}' addresses no text"))
}

#[cfg(test)]
mod tests {
    use super::{AnchorEdit, StagingSimulator};
    use crate::edits::anchor::encode_anchor;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Creates a unique temporary directory for a single test case.
    fn unique_test_dir() -> PathBuf {
        let seq = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "tauqe-stream-blocking-{}-{}",
            std::process::id(),
            seq
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reads_files_and_moves_without_stalling_the_runtime() {
        let repo_root = unique_test_dir();
        std::fs::write(repo_root.join("source.txt"), "hello world").expect("write source file");

        let mut simulator = StagingSimulator::new(HashMap::new(), repo_root.clone());

        assert_eq!(
            simulator.current_content("source.txt").unwrap(),
            "hello world"
        );
        assert_eq!(
            simulator
                .apply_hunk("source.txt", "hello", "goodbye")
                .map(|()| simulator.current_content("source.txt").unwrap())
                .unwrap(),
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
        let repo_root = unique_test_dir();
        std::fs::write(repo_root.join("source.txt"), "alpha\nbeta\ngamma\n")
            .expect("write source file");
        let mut simulator = StagingSimulator::new(HashMap::new(), repo_root.clone());

        // The upper range covers two lines and the lower one a single line, and
        // both name the rendered snapshot whatever order the proposal lists them
        // in.
        let upper = Anchored::new("source.txt", 0, 1, "ALPHA\nBETA\n");
        let lower = Anchored::new("source.txt", 2, 2, "GAMMA\n");
        let replaced = simulator
            .apply_anchor_batch("source.txt", &[upper.as_edit(), lower.as_edit()])
            .expect("the batch applies");

        assert_eq!(
            replaced,
            vec!["alpha\nbeta".to_string(), "gamma".to_string()]
        );
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
        let repo_root = unique_test_dir();
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
        let repo_root = unique_test_dir();
        std::fs::write(repo_root.join("source.txt"), "alpha\nbeta\ngamma\n")
            .expect("write source file");
        // The change set starts from content an earlier attempt already staged,
        // so those two lines are the snapshot the anchors name.
        let mut simulator = StagingSimulator::new(
            HashMap::from([("source.txt".to_string(), "alpha\nbeta\n".to_string())]),
            repo_root.clone(),
        );

        let second = Anchored::new("source.txt", 1, 1, "BETA\n");
        let replaced = simulator
            .apply_anchor_batch("source.txt", &[second.as_edit()])
            .expect("the seeded snapshot has the addressed line");
        assert_eq!(replaced, vec!["beta".to_string()]);

        // The snapshot stays the address space of every later batch of the file.
        let third = Anchored::new("source.txt", 2, 2, "GAMMA\n");
        let error = simulator
            .apply_anchor_batch("source.txt", &[third.as_edit()])
            .expect_err("the seeded snapshot has no third line");
        assert!(error.contains("names no line"), "{error}");
    }

    #[test]
    fn a_rangeless_replace_is_rejected_with_the_staging_classification() {
        let repo_root = unique_test_dir();
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
}
