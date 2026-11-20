//! Deterministic Codex patch parsing and atomic virtual application.
//!
//! Each section is resolved against the state produced by earlier sections. No
//! intermediate state is returned if parsing, matching or admission fails.

use std::path::Path;

use crate::toolchain::config::ProjectConfig;

use super::change_set::{
    stage_and_validate_edits_with, StageEditsResult, StagedConfigValidator, StagedEditsState,
};
use super::error::EditError;
use super::line_endings::align_line_endings;
use super::operation::{EditCreate, EditDelete, EditMove, EditOperation, EditOverwrite};

#[derive(Debug)]
enum Section {
    Add(String, String),
    Delete(String),
    Update {
        path: String,
        destination: Option<String>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Debug)]
struct Hunk {
    header_context: Option<String>,
    lines: Vec<(char, String)>,
}

/// Parses a Codex patch and stages it as one all-or-nothing virtual change set.
/// The caller retains its prior state on error; no working-tree bytes are written.
#[cfg(test)]
fn stage_apply_patch(
    repo_root: &Path,
    project: &ProjectConfig,
    context: &[String],
    patch: &str,
    initial_staged: Option<&StagedEditsState>,
    config_validator: Option<StagedConfigValidator>,
) -> Result<StageEditsResult, EditError> {
    stage_apply_patch_with_batches(
        repo_root,
        project,
        context,
        patch,
        initial_staged,
        config_validator,
    )
    .map(|(result, _)| result)
}

/// Retains the accepted section boundaries for replay before formatting.
pub fn stage_apply_patch_with_batches(
    repo_root: &Path,
    project: &ProjectConfig,
    context: &[String],
    patch: &str,
    initial_staged: Option<&StagedEditsState>,
    config_validator: Option<StagedConfigValidator>,
) -> Result<(StageEditsResult, Vec<Vec<EditOperation>>), EditError> {
    let sections = parse(patch)?;
    let mut state = initial_staged.cloned().unwrap_or_default();
    let mut successful_paths = Vec::new();
    let mut batches = Vec::new();
    for section in sections {
        let operations = match section {
            Section::Add(path, content) => {
                let path = normalize(repo_root, &path)?;
                if exists(&state, repo_root, &path) {
                    return Err(EditError::DestinationExists(path));
                }
                vec![EditOperation::Create(EditCreate { path, content })]
            }
            Section::Delete(path) => {
                let path = normalize(repo_root, &path)?;
                if !exists(&state, repo_root, &path) {
                    return Err(EditError::FileNotFound(path));
                }
                vec![EditOperation::Delete(EditDelete { path })]
            }
            Section::Update {
                path,
                destination,
                hunks,
            } => {
                let path = normalize(repo_root, &path)?;
                let original = read_text(&state, repo_root, &path)?;
                let content = apply_hunks(&path, &original, &hunks)?;
                let mut operations = vec![EditOperation::Overwrite(EditOverwrite {
                    path: path.clone(),
                    content,
                })];
                if let Some(destination) = destination {
                    let to = normalize(repo_root, &destination)?;
                    if to == path || exists(&state, repo_root, &to) {
                        return Err(EditError::DestinationExists(to));
                    }
                    operations.push(EditOperation::Move(EditMove { from: path, to }));
                }
                operations
            }
        };
        let result = stage_and_validate_edits_with(
            repo_root,
            project,
            context,
            &operations,
            Some(&state),
            config_validator,
        );
        if let Some(failure) = result.failed_files.into_iter().next() {
            return Err(failure.error);
        }
        state = result.staged_state;
        for path in result.successful_paths {
            if !successful_paths.contains(&path) {
                successful_paths.push(path);
            }
        }
        batches.push(result.successful_edits);
    }
    Ok((
        StageEditsResult {
            staged_state: state,
            successful_paths,
            successful_edits: batches.iter().flatten().cloned().collect(),
            failed_files: Vec::new(),
        },
        batches,
    ))
}

fn normalize(root: &Path, path: &str) -> Result<String, EditError> {
    crate::paths::normalize_repo_path(root, path)
        .map_err(|err| EditError::InvalidPath(path.to_string(), err.to_string()))
}

fn exists(state: &StagedEditsState, root: &Path, path: &str) -> bool {
    state.staged_files.contains_key(path)
        || (!state.deleted_paths.contains(path) && root.join(path).is_file())
}

fn read_text(state: &StagedEditsState, root: &Path, path: &str) -> Result<String, EditError> {
    if state.deleted_paths.contains(path) {
        return Err(EditError::FileNotFound(path.to_string()));
    }
    if let Some(bytes) = state.staged_files.get(path) {
        return String::from_utf8(bytes.clone()).map_err(|err| {
            EditError::Io(
                path.to_string(),
                std::io::Error::new(std::io::ErrorKind::InvalidData, err),
            )
        });
    }
    if !root.join(path).is_file() {
        return Err(EditError::FileNotFound(path.to_string()));
    }
    std::fs::read_to_string(root.join(path)).map_err(|err| EditError::Io(path.to_string(), err))
}

