use std::collections::{HashMap, HashSet};
use std::path::Path;
use thiserror::Error;
use tauqe_protocol::{EditOperation, EditProposal};

use crate::context::ContextManager;

pub mod protocol;
pub mod stream;

pub use protocol::{EditProtocol, EditProtocolFactory, XmlEditProtocol};
pub use stream::{EditStreamFilter, JsonStreamFilter, XmlStreamFilter};

#[derive(Debug, Error)]
pub enum EditError {
    #[error("File '{0}' is not in context. Only files explicitly in context can be edited.")]
    NotInContext(String),

    #[error("File '{0}' is read-only. Change its access to editable before applying edits.")]
    ReadOnly(String),

    #[error("File '{0}' does not exist on disk.")]
    FileNotFound(String),

    #[error("Target path '{0}' already exists.")]
    DestinationExists(String),

    #[error("Target path '{0}' is invalid: {1}")]
    InvalidPath(String, String),

    #[error("In file '{path}': old_text not found (0 matches). Code might be stale or incorrect.")]
    NoMatch { path: String },

    #[error("In file '{path}': old_text matches {count} times. Replacement must be uniquely identifiable (exactly 1 match).")]
    AmbiguousMatch { path: String, count: usize },

    #[error("IO error for '{0}': {1}")]
    Io(String, #[source] std::io::Error),
}

/// Cumulative in-memory state. Paths are normalized relative to the repository.
/// A path must occur in at most one of these collections.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagedEditsState {
    pub staged_files: HashMap<String, String>,
    pub deleted_paths: HashSet<String>,
}

#[derive(Debug)]
pub struct FailedFileEdit {
    pub path: String,
    pub error: EditError,
}

#[derive(Debug)]
pub struct StageEditsResult {
    /// Includes successful state from earlier attempts, unchanged on a file failure.
    pub staged_state: StagedEditsState,
    /// Successful paths and operations from this attempt only.
    pub successful_paths: Vec<String>,
    pub successful_edits: Vec<EditOperation>,
    pub failed_files: Vec<FailedFileEdit>,
}

impl StageEditsResult {
    pub fn is_success(&self) -> bool {
        self.failed_files.is_empty()
    }
}

