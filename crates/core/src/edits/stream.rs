use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
/// common to both parsers: in-memory hunk application, move registration, and
/// full-content overwrite. Keeping this logic in one place guarantees the
/// streaming UI status never diverges between protocols.
pub(crate) struct StagingSimulator {
    staged_contents: HashMap<String, String>,
    repo_root: PathBuf,
}

impl StagingSimulator {
    pub(crate) fn new(staged_contents: HashMap<String, String>, repo_root: PathBuf) -> Self {
        Self {
            staged_contents,
            repo_root,
        }
    }

    /// Replaces the staged file contents while preserving the repository root.
    pub(crate) fn set_staged_contents(&mut self, staged_contents: HashMap<String, String>) {
        self.staged_contents = staged_contents;
    }

    /// Returns the current content from staged memory or, failing that, from disk.
    pub(crate) fn current_content(&self, path: &str) -> Result<String, String> {
        current_file_content(&self.staged_contents, &self.repo_root, path)
    }

    /// Applies a replace hunk in memory and records the updated file content.
    ///
    /// Reuses the same resolution as the final disk application so the
    /// streaming UI status never diverges from the applied result.
    pub(crate) fn apply_hunk(
        &mut self,
        path: &str,
        old_text: &str,
        new_text: &str,
    ) -> Result<(), String> {
        let current_content = self.current_content(path)?;
        let updated = crate::edits::apply_replace(&current_content, old_text, new_text, path)
            .map_err(|err| err.to_string())?;
        self.staged_contents.insert(path.to_string(), updated);
        Ok(())
    }

    /// Registers a move by relocating staged or on-disk content to the new path.
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
    }

    /// Records full file content for a create or overwrite operation.
    pub(crate) fn overwrite(&mut self, path: &str, content: String) {
        self.staged_contents.insert(path.to_string(), content);
    }
}

#[cfg(test)]
mod tests {
    use super::StagingSimulator;
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
}