fn parse(patch: &str) -> Result<Vec<Section>, EditError> {
    let lines: Vec<&str> = patch.lines().collect();
    if lines.first() != Some(&"*** Begin Patch") {
        return Err(EditError::InvalidPatch("expected *** Begin Patch".into()));
    }
    let mut i = 1;
    let mut sections = Vec::new();
    while i < lines.len() && lines[i] != "*** End Patch" {
        let header = lines[i];
        i += 1;
        let section_error = |path: &str, line, reason| EditError::InvalidPatchSection {
            path: path.to_owned(),
            line,
            reason,
        };
        if let Some(path) = header.strip_prefix("*** Add File: ") {
            let mut body = String::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let line = lines[i]
                    .strip_prefix('+')
                    .ok_or_else(|| section_error(path, i + 1, "expected a line starting with +"))?;
                body.push_str(line);
                body.push('\n');
                i += 1;
            }
            sections.push(Section::Add(path.to_string(), body));
        } else if let Some(path) = header.strip_prefix("*** Delete File: ") {
            sections.push(Section::Delete(path.to_string()));
        } else if let Some(path) = header.strip_prefix("*** Update File: ") {
            let section_line = i;
            let destination = if i < lines.len() && lines[i].starts_with("*** Move to: ") {
                let to = lines[i]["*** Move to: ".len()..].to_string();
                i += 1;
                Some(to)
            } else {
                None
            };
            let mut hunks = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let hunk_line = i + 1;
                let marker = lines[i];
                if !marker.starts_with("@@") {
                    return Err(section_error(path, hunk_line, "expected @@"));
                }
                let header_context = marker.strip_prefix("@@ ").map(str::to_string);
                if marker != "@@" && header_context.is_none() {
                    return Err(section_error(path, hunk_line, "invalid hunk header"));
                }
                i += 1;
                let mut hunk = Hunk {
                    header_context,
                    lines: Vec::new(),
                };
                while i < lines.len()
                    && !lines[i].starts_with("*** ")
                    && !lines[i].starts_with("@@")
                {
                    let line = lines[i];
                    let Some((kind, text)) = line
                        .chars()
                        .next()
                        .map(|kind| (kind, &line[kind.len_utf8()..]))
                    else {
                        return Err(section_error(
                            path,
                            i + 1,
                            "expected a line starting with space, +, or -",
                        ));
                    };
                    if !matches!(kind, ' ' | '+' | '-') {
                        return Err(section_error(
                            path,
                            i + 1,
                            "expected a line starting with space, +, or -",
                        ));
                    }
                    hunk.lines.push((kind, text.to_string()));
                    i += 1;
                }
                if i < lines.len() && lines[i] == "*** End of File" {
                    i += 1;
                }
                if hunk.lines.is_empty() && hunk.header_context.is_none() {
                    return Err(section_error(
                        path,
                        hunk_line,
                        "remove this context-only hunk or include a + or - line",
                    ));
                }
                hunks.push(hunk);
            }
            if hunks.is_empty() && destination.is_none() {
                return Err(section_error(path, section_line, "update requires a hunk"));
            }
            if destination.is_none()
                && !hunks
                    .iter()
                    .flat_map(|h| &h.lines)
                    .any(|(kind, _)| *kind != ' ')
            {
                return Err(section_error(
                    path,
                    section_line + 1,
                    "context-only hunk: include an insertion or removal in this file update",
                ));
            }
            sections.push(Section::Update {
                path: path.to_string(),
                destination,
                hunks,
            });
        } else {
            return Err(EditError::InvalidPatch(format!(
                "unknown section: {header}"
            )));
        }
    }
    if lines.get(i) != Some(&"*** End Patch") || i + 1 != lines.len() || sections.is_empty() {
        return Err(EditError::InvalidPatch(
            "missing end marker, trailing data or empty patch".into(),
        ));
    }
    Ok(sections)
}

fn apply_hunks(path: &str, original: &str, hunks: &[Hunk]) -> Result<String, EditError> {
    let ending = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut lines: Vec<String> = original.split_inclusive('\n').map(str::to_string).collect();
    let mut cursor = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        let context_error = |section, context, count| EditError::PatchContext {
            path: path.into(),
            hunk: index + 1,
            section,
            context,
            count,
        };
        if let Some(context) = &hunk.header_context {
            let needle = [format!("{context}{ending}")];
            let start = find_context(&lines, cursor, &needle, ending)
                .map_err(|count| context_error("@@ context", context.clone(), count))?;
            cursor = start + 1;
        }
        let old: Vec<String> = hunk
            .lines
            .iter()
            .filter(|(kind, _)| *kind != '+')
            .map(|(_, text)| align_line_endings(&format!("{text}\n"), original))
            .collect();
        let at = if old.is_empty() {
            cursor
        } else {
            find_context(&lines, cursor, &old, ending)
                .map_err(|count| context_error("old lines", old.concat(), count))?
        };
        let mut source = lines[at..at + old.len()].iter();
        let mut new = Vec::new();
        for (kind, text) in &hunk.lines {
            match kind {
                '+' => new.push(align_line_endings(&format!("{text}\n"), original)),
                ' ' => new.extend(source.next().cloned()),
                _ => {
                    source.next();
                }
            }
        }
        // An unchanged final line may lack a newline. Give it a separator
        // only when the hunk inserts a line after it.
        let interior = new.len().saturating_sub(1);
        for line in &mut new[..interior] {
            if !line.ends_with('\n') {
                line.push_str(ending);
            }
        }
        let next = at + new.len();
        lines.splice(at..at + old.len(), new);
        cursor = next;
    }
    Ok(lines.concat())
}

