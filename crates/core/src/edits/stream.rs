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
    std::fs::read_to_string(repo_root.join(path))
        .map_err(|e| format!("Cannot read file '{}': {}", path, e))
}

pub(crate) fn normalize_hunk(raw: &str) -> String {
    let s = if let Some(stripped) = raw.strip_prefix("\r\n") {
        stripped
    } else if let Some(stripped) = raw.strip_prefix('\n') {
        stripped
    } else {
        raw
    };

    if let Some(last_nl) = s.rfind('\n') {
        let trailing = &s[last_nl + 1..];
        if trailing.chars().all(|c| c == ' ' || c == '\t') {
            return s[..=last_nl].to_string();
        }
    }
    s.to_string()
}

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
            if let Ok(content) = std::fs::read_to_string(&full_from) {
                self.staged_contents.insert(to.to_string(), content);
            }
        }
    }

    /// Records full file content for a create or overwrite operation.
    pub(crate) fn overwrite(&mut self, path: &str, content: String) {
        self.staged_contents.insert(path.to_string(), content);
    }
}
