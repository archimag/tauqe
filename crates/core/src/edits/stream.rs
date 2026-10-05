use std::collections::HashMap;

use crate::model::gateway::StreamEvent;

/// State-machine streaming parser for XML edits.
///
/// It intercepts XML edit blocks and emits semantic `StreamEvent::Edit*` events
/// while forwarding regular conversational text as `StreamEvent::TextDelta`.
pub struct XmlStreamFilter {
    buffer: String,
    in_edits_block: bool,
    current_file: Option<CurrentStreamingFile>,
    file_hunks_count: usize,
    editable_paths: Vec<String>,
    repo_root: std::path::PathBuf,
    // In-memory staged file state for validating consecutive hunks during streaming
    staged_contents: HashMap<String, String>,
}

struct CurrentStreamingFile {
    path: String,
    op_type: String, // "replace", "create", "delete"
}

impl XmlStreamFilter {
    pub fn new(editable_paths: Vec<String>, repo_root: std::path::PathBuf) -> Self {
        Self {
            buffer: String::new(),
            in_edits_block: false,
            current_file: None,
            file_hunks_count: 0,
            editable_paths,
            repo_root,
            staged_contents: HashMap::new(),
        }
    }

    /// Process incoming chunk of model text and generate corresponding `StreamEvent`s.
    pub fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();