// Exact matches take precedence. Only when none exist do we ignore leading
// and trailing horizontal whitespace, and the result must still be unique.
fn find_context(
    lines: &[String],
    cursor: usize,
    old: &[String],
    ending: &str,
) -> Result<usize, usize> {
    for ignore_whitespace in [false, true] {
        let matches: Vec<_> = lines[cursor..]
            .windows(old.len())
            .enumerate()
            .filter_map(|(offset, window)| {
                window
                    .iter()
                    .zip(old)
                    .all(|(actual, expected)| {
                        let actual = actual.strip_suffix(ending).unwrap_or(actual);
                        let expected = expected.strip_suffix(ending).unwrap_or(expected);
                        if ignore_whitespace {
                            actual.trim_matches([' ', '\t']) == expected.trim_matches([' ', '\t'])
                        } else {
                            actual == expected
                        }
                    })
                    .then_some(cursor + offset)
            })
            .collect();
        match matches.as_slice() {
            [at] => return Ok(*at),
            [] => {}
            _ => return Err(matches.len()),
        }
    }
    Err(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_file_patch_is_atomic_and_uses_staged_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "one\r\ntwo\r\n").unwrap();
        let context = vec!["a.txt".into()];
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n one\n-two\n+three\n*** Move to: invalid\n*** End Patch";
        assert!(
            stage_apply_patch(root, &ProjectConfig::default(), &context, patch, None, None)
                .is_err()
        );
        let first = stage_apply_patch(root, &ProjectConfig::default(), &context,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n one\n-two\n+three\n*** Add File: b.txt\n+new\n*** End Patch", None, None).unwrap();
        assert_eq!(
            first.staged_state.staged_files["a.txt"],
            b"one\r\nthree\r\n"
        );
        let second = stage_apply_patch(root, &ProjectConfig::default(), &context,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-three\n+four\n*** Delete File: b.txt\n*** End Patch", Some(&first.staged_state), None).unwrap();
        assert_eq!(
            second.staged_state.staged_files["a.txt"],
            b"one\r\nfour\r\n"
        );
        assert!(second.staged_state.deleted_paths.contains("b.txt"));
        assert_eq!(
            std::fs::read(root.join("a.txt")).unwrap(),
            b"one\r\ntwo\r\n"
        );
    }

    #[test]
    fn missing_context_and_bad_section_refuse_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "same\nsame\n").unwrap();
        let context = vec!["a.txt".into()];
        for patch in [
            "*** Begin Patch\n*** Add File: b.txt\n+new\n*** Update File: a.txt\n@@\n-same\n+other\n*** End Patch",
            "*** Begin Patch\n*** Add File: b.txt\n+new\n*** Bogus: a.txt\n*** End Patch",
            "*** Begin Patch\n*** Add File: ../escape\n+new\n*** End Patch",
        ] {
            assert!(stage_apply_patch(root, &ProjectConfig::default(), &context, patch, None, None).is_err());
            assert!(!root.join("b.txt").exists());
        }
    }

    #[test]
    fn whitespace_fallback_preserves_context_and_refuses_ambiguous_locations() {
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@  heading\n   keep\n-  old\n+new\n*** End Patch";
        let sections = parse(patch).unwrap();
        let [Section::Update { hunks, .. }] = sections.as_slice() else {
            panic!("expected an update");
        };
        assert_eq!(
            apply_hunks("a.txt", "heading\r\n\tkeep \r\nold\r\n", hunks).unwrap(),
            "heading\r\n\tkeep \r\nnew\r\n"
        );
        assert!(matches!(
            apply_hunks("a.txt", "heading\nkeep\nold\n\tkeep\n\told\n", hunks),
            Err(EditError::PatchContext { count: 2, .. })
        ));
    }

    #[test]
    fn move_after_update_and_insertions_are_virtual() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "first\nlast\n").unwrap();
        let context = vec!["a.txt".into()];
        let result = stage_apply_patch(root, &ProjectConfig::default(), &context,
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n@@ first\n+middle\n last\n*** End Patch", None, None).unwrap();
        assert_eq!(
            result.staged_state.staged_files["b.txt"],
            b"first\nmiddle\nlast\n"
        );
        assert!(result.staged_state.deleted_paths.contains("a.txt"));
    }
}
