use std::collections::HashMap;
use std::path::Path;

use crate::providers::StreamEvent;

pub mod discussion;
pub mod json;
pub mod xml;

pub use discussion::DiscussionStreamFilter;
pub use json::JsonStreamFilter;
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