        loop {
            if !self.in_edits_block {
                // Check if an edit block starts
                if let Some(tag_idx) = find_any_edit_start(&self.buffer) {
                    if tag_idx > 0 {
                        let text_part = self.buffer[..tag_idx].to_string();
                        events.push(StreamEvent::TextDelta(text_part));
                        self.buffer.drain(..tag_idx);
                    }

                    if self.buffer.starts_with("<workbench_edits>")
                        || self.buffer.starts_with("<workbench_edits")
                    {
                        if let Some(close_tag_bracket) = self.buffer.find('>') {
                            self.buffer.drain(..=close_tag_bracket);
                            self.in_edits_block = true;
                            events.push(StreamEvent::EditStarted);
                            continue;
                        } else {
                            // Incomplete opening tag, wait for more chunks
                            break;
                        }
                    } else {
                        // Tag like <edit path="..."> without wrapper
                        self.in_edits_block = true;
                        events.push(StreamEvent::EditStarted);
                        continue;
                    }
                } else {
                    // Safe to emit text if not holding a partial '<' at the very end
                    let emit_len = safe_text_emit_len(&self.buffer);
                    if emit_len > 0 {
                        let text_part = self.buffer[..emit_len].to_string();
                        events.push(StreamEvent::TextDelta(text_part));
                        self.buffer.drain(..emit_len);
                    }
                    break;
                }
            }

            // Inside edit block
            if self.current_file.is_none() {
                // Check if closing whole workbench_edits block
                if let Some(close_wb) = self.buffer.find("</workbench_edits>") {
                    self.buffer.drain(..close_wb + "</workbench_edits>".len());
                    self.in_edits_block = false;
                    continue;
                }

                // Look for start of a file tag: <edit, <create, <delete
                if let Some((tag_name, tag_start)) = find_file_tag_start(&self.buffer) {
                    if let Some(close_bracket) = self.buffer[tag_start..].find('>') {
                        let header = &self.buffer[tag_start..tag_start + close_bracket + 1];
                        let is_self_closing = header.ends_with("/>");

                        if let Some(path_attr) = extract_attr(header, "path") {
                            let resolved = resolve_path(&path_attr, &self.editable_paths);
                            let op_type = match tag_name.as_str() {
                                "create" => "create".to_string(),
                                "delete" => "delete".to_string(),
                                _ => "replace".to_string(),
                            };

                            events.push(StreamEvent::EditFileStarted {
                                path: resolved.clone(),
                                op_type: op_type.clone(),
                            });

                            self.buffer.drain(..tag_start + close_bracket + 1);
                            self.file_hunks_count = 0;

                            if is_self_closing || tag_name == "delete" {
                                events.push(StreamEvent::EditFileDone {
                                    path: resolved,
                                    status: "ok".to_string(),
                                    error: None,
                                    hunks_count: 0,
                                });
                                continue;
                            } else {
                                self.current_file = Some(CurrentStreamingFile {
                                    path: resolved,
                                    op_type,
                                });
                                continue;
                            }
                        } else {
                            // Missing path attribute
                            self.buffer.drain(..tag_start + close_bracket + 1);
                            continue;
                        }
                    } else {
                        // Incomplete header tag
                        break;
                    }
                } else {
                    // No new file tag found yet
                    break;
                }
            }

            // Processing content of current file
            if let Some((curr_path, curr_op_type)) = self
                .current_file
                .as_ref()
                .map(|c| (c.path.clone(), c.op_type.clone()))
            {
                let close_tag = if curr_op_type == "create" {
                    "</create>"
                } else {
                    "</edit>"
                };

                if curr_op_type == "create" {
                    if let Some(close_pos) = self.buffer.find(close_tag) {
                        let content = self.buffer[..close_pos].to_string();
                        self.buffer.drain(..close_pos + close_tag.len());

                        let norm_content = normalize_hunk(&content);

                        events.push(StreamEvent::EditHunk {
                            path: curr_path.clone(),
                            hunk_index: 0,
                            old_text: String::new(),
                            new_text: norm_content.clone(),
                        });

                        self.staged_contents.insert(curr_path.clone(), norm_content);

                        events.push(StreamEvent::EditFileDone {
                            path: curr_path,
                            status: "ok".to_string(),
                            error: None,
                            hunks_count: 1,
                        });

                        self.current_file = None;
                        continue;
                    } else {
                        break;
                    }
                } else {
                    // Replace mode: check for completed <search>...</search> <replace>...</replace>
                    if let Some((old_text, new_text, hunk_end)) = find_next_hunk(&self.buffer) {
                        let hunk_idx = self.file_hunks_count;
                        self.file_hunks_count += 1;

                        // Memory validation of this hunk against target file content
                        let val_res = self.validate_hunk_in_memory(&curr_path, &old_text, &new_text);

                        events.push(StreamEvent::EditHunk {
                            path: curr_path.clone(),
                            hunk_index: hunk_idx,
                            old_text,
                            new_text,
                        });

                        self.buffer.drain(..hunk_end);

                        if let Err(err_msg) = val_res {
                            events.push(StreamEvent::EditFileDone {
                                path: curr_path,
                                status: "error".to_string(),
                                error: Some(err_msg),
                                hunks_count: self.file_hunks_count,
                            });
                            self.current_file = None;
                            continue;
                        }
                        continue;
                    }

                    // Check if file closed
                    if let Some(close_pos) = self.buffer.find(close_tag) {
                        self.buffer.drain(..close_pos + close_tag.len());
                        events.push(StreamEvent::EditFileDone {
                            path: curr_path,
                            status: "ok".to_string(),
                            error: None,
                            hunks_count: self.file_hunks_count,
                        });
                        self.current_file = None;
                        continue;
                    } else {
                        break;
                    }
                }
            }
        }

        events
    }

    /// Flush any remaining buffered text when stream ends
    pub fn finish(mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            if let Some(curr) = self.current_file.take() {
                events.push(StreamEvent::EditFileDone {
                    path: curr.path,
                    status: "ok".to_string(),
                    error: None,
                    hunks_count: self.file_hunks_count,
                });
            }
            if !self.buffer.trim().is_empty() && !self.buffer.contains('<') {
                events.push(StreamEvent::TextDelta(self.buffer));
            }
        }
        events
    }

    fn validate_hunk_in_memory(
        &mut self,
        path: &str,
        old_text: &str,
        new_text: &str,
    ) -> Result<(), String> {
        let current_content = if let Some(content) = self.staged_contents.get(path) {
            content.clone()
        } else {
            let full_path = self.repo_root.join(path);
            std::fs::read_to_string(&full_path)
                .map_err(|e| format!("Cannot read file '{}': {}", path, e))?
        };

        let matches: Vec<(usize, &str)> = current_content.match_indices(old_text).collect();
        if matches.is_empty() {
            return Err("Search block not found in file".to_string());
        }
        if matches.len() > 1 {
            return Err(format!("Search block matches {} times (ambiguous)", matches.len()));
        }

        let (idx, _) = matches[0];
        let mut updated = String::new();
        updated.push_str(&current_content[..idx]);
        updated.push_str(new_text);
        updated.push_str(&current_content[idx + old_text.len()..]);
        self.staged_contents.insert(path.to_string(), updated);

        Ok(())
    }
}