/// Validates edits without changing disk or context. Operations for each normalized
/// path are applied in order, and the entire file is rolled back if any operation
/// fails. Other files can succeed independently. Pass the returned staged state
/// into the next attempt to retain earlier successes.
pub fn stage_and_validate_edits(
    repo_root: &Path,
    context_manager: &ContextManager,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
) -> StageEditsResult {
    let mut result = StageEditsResult {
        staged_state: initial_staged.cloned().unwrap_or_default(),
        successful_paths: Vec::new(),
        successful_edits: Vec::new(),
        failed_files: Vec::new(),
    };
    let mut file_order = Vec::new();
    let mut grouped: HashMap<String, Vec<&EditOperation>> = HashMap::new();
    let mut invalid_paths = HashSet::new();

    let flush_grouped = |grouped: &mut HashMap<String, Vec<&EditOperation>>,
                         file_order: &mut Vec<String>,
                         res: &mut StageEditsResult| {
        for path in file_order.drain(..) {
            if let Some(operations) = grouped.remove(&path) {
                match stage_file_edits(
                    repo_root,
                    context_manager,
                    &path,
                    &operations,
                    &res.staged_state,
                ) {
                    Ok(Some(content)) => {
                        res.staged_state.deleted_paths.remove(&path);
                        res.staged_state.staged_files.insert(path.clone(), content);
                    }
                    Ok(None) => {
                        res.staged_state.staged_files.remove(&path);
                        res.staged_state.deleted_paths.insert(path.clone());
                    }
                    Err(error) => {
                        res.failed_files.push(FailedFileEdit { path, error });
                        continue;
                    }
                }
                if !res.successful_paths.contains(&path) {
                    res.successful_paths.push(path);
                }
                res.successful_edits
                    .extend(operations.into_iter().cloned());
            }
        }
    };

    for edit in edits {
        match edit {
            EditOperation::Move { from, to } => {
                flush_grouped(&mut grouped, &mut file_order, &mut result);

                let clean_from = match context_manager.normalize_path(from) {
                    Ok(p) => p,
                    Err(err) => {
                        result.failed_files.push(FailedFileEdit {
                            path: from.clone(),
                            error: EditError::InvalidPath(from.clone(), err.to_string()),
                        });
                        continue;
                    }
                };

                let clean_to = match context_manager.normalize_path(to) {
                    Ok(p) => p,
                    Err(err) => {
                        result.failed_files.push(FailedFileEdit {
                            path: to.clone(),
                            error: EditError::InvalidPath(to.clone(), err.to_string()),
                        });
                        continue;
                    }
                };

                if clean_from == clean_to {
                    result.failed_files.push(FailedFileEdit {
                        path: clean_to.clone(),
                        error: EditError::InvalidPath(clean_to, "Source and destination are identical".to_string()),
                    });
                    continue;
                }

                let in_context = context_manager.contains(&clean_from).unwrap_or(false);
                let editable = context_manager.is_editable(&clean_from).unwrap_or(false);
                let newly_created = !repo_root.join(&clean_from).exists()
                    && result.staged_state.staged_files.contains_key(&clean_from);

                if in_context && !editable {
                    result.failed_files.push(FailedFileEdit {
                        path: clean_from.clone(),
                        error: EditError::ReadOnly(clean_from),
                    });
                    continue;
                }

                if !in_context && !newly_created {
                    result.failed_files.push(FailedFileEdit {
                        path: clean_from.clone(),
                        error: EditError::NotInContext(clean_from),
                    });
                    continue;
                }

                let content = if let Some(c) = result.staged_state.staged_files.get(&clean_from) {
                    Some(c.clone())
                } else if result.staged_state.deleted_paths.contains(&clean_from) {
                    None
                } else {
                    let full = repo_root.join(&clean_from);
                    if full.is_file() {
                        std::fs::read_to_string(&full).ok()
                    } else {
                        None
                    }
                };

                let Some(content) = content else {
                    result.failed_files.push(FailedFileEdit {
                        path: clean_from.clone(),
                        error: EditError::FileNotFound(clean_from),
                    });
                    continue;
                };

                let full_to = repo_root.join(&clean_to);
                let dest_exists = (full_to.exists()
                    && !result.staged_state.deleted_paths.contains(&clean_to))
                    || result.staged_state.staged_files.contains_key(&clean_to);

                if dest_exists {
                    result.failed_files.push(FailedFileEdit {
                        path: clean_to.clone(),
                        error: EditError::DestinationExists(clean_to),
                    });
                    continue;
                }

                result.staged_state.staged_files.remove(&clean_from);
                result.staged_state.deleted_paths.insert(clean_from.clone());
                result.staged_state.staged_files.insert(clean_to.clone(), content);
                result.staged_state.deleted_paths.remove(&clean_to);

                if !result.successful_paths.contains(&clean_from) {
                    result.successful_paths.push(clean_from);
                }
                if !result.successful_paths.contains(&clean_to) {
                    result.successful_paths.push(clean_to);
                }
                result.successful_edits.push(edit.clone());
            }
            EditOperation::Replace { path, .. }
            | EditOperation::Create { path, .. }
            | EditOperation::Delete { path }
            | EditOperation::Overwrite { path, .. } => {
                match context_manager.normalize_path(path) {
                    Ok(clean_path) => {
                        if !grouped.contains_key(&clean_path) {
                            file_order.push(clean_path.clone());
                        }
                        grouped.entry(clean_path).or_default().push(edit);
                    }
                    Err(err) => {
                        if invalid_paths.insert(path.clone()) {
                            result.failed_files.push(FailedFileEdit {
                                path: path.clone(),
                                error: EditError::InvalidPath(path.clone(), err.to_string()),
                            });
                        }
                    }
                }
            }
        }
    }

    flush_grouped(&mut grouped, &mut file_order, &mut result);
    result
}

/// None represents a staged deletion. Disk reads are deferred until a replace
/// needs the original content, so create/delete also work for non-text files.
fn stage_file_edits(
    repo_root: &Path,
    context_manager: &ContextManager,
    path: &str,
    edits: &[&EditOperation],
    initial_staged: &StagedEditsState,
) -> Result<Option<String>, EditError> {
    let invalid_path = |err: anyhow::Error| EditError::InvalidPath(path.to_string(), err.to_string());
    let in_context = context_manager.contains(path).map_err(invalid_path)?;
    let editable = context_manager.is_editable(path).map_err(invalid_path)?;
    let full_path = repo_root.join(path);
    let deleted = initial_staged.deleted_paths.contains(path);
    let mut content = if deleted {
        None
    } else {
        initial_staged.staged_files.get(path).cloned()
    };
    let mut loaded = deleted || content.is_some();
    // Files created in memory need not be added to context until disk commit.
    let mut newly_created = !full_path.exists() && content.is_some();

    for edit in edits {
        if in_context && !editable {
            return Err(EditError::ReadOnly(path.to_string()));
        }
        if !in_context && !newly_created {
            match edit {
                EditOperation::Create { .. } if !full_path.exists() => {}
                _ => return Err(EditError::NotInContext(path.to_string())),
            }
        }

        match edit {
            EditOperation::Replace { old_text, new_text, .. } => {
                if !loaded {
                    if !full_path.is_file() {
                        return Err(EditError::FileNotFound(path.to_string()));
                    }
                    content = Some(std::fs::read_to_string(&full_path)
                        .map_err(|err| EditError::Io(path.to_string(), err))?);
                    loaded = true;
                }
                let current = content.as_ref()
                    .ok_or_else(|| EditError::FileNotFound(path.to_string()))?;
                let updated = apply_replace(current, old_text, new_text, path)?;
                content = Some(updated);
            }
            EditOperation::Create { content: new_content, .. } => {
                content = Some(new_content.clone());
                loaded = true;
                newly_created = !full_path.exists();
            }
            EditOperation::Overwrite { content: new_content, .. } => {
                // Full replacement requires an existing (on disk or staged) file.
                if content.is_none() && (loaded || !full_path.is_file()) {
                    return Err(EditError::FileNotFound(path.to_string()));
                }
                content = Some(new_content.clone());
                loaded = true;
            }
            EditOperation::Delete { .. } => {
                content = None;
                loaded = true;
            }
            EditOperation::Move { .. } => {
                return Err(EditError::InvalidPath(
                    path.to_string(),
                    "Move operations cannot be staged as per-file edits".to_string(),
                ));
            }
        }
    }
    Ok(content)
}

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

    // 1. Level 1: Trim-End matching (trailing whitespace and empty line normalization)
    let mut trim_end_matches = Vec::new();
    for i in 0..=file_lines.len() - p_len {
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
    for i in 0..=file_lines.len() - p_len {
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

fn apply_replace(
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

/// Applies an EditProposal after validating every file in memory.
///
/// If ANY edit fails validation, no files on disk are modified. As before,
/// disk IO failures during commit are returned but are not transactionally rolled back.
pub fn apply_edit_proposal(
    repo_root: &Path,
    context_manager: &mut ContextManager,
    proposal: &EditProposal,
) -> Result<Vec<String>, EditError> {
    let result = stage_and_validate_edits(repo_root, context_manager, &proposal.edits, None);
    if let Some(failed) = result.failed_files.into_iter().next() {
        return Err(failed.error);
    }

    // Update context layer for moved files
    for edit in &proposal.edits {
        if let EditOperation::Move { from, to } = edit {
            if let (Ok(clean_from), Ok(clean_to)) = (
                context_manager.normalize_path(from),
                context_manager.normalize_path(to),
            ) {
                let _ = context_manager.rename_file(&clean_from, &clean_to);
            }
        }
    }

    // Commit only after all files have passed validation.
    for path in &result.successful_paths {
        let full_path = repo_root.join(path);
        if let Some(content) = result.staged_state.staged_files.get(path) {
            if let Some(parent) = full_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| EditError::Io(path.clone(), err))?;
            }
            std::fs::write(&full_path, content)
                .map_err(|err| EditError::Io(path.clone(), err))?;

            if context_manager.contains(path).unwrap_or(false) {
                let _ = context_manager.update_file_metadata(path);
            } else {
                let _ = context_manager.add_auto_file(path, tauqe_protocol::ContextAccess::Editable);
            }
        } else {
            match std::fs::remove_file(&full_path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(EditError::Io(path.clone(), err)),
            }
            let _ = context_manager.remove_file(path);
        }
    }
    Ok(result.successful_paths)
}

#[cfg(test)]
mod staging_tests {
    use super::*;
    use tempfile::tempdir;
    use tauqe_protocol::{ContextAccess, ContextLayer};

    fn replace(path: &str, old_text: &str, new_text: &str) -> EditOperation {
        EditOperation::Replace {
            path: path.to_string(),
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn partial_failure_rolls_back_entire_file_and_keeps_disk_unchanged() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "one").unwrap();
        std::fs::write(root.join("b.rs"), "two").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("a.rs", ContextAccess::Editable).unwrap();
        cm.add_file("b.rs", ContextAccess::Editable).unwrap();
        let before = cm.get_state().revision;
        let edits = vec![
            replace("a.rs", "one", "changed"),
            replace("b.rs", "two", "success"),
            replace("./a.rs", "missing", "failed"),
        ];
        let result = stage_and_validate_edits(root, &cm, &edits, None);
        assert_eq!(result.successful_paths, vec!["b.rs"]);
        assert_eq!(result.successful_edits, vec![edits[1].clone()]);
        assert_eq!(result.staged_state.staged_files["b.rs"], "success");
        assert!(!result.staged_state.staged_files.contains_key("a.rs"));
        assert_eq!(result.failed_files.len(), 1);
        assert_eq!(result.failed_files[0].path, "a.rs");
        assert!(matches!(&result.failed_files[0].error, EditError::NoMatch { .. }));
        assert_eq!(cm.get_state().revision, before);
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(root.join("b.rs")).unwrap(), "two");

        let proposal = EditProposal { summary: "test".into(), edits };
        assert!(apply_edit_proposal(root, &mut cm, &proposal).is_err());
        assert_eq!(std::fs::read_to_string(root.join("b.rs")).unwrap(), "two");
    }

    #[test]
    fn retries_use_staged_content_and_preserve_it_on_failure() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "one").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("a.rs", ContextAccess::Editable).unwrap();
        let first = stage_and_validate_edits(root, &cm, &[replace("a.rs", "one", "two")], None);
        let second = stage_and_validate_edits(
            root, &cm, &[replace("a.rs", "two", "three")], Some(&first.staged_state),
        );
        assert!(second.is_success());
        assert_eq!(second.staged_state.staged_files["a.rs"], "three");
        let failed = stage_and_validate_edits(
            root, &cm, &[replace("a.rs", "missing", "four")], Some(&second.staged_state),
        );
        assert!(!failed.is_success());
        assert_eq!(failed.staged_state, second.staged_state);
        assert_eq!(first.staged_state.staged_files["a.rs"], "two");
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one");
    }

    #[test]
    fn create_replace_delete_and_recreate_follow_operation_order() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let mut cm = ContextManager::new(root.to_path_buf());
        let create = EditOperation::Create { path: "new.rs".into(), content: "one".into() };
        let delete = EditOperation::Delete { path: "new.rs".into() };
        let first = stage_and_validate_edits(root, &cm, std::slice::from_ref(&create), None);
        let second = stage_and_validate_edits(
            root, &cm, &[replace("new.rs", "one", "two")], Some(&first.staged_state),
        );
        assert!(second.is_success());
        assert_eq!(second.staged_state.staged_files["new.rs"], "two");
        let deleted = stage_and_validate_edits(
            root, &cm, std::slice::from_ref(&delete), Some(&second.staged_state),
        );
        assert!(deleted.is_success());
        assert!(deleted.staged_state.deleted_paths.contains("new.rs"));
        assert!(!deleted.staged_state.staged_files.contains_key("new.rs"));
        assert!(!root.join("new.rs").exists());

        let proposal = EditProposal {
            summary: "create".into(),
            edits: vec![create.clone(), delete, create, replace("new.rs", "one", "final")],
        };
        assert_eq!(apply_edit_proposal(root, &mut cm, &proposal).unwrap(), vec!["new.rs"]);
        assert_eq!(std::fs::read_to_string(root.join("new.rs")).unwrap(), "final");
        assert!(cm.is_editable("new.rs").unwrap());
        let state = cm.get_state();
        let new_item = state.items.iter().find(|i| i.path == "new.rs").unwrap();
        assert_eq!(new_item.layer, ContextLayer::Auto);
        let deletion = EditProposal {
            summary: "delete".into(),
            edits: vec![EditOperation::Delete { path: "new.rs".into() }],
        };
        apply_edit_proposal(root, &mut cm, &deletion).unwrap();
        assert!(!root.join("new.rs").exists());
        assert!(!cm.contains("new.rs").unwrap());
    }

    #[test]
    fn permissions_paths_missing_files_and_ambiguous_matches_are_reported() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("readonly.rs"), "one").unwrap();
        std::fs::write(root.join("outside.rs"), "one").unwrap();
        std::fs::write(root.join("ambiguous.rs"), "one one").unwrap();
        std::fs::write(root.join("missing.rs"), "one").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("readonly.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_file("ambiguous.rs", ContextAccess::Editable).unwrap();
        cm.add_file("missing.rs", ContextAccess::Editable).unwrap();
        std::fs::remove_file(root.join("missing.rs")).unwrap();
        for edit in [
            replace("readonly.rs", "one", "two"),
            EditOperation::Create { path: "readonly.rs".into(), content: "two".into() },
            EditOperation::Delete { path: "readonly.rs".into() },
        ] {
            let result = stage_and_validate_edits(root, &cm, &[edit], None);
            assert!(matches!(&result.failed_files[0].error, EditError::ReadOnly(_)));
        }
        for edit in [
            replace("outside.rs", "one", "two"),
            EditOperation::Create { path: "outside.rs".into(), content: "two".into() },
            EditOperation::Delete { path: "outside.rs".into() },
        ] {
            let result = stage_and_validate_edits(root, &cm, &[edit], None);
            assert!(matches!(&result.failed_files[0].error, EditError::NotInContext(_)));
        }
        let ambiguous = stage_and_validate_edits(root, &cm, &[replace("ambiguous.rs", "one", "two")], None);
        assert!(matches!(&ambiguous.failed_files[0].error, EditError::AmbiguousMatch { count: 2, .. }));
        let missing = stage_and_validate_edits(root, &cm, &[replace("missing.rs", "one", "two")], None);
        assert!(matches!(&missing.failed_files[0].error, EditError::FileNotFound(_)));
        let invalid = stage_and_validate_edits(root, &cm, &[replace("../escape.rs", "one", "two")], None);
        assert!(matches!(&invalid.failed_files[0].error, EditError::InvalidPath(_, _)));
    }

    #[test]
    fn overwrite_replaces_existing_file_and_requires_context() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "one").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("a.rs", ContextAccess::Editable).unwrap();

        let op = EditOperation::Overwrite { path: "a.rs".into(), content: "all new".into() };
        let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&op), None);
        assert!(result.is_success());
        assert_eq!(result.staged_state.staged_files["a.rs"], "all new");
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one");

        let missing = EditOperation::Overwrite { path: "missing.rs".into(), content: "x".into() };
        let result = stage_and_validate_edits(root, &cm, &[missing], None);
        assert!(matches!(&result.failed_files[0].error, EditError::NotInContext(_)));

        let proposal = EditProposal { summary: "overwrite".into(), edits: vec![op] };
        apply_edit_proposal(root, &mut cm, &proposal).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "all new");
    }

    #[test]
    fn test_move_file_stages_and_applies() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("old.rs"), "fn hello() {}").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("old.rs", ContextAccess::Editable).unwrap();

        let move_op = EditOperation::Move {
            from: "old.rs".into(),
            to: "new.rs".into(),
        };

        let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&move_op), None);
        assert!(result.is_success());
        assert_eq!(result.staged_state.staged_files["new.rs"], "fn hello() {}");
        assert!(result.staged_state.deleted_paths.contains("old.rs"));

        let proposal = EditProposal {
            summary: "rename".into(),
            edits: vec![move_op],
        };
        apply_edit_proposal(root, &mut cm, &proposal).unwrap();
        assert!(!root.join("old.rs").exists());
        assert_eq!(std::fs::read_to_string(root.join("new.rs")).unwrap(), "fn hello() {}");
        assert!(cm.is_editable("new.rs").unwrap());
        assert!(!cm.contains("old.rs").unwrap());
    }

    #[test]
    fn fuzzy_replace_tolerates_trailing_whitespace_and_blank_line_spaces() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("code.rs"), "fn foo() {   \n    \n    bar();\n}\n").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("code.rs", ContextAccess::Editable).unwrap();

        // Model sent search text without trailing spaces and with clean empty line
        let edit = replace("code.rs", "fn foo() {\n\n    bar();\n}\n", "fn foo() {\n    baz();\n}\n");
        let result = stage_and_validate_edits(root, &cm, &[edit], None);
        assert!(result.is_success());
        assert_eq!(result.staged_state.staged_files["code.rs"], "fn foo() {\n    baz();\n}\n");
    }

    #[test]
    fn fuzzy_replace_adjusts_leading_indentation_shift() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let file_content = "impl Test {\n        fn run() {\n            let a = 1;\n            let b = 2;\n        }\n}\n";
        std::fs::write(root.join("indent.rs"), file_content).unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("indent.rs", ContextAccess::Editable).unwrap();

        // Model sent search text with 0-space base indentation instead of 8-space
        let old = "fn run() {\n    let a = 1;\n    let b = 2;\n}\n";
        let new = "fn run() {\n    let a = 1;\n    let b = 42;\n}\n";
        let edit = replace("indent.rs", old, new);
        let result = stage_and_validate_edits(root, &cm, &[edit], None);
        assert!(result.is_success());
        let staged = &result.staged_state.staged_files["indent.rs"];
        assert_eq!(staged, "impl Test {\n        fn run() {\n            let a = 1;\n            let b = 42;\n        }\n}\n");
    }

    #[test]
    fn fuzzy_replace_rejects_ambiguous_matches() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let file_content = "fn a() {\n    foo();\n}\nfn b() {\n    foo();\n}\n";
        std::fs::write(root.join("dup.rs"), file_content).unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("dup.rs", ContextAccess::Editable).unwrap();

        // Matches both a() and b() with whitespace normalization
        let edit = replace("dup.rs", "    foo();  \n", "    bar();\n");
        let result = stage_and_validate_edits(root, &cm, &[edit], None);
        assert!(!result.is_success());
        assert!(matches!(&result.failed_files[0].error, EditError::AmbiguousMatch { count: 2, .. }));
    }

    #[test]
    fn staging_preserves_crlf_and_empty_attempt_preserves_state() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "one\r\n").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("a.rs", ContextAccess::Editable).unwrap();
        let result = stage_and_validate_edits(root, &cm, &[replace("a.rs", "one\n", "two\n")], None);
        assert!(result.is_success());
        assert_eq!(result.staged_state.staged_files["a.rs"], "two\r\n");
        let empty = stage_and_validate_edits(root, &cm, &[], Some(&result.staged_state));
        assert!(empty.is_success());
        assert_eq!(empty.staged_state, result.staged_state);
        assert!(empty.successful_edits.is_empty());
    }
}