fn safe_text_emit_len(buf: &str) -> usize {
    if let Some(last_lt) = buf.rfind('<') {
        let trailing = &buf[last_lt..];
        if "<workbench_edits>".starts_with(trailing)
            || "<edit".starts_with(trailing)
            || "<create".starts_with(trailing)
            || "<delete".starts_with(trailing)
        {
            return last_lt;
        }
    }
    buf.len()
}

fn find_any_edit_start(buf: &str) -> Option<usize> {
    let mut min_idx = None;
    for prefix in &["<workbench_edits", "<edit", "<create", "<delete"] {
        if let Some(idx) = buf.find(prefix) {
            min_idx = Some(min_idx.map_or(idx, |m: usize| m.min(idx)));
        }
    }
    min_idx
}

fn find_file_tag_start(buf: &str) -> Option<(String, usize)> {
    let mut candidates = Vec::new();
    for tag in &["edit", "create", "delete", "replace"] {
        let pat = format!("<{}", tag);
        let mut from = 0;
        while let Some(idx) = buf[from..].find(&pat) {
            let abs_idx = from + idx;
            let after = &buf[abs_idx + pat.len()..];
            if let Some(c) = after.chars().next() {
                if c.is_whitespace() || c == '>' {
                    candidates.push((tag.to_string(), abs_idx));
                    break;
                }
            }
            from = abs_idx + pat.len();
        }
    }
    candidates.into_iter().min_by_key(|(_, idx)| *idx)
}

fn find_next_hunk(buf: &str) -> Option<(String, String, usize)> {
    let search_start_pat = "<search>";
    let search_end_pat = "</search>";
    let s_start = buf.find(search_start_pat)?;
    let s_end = buf[s_start + search_start_pat.len()..].find(search_end_pat)?;
    let old_text_raw = &buf[s_start + search_start_pat.len()..s_start + search_start_pat.len() + s_end];

    let after_search = s_start + search_start_pat.len() + s_end + search_end_pat.len();
    let rest = &buf[after_search..];

    let (replace_start_pat, replace_end_pat) = if rest.contains("<replace>") {
        ("<replace>", "</replace>")
    } else if rest.contains("<with>") {
        ("<with>", "</with>")
    } else {
        return None;
    };

    let r_start = rest.find(replace_start_pat)?;
    let r_end = rest[r_start + replace_start_pat.len()..].find(replace_end_pat)?;
    let new_text_raw = &rest[r_start + replace_start_pat.len()..r_start + replace_start_pat.len() + r_end];

    let abs_end = after_search + r_start + replace_start_pat.len() + r_end + replace_end_pat.len();

    Some((
        normalize_hunk(old_text_raw),
        normalize_hunk(new_text_raw),
        abs_end,
    ))
}

fn extract_attr(header: &str, attr: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(pos) = header[search_from..].find(attr) {
        let abs_pos = search_from + pos;
        let after_attr = &header[abs_pos + attr.len()..];
        let trimmed = after_attr.trim_start();
        if trimmed.starts_with('=') {
            let after_eq = trimmed[1..].trim_start();
            let quote = after_eq.chars().next()?;
            if quote == '"' || quote == '\'' {
                let rest = &after_eq[quote.len_utf8()..];
                let end_quote = rest.find(quote)?;
                return Some(rest[..end_quote].trim().trim_matches('`').to_string());
            }
        }
        search_from = abs_pos + attr.len();
    }
    None
}

fn resolve_path(candidate: &str, editable_paths: &[String]) -> String {
    let cleaned = candidate.trim().trim_matches('`');
    for ed in editable_paths {
        if cleaned == ed || cleaned.ends_with(ed) {
            return ed.clone();
        }
    }
    cleaned.to_string()
}

fn normalize_hunk(raw: &str) -> String {
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
