use tauqe_protocol::{ContextAccess, EditOperation, ModelResult};

use super::utils::{normalize_content, resolve_target_path_for_op};
use super::{ContextRequest, EditProtocol};

/// XML-based code edit protocol
#[derive(Debug, Default, Clone)]
pub struct XmlEditProtocol;

impl EditProtocol for XmlEditProtocol {
    fn name(&self) -> &'static str {
        "xml"
    }

    fn with_turn_marker(&self, marker: &str) -> Option<Box<dyn EditProtocol>> {
        Some(Box::new(MarkedXmlEditProtocol::new(marker)))
    }

    fn parse_context_requests(&self, raw_text: &str) -> Vec<ContextRequest> {
        parse_context_request_tags(raw_text)
    }

    fn system_instructions(&self, editable_paths: &[String]) -> String {
        let mut prompt = String::new();
        prompt.push_str("## Code Modification Protocol (XML Edits)\n");
        prompt.push_str("When proposing changes, use structured XML blocks:\n");
        prompt.push_str("- Existing files: modify ONLY files listed in <editable_files> via <edit path=\"...\">.\n");
        prompt.push_str("- New files: create new files using <create path=\"...\"> with a valid repository-relative path. Created files are automatically added to context.\n");
        prompt.push_str("- Move or rename files: move existing editable files using <move from=\"...\" to=\"...\" />.\n");
        prompt
            .push_str("- File deletions: delete obsolete files using <delete path=\"...\" />.\n\n");

        if editable_paths.is_empty() {
            prompt.push_str("NOTE: No existing files are currently marked as editable. You may answer questions, explain code, or create new files with <create path=\"...\"> if requested.\n\n");
        } else {
            prompt.push_str("Currently permitted existing target files for modification:\n");
            for path in editable_paths {
                prompt.push_str(&format!("- {}\n", path));
            }
            prompt.push('\n');
        }

        let sample_path = editable_paths
            .first()
            .map(|s| s.as_str())
            .unwrap_or("example_file.rs");

        prompt.push_str("Format for proposing changes:\n");
        prompt.push_str("<tauqe_edits summary=\"Concise commit message in imperative mood, e.g. Add validation helper and update context manager\">\n");
        prompt.push_str("  <!-- To modify an existing file: -->\n");
        prompt.push_str(&format!("  <edit path=\"{}\">\n", sample_path));
        prompt.push_str("    <search>\n");
        prompt.push_str("exact lines from target file to be replaced\n");
        prompt.push_str("    </search>\n");
        prompt.push_str("    <replace>\n");
        prompt.push_str("new replacement lines\n");
        prompt.push_str("    </replace>\n");
        prompt.push_str("  </edit>\n\n");
        prompt.push_str("  <!-- To create a new file: -->\n");
        prompt.push_str("  <create path=\"path/to/new_file.rs\">\n");
        prompt.push_str("// Complete file content\n");
        prompt.push_str("  </create>\n\n");
        prompt.push_str("  <!-- To move or rename a file: -->\n");
        prompt.push_str("  <move from=\"path/to/old_file.rs\" to=\"path/to/new_file.rs\" />\n\n");
        prompt.push_str("  <!-- To delete a file: -->\n");
        prompt.push_str("  <delete path=\"path/to/file_to_delete.rs\" />\n");
        prompt.push_str("</tauqe_edits>\n\n");

        prompt.push_str("CRITICAL INVARIANTS:\n");
        prompt.push_str("1. Specify 'summary=\"...\"' on <tauqe_edits> with a high-quality Git commit message.\n");
        prompt.push_str("2. For <edit>, the 'path' attribute MUST EXACTLY match one of the paths listed in <editable_files>.\n");
        prompt.push_str("3. For <create>, specify a valid relative path within the repository.\n");
        prompt.push_str("4. The <search> block must match EXACTLY ONE location in the target file, including whitespace and line breaks.\n");
        prompt.push_str(
            "5. Keep <search> blocks as concise as possible while maintaining uniqueness.\n",
        );
        prompt.push_str("6. You may include multiple <edit>, <create>, or <delete> blocks inside <tauqe_edits>.\n");
        prompt.push_str("7. If no code changes are needed (e.g. conversational answer or explanation), output plain text without any XML edit tags.\n");
        prompt.push_str("8. If the task requires files listed in <repo_map> that are not yet present in the context files, request them FIRST instead of guessing: output only a short note plus one tag per file, e.g. <context_request path=\"path/to/file.rs\" access=\"read_only\" /> (use access=\"editable\" for files you need to modify). You will be called again with the full file contents added to context. Never request files that are already in context.\n");
        prompt.push_str("9. Code Verification: You may request code verification using <verify target=\"all|check|test|clippy\" on_success=\"silent|report\" />. Use target=\"all\" (or check, test, clippy). Always use on_success=\"silent\" unless the user explicitly asked to see the raw test/build logs or command output. Do not quote or output raw logs when verification succeeds; concise confirmation that all checks passed is sufficient.\n");
        prompt.push_str("10. Detect User Language: Identify the primary language of the user's prompt and output a <user_language>language</user_language> tag (e.g. <user_language>Russian</user_language>). All conversational explanations MUST be in this detected language. Internal thoughts and reasoning MUST be strictly in English.\n\n");

        prompt
    }

    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult {
        // Fallback check for tauqe_edit JSON block
        if let Some(json_res) = crate::edits::parse_tauqe_edit_json(raw_text) {
            return json_res;
        }

        if !has_xml_edit_tags(raw_text) {
            return ModelResult::Answer {
                text: strip_verify_tags(&strip_user_language_tags(&strip_context_request_tags(raw_text))),
            };
        }

        let extracted_summary = extract_summary_from_xml(raw_text);

        match parse_xml_edits(raw_text, editable_paths) {
            Ok(edits) => {
                if edits.is_empty() {
                    ModelResult::Answer {
                        text: raw_text.to_string(),
                    }
                } else {
                    let summary =
                        extracted_summary.unwrap_or_else(|| "Apply AI code changes".to_string());
                    ModelResult::Edit {
                        summary,
                        edits,
                        proposal: None,
                        applied: false,
                        error: None,
                        changed_files: Vec::new(),
                        commit_hash: None,
                    }
                }
            }
            Err(err_msg) => ModelResult::Edit {
                summary: "Malformed XML edit block".to_string(),
                edits: Vec::new(),
                proposal: None,
                applied: false,
                error: Some(err_msg),
                changed_files: Vec::new(),
                commit_hash: None,
            },
        }
    }
}

/// Generates a process-unique alphanumeric marker without an additional dependency.
pub fn generate_turn_marker() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQUENCE: OnceLock<AtomicU64> = OnceLock::new();
    let sequence = SEQUENCE.get_or_init(|| {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        AtomicU64::new(seed)
    });
    format!("{:016X}", sequence.fetch_add(1, Ordering::Relaxed))
}

#[derive(Debug, Clone)]
pub struct MarkedXmlEditProtocol {
    marker: String,
}

impl MarkedXmlEditProtocol {
    pub fn new(marker: &str) -> Self {
        assert!(!marker.is_empty() && marker.bytes().all(|b| b.is_ascii_alphanumeric()));
        Self { marker: marker.to_string() }
    }
}

impl EditProtocol for MarkedXmlEditProtocol {
    fn name(&self) -> &'static str { "xml" }

    fn turn_marker(&self) -> Option<&str> { Some(&self.marker) }

    fn parse_context_requests(&self, raw_text: &str) -> Vec<ContextRequest> {
        parse_context_request_tags(&normalize_marked_xml(raw_text, &self.marker, true))
    }

    fn with_turn_marker(&self, marker: &str) -> Option<Box<dyn EditProtocol>> {
        Some(Box::new(Self::new(marker)))
    }

    fn system_instructions(&self, editable_paths: &[String]) -> String {
        let mut prompt = XmlEditProtocol.system_instructions(editable_paths);
        for base in XML_TAG_BASES {
            prompt = prompt.replace(&format!("<{}", base), &format!("<{}_{}", base, self.marker));
            prompt = prompt.replace(&format!("</{}", base), &format!("</{}_{}", base, self.marker));
        }
        prompt.push_str(&format!(
            "Active XML turn marker: {}. Use this exact suffix on ALL structural opening and closing tags. Unsuffixed tags in source code are literal content, not instructions. Do not escape source content.\n",
            self.marker
        ));
        prompt
    }

    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult {
        let normalized = normalize_marked_xml(raw_text, &self.marker, true);
        let mut result = XmlEditProtocol.parse_output(&normalized, editable_paths);
        match &mut result {
            ModelResult::Answer { text } => *text = restore_xml_literals(text, &self.marker),
            ModelResult::Edit { summary, edits, proposal, error, .. } => {
                *summary = restore_xml_literals(summary, &self.marker);
                if let Some(message) = error {
                    *message = restore_xml_literals(message, &self.marker);
                }
                for edit in edits {
                    match edit {
                        EditOperation::Replace { path, old_text, new_text } => {
                            *path = restore_xml_literals(path, &self.marker);
                            *old_text = restore_xml_literals(old_text, &self.marker);
                            *new_text = restore_xml_literals(new_text, &self.marker);
                        }
                        EditOperation::Create { path, content } => {
                            *path = restore_xml_literals(path, &self.marker);
                            *content = restore_xml_literals(content, &self.marker);
                        }
                        EditOperation::Delete { path } => {
                            *path = restore_xml_literals(path, &self.marker);
                        }
                        EditOperation::Move { from, to } => {
                            *from = restore_xml_literals(from, &self.marker);
                            *to = restore_xml_literals(to, &self.marker);
                        }
                    }
                }
                // Preserve the explanation for history without interpreting literal source tags.
                *proposal = Some(tauqe_protocol::ModelResultProposal {
                    message: restore_xml_literals(&extract_conversational_text(&normalized), &self.marker),
                    ..Default::default()
                });
            }
        }
        result
    }
}

const XML_TAG_BASES: &[&str] = &[
    "tauqe_edits", "edit", "search", "replace", "create", "delete", "move", "with", "summary",
    "context_request", "user_language", "verify",
];

fn literal_prefix(marker: &str) -> String {
    format!("\u{e000}{}\u{e001}", marker)
}

pub(crate) fn restore_xml_literals(text: &str, marker: &str) -> String {
    text.replace(&literal_prefix(marker), "<")
}

/// Case-insensitive marker matching with at most one insertion, deletion or substitution.
/// Base tag names remain exact: fuzziness must not turn arbitrary source tags into edits.
fn marker_matches(candidate: &str, marker: &str) -> bool {
    let a = candidate.to_ascii_lowercase();
    let b = marker.to_ascii_lowercase();
    if a == b { return true; }
    if a.len().abs_diff(b.len()) > 1 || b.len() < 4 { return false; }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j, mut errors) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else {
            errors += 1;
            if errors > 1 { return false; }
            if a.len() >= b.len() { i += 1; }
            if b.len() >= a.len() { j += 1; }
        }
    }
    errors + (a.len() - i) + (b.len() - j) <= 1
}

/// Converts only active structural tags to the legacy parser's dialect, protecting
/// literal '<' characters from its delimiter searches. Used by BOTH final and stream
/// parsing. An incomplete trailing token is withheld so the returned prefix is stable.
/// Unsuffixed tags in source code are protected as literal content, never closing active blocks.
pub(crate) fn normalize_marked_xml(text: &str, marker: &str, finished: bool) -> String {
    let mut output = String::new();
    let mut stack: Vec<&str> = Vec::new();
    let protection = literal_prefix(marker);
    let mut pos = 0;
    while pos < text.len() {
        let Some(offset) = text[pos..].find('<') else {
            output.push_str(&text[pos..]);
            break;
        };
        let start = pos + offset;
        output.push_str(&text[pos..start]);
        let closing = text[start..].starts_with("</");
        let name_start = start + if closing { 2 } else { 1 };
        let name_len = text[name_start..].bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
            .count();
        let name_end = name_start + name_len;
        if name_end == text.len() && !finished { break; }
        let name = &text[name_start..name_end];
        let active = XML_TAG_BASES.iter().copied().find(|base| {
            name.strip_prefix(base)
                .and_then(|rest| rest.strip_prefix('_').or_else(|| rest.strip_prefix('-')))
                .map(|suffix| marker_matches(suffix, marker))
                .unwrap_or(false)
        });
        let boundary = text[name_end..].chars().next()
            .map(|c| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(false);
        if let (true, Some(base)) = (boundary, active) {
            // Find the end of the header without mistaking quoted '>' for its delimiter.
            let mut quote = None;
            let mut end = None;
            for (idx, ch) in text[name_end..].char_indices() {
                if let Some(q) = quote {
                    if ch == q { quote = None; }
                } else if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                } else if ch == '>' {
                    end = Some(name_end + idx + 1);
                    break;
                }
            }
            let Some(end) = end else {
                if !finished { break; }
                output.push_str(&protection);
                pos = start + 1;
                continue;
            };
            let accepted = !closing || stack.last().copied() == Some(base);
            if accepted {
                output.push('<');
                if closing { output.push('/'); }
                output.push_str(base);
                output.push_str(&text[name_end..end]);
                if closing {
                    stack.pop();
                } else if !text[name_end..end].trim_end().ends_with("/>") {
                    stack.push(base);
                }
                pos = end;
                continue;
            }
        }
        output.push_str(&protection);
        pos = start + 1;
    }
    output
}

/// Locates the next complete `<context_request .../>` tag (unsuffixed dialect).
/// Returns the byte range `(start, end)` including an immediately following
/// `</context_request>` for the paired form. Incomplete tags yield None.
pub(crate) fn next_context_request_tag(text: &str) -> Option<(usize, usize)> {
    const OPEN: &str = "<context_request";
    let mut from = 0;
    while let Some(rel) = text[from..].find(OPEN) {
        let start = from + rel;
        let after = start + OPEN.len();
        let bytes = &text.as_bytes()[after..];
        let mut idx = 0;
        if idx < bytes.len() && (bytes[idx] == b'_' || bytes[idx] == b'-') {
            idx += 1;
            while idx < bytes.len()
                && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-')
            {
                idx += 1;
            }
        }
        let tag_name = &text[start + 1..after + idx];
        let rest = &text[after + idx..];
        let boundary = rest
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if boundary {
            let gt = rest.find('>')?;
            let mut end = after + idx + gt + 1;
            if !rest[..gt].trim_end().ends_with('/') {
                let close_tag = format!("</{}>", tag_name);
                let after_open = &text[end..];
                let trimmed = after_open.trim_start();
                if trimmed.starts_with(&close_tag) {
                    end += (after_open.len() - trimmed.len()) + close_tag.len();
                }
            }
            return Some((start, end));
        }
        from = after;
    }
    None
}

/// Locates the next complete `<user_language>...</user_language>` or self-closing tag.
pub(crate) fn next_user_language_tag(text: &str) -> Option<(usize, usize)> {
    const OPEN: &str = "<user_language";
    let mut from = 0;
    while let Some(rel) = text[from..].find(OPEN) {
        let start = from + rel;
        let after = start + OPEN.len();
        let bytes = &text.as_bytes()[after..];
        let mut idx = 0;
        if idx < bytes.len() && (bytes[idx] == b'_' || bytes[idx] == b'-') {
            idx += 1;
            while idx < bytes.len()
                && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-')
            {
                idx += 1;
            }
        }
        let tag_name = &text[start + 1..after + idx];
        let rest = &text[after + idx..];
        let boundary = rest
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if boundary {
            let gt = rest.find('>')?;
            let end_open = after + idx + gt + 1;
            if rest[..gt].trim_end().ends_with('/') {
                return Some((start, end_open));
            }
            let close_tag = format!("</{}>", tag_name);
            let close_offset = text[end_open..].find(&close_tag)?;
            let end = end_open + close_offset + close_tag.len();
            return Some((start, end));
        }
        from = after;
    }
    None
}

/// Extracts user language tag content if present (e.g. `<user_language>Russian</user_language>`
/// or `<user_language lang="ru"/>`).
pub fn extract_user_language(text: &str) -> Option<String> {
    let (start, tag_name) = find_base_tag_start(text, "user_language")?;
    let slice = &text[start..];
    let open_end = slice.find('>')?;
    let header = &slice[..open_end + 1];
    if let Some(val) = extract_attribute(header, "value")
        .or_else(|| extract_attribute(header, "lang"))
    {
        let trimmed = val.trim();
        if !trimmed.is_empty() && trimmed.len() < 50 && !trimmed.contains('\n') {
            return Some(trimmed.to_string());
        }
    }
    let close_tag = format!("</{}>", tag_name);
    let close_start = slice.find(&close_tag)?;
    if close_start >= open_end {
        let content = slice[open_end + 1..close_start].trim();
        if !content.is_empty() && content.len() < 50 && !content.contains('\n') {
            return Some(content.to_string());
        }
    }
    None
}

/// Removes all user_language tags from text.
pub fn strip_user_language_tags(text: &str) -> String {
    let mut result = text.to_string();
    while let Some((start, tag_name)) = find_base_tag_start(&result, "user_language") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(close_offset) = result[start..].find(&close_tag) {
            let end = start + close_offset + close_tag.len();
            result.replace_range(start..end, "");
        } else if let Some(gt_offset) = result[start..].find('>') {
            let end = start + gt_offset + 1;
            result.replace_range(start..end, "");
        } else {
            result.replace_range(start.., "");
            break;
        }
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyTarget {
    All,
    Check,
    Test,
    Clippy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOnSuccess {
    Silent,
    Report,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyRequest {
    pub target: VerifyTarget,
    pub on_success: VerifyOnSuccess,
}

/// Locates the next complete `<verify .../>` or `<verify>...</verify>` tag.
pub(crate) fn next_verify_tag(text: &str) -> Option<(usize, usize)> {
    const OPEN: &str = "<verify";
    let mut from = 0;
    while let Some(rel) = text[from..].find(OPEN) {
        let start = from + rel;
        let after = start + OPEN.len();
        let bytes = &text.as_bytes()[after..];
        let mut idx = 0;
        if idx < bytes.len() && (bytes[idx] == b'_' || bytes[idx] == b'-') {
            idx += 1;
            while idx < bytes.len()
                && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-')
            {
                idx += 1;
            }
        }
        let tag_name = &text[start + 1..after + idx];
        let rest = &text[after + idx..];
        let boundary = rest
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if boundary {
            let gt = rest.find('>')?;
            let mut end = after + idx + gt + 1;
            if !rest[..gt].trim_end().ends_with('/') {
                let close_tag = format!("</{}>", tag_name);
                let after_open = &text[end..];
                let trimmed = after_open.trim_start();
                if trimmed.starts_with(&close_tag) {
                    end += (after_open.len() - trimmed.len()) + close_tag.len();
                }
            }
            return Some((start, end));
        }
        from = after;
    }
    None
}

/// Extracts verification request if specified by the model.
pub fn extract_verify_request(text: &str) -> Option<VerifyRequest> {
    let (start, end) = next_verify_tag(text)?;
    let tag = &text[start..end];
    let header_end = tag.find('>').unwrap_or(tag.len());
    let header = &tag[..header_end];

    let target = match extract_attribute(header, "target").as_deref() {
        Some("check") => VerifyTarget::Check,
        Some("test") => VerifyTarget::Test,
        Some("clippy") => VerifyTarget::Clippy,
        _ => VerifyTarget::All,
    };

    let on_success = match extract_attribute(header, "on_success").as_deref() {
        Some("silent") => VerifyOnSuccess::Silent,
        _ => VerifyOnSuccess::Report,
    };

    Some(VerifyRequest { target, on_success })
}

/// Removes all verify tags from text.
pub fn strip_verify_tags(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some((start, end)) = next_verify_tag(rest) {
        out.push_str(&rest[..start]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Removes all context request tags from text.
pub fn strip_context_request_tags(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some((start, end)) = next_context_request_tag(rest) {
        out.push_str(&rest[..start]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn parse_context_request_tags(text: &str) -> Vec<ContextRequest> {
    let mut requests = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = next_context_request_tag(&text[pos..]) {
        let tag = &text[pos + start..pos + end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        if let Some(path) = extract_attribute(header, "path") {
            let path = path
                .trim()
                .trim_start_matches("./")
                .trim_start_matches('/')
                .to_string();
            if !path.is_empty() {
                let editable = extract_attribute(header, "access")
                    .is_some_and(|a| a.trim().eq_ignore_ascii_case("editable"));
                requests.push(ContextRequest {
                    path,
                    access: if editable {
                        ContextAccess::Editable
                    } else {
                        ContextAccess::ReadOnly
                    },
                });
            }
        }
        pos += end;
    }
    requests
}

pub fn has_xml_edit_tags(text: &str) -> bool {
    text.contains("<edit")
        || text.contains("<replace")
        || text.contains("<create")
        || text.contains("<delete")
        || text.contains("<move")
        || text.contains("<tauqe_edits")
}

/// Extracts conversational text from model output by stripping edit tags
/// (<tauqe_edits>, <edit>, <create>, <delete>, etc.) and cleaning up extra whitespace.
pub fn extract_conversational_text(raw_text: &str) -> String {
    let mut text = raw_text.to_string();

    // 1. Remove ```tauqe_edit ... ``` JSON fenced blocks if present
    while let Some(start_idx) = text.find("```tauqe_edit") {
        let after_start = start_idx + "```tauqe_edit".len();
        if let Some(end_idx) = text[after_start..].find("```") {
            let total_end = after_start + end_idx + 3;
            text.replace_range(start_idx..total_end, "");
        } else {
            text.replace_range(start_idx.., "");
            break;
        }
    }

    // 2. Remove <tauqe_edits>...</tauqe_edits> blocks (and enclosing fences if any)
    while let Some((start, tag_name)) = find_base_tag_start(&text, "tauqe_edits") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(close_offset) = text[start..].find(&close_tag) {
            let end = start + close_offset + close_tag.len();
            let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
            text.replace_range(exp_start..exp_end, "");
        } else if let Some(gt_offset) = text[start..].find('>') {
            let header = &text[start..start + gt_offset + 1];
            if header.trim_end().ends_with("/>") {
                let end = start + gt_offset + 1;
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
                text.replace_range(exp_start..exp_end, "");
            } else {
                let end = start + gt_offset + 1;
                text.replace_range(start..end, "");
            }
        } else {
            text.replace_range(start.., "");
            break;
        }
    }

    // 3. Remove standalone <edit ...>...</edit>
    while let Some((start, tag_name)) = find_base_tag_start(&text, "edit") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(close_offset) = text[start..].find(&close_tag) {
            let end = start + close_offset + close_tag.len();
            let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
            text.replace_range(exp_start..exp_end, "");
        } else if let Some(gt_offset) = text[start..].find('>') {
            let end = start + gt_offset + 1;
            text.replace_range(start..end, "");
        } else {
            text.replace_range(start.., "");
            break;
        }
    }

    // 4. Remove standalone <create ...>...</create>
    while let Some((start, tag_name)) = find_base_tag_start(&text, "create") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(close_offset) = text[start..].find(&close_tag) {
            let end = start + close_offset + close_tag.len();
            let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
            text.replace_range(exp_start..exp_end, "");
        } else if let Some(gt_offset) = text[start..].find('>') {
            let end = start + gt_offset + 1;
            text.replace_range(start..end, "");
        } else {
            text.replace_range(start.., "");
            break;
        }
    }

    // 5. Remove standalone <delete .../> or <delete ...>...</delete>
    while let Some((start, tag_name)) = find_base_tag_start(&text, "delete") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(gt_offset) = text[start..].find('>') {
            let tag_open_end = start + gt_offset + 1;
            let header = &text[start..tag_open_end];
            if header.trim_end().ends_with("/>") {
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, tag_open_end);
                text.replace_range(exp_start..exp_end, "");
            } else if let Some(close_offset) = text[start..].find(&close_tag) {
                let end = start + close_offset + close_tag.len();
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
                text.replace_range(exp_start..exp_end, "");
            } else {
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, tag_open_end);
                text.replace_range(exp_start..exp_end, "");
            }
        } else {
            text.replace_range(start.., "");
            break;
        }
    }

    // 5b. Remove standalone <move .../> or <move ...>...</move>
    while let Some((start, tag_name)) = find_base_tag_start(&text, "move") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(gt_offset) = text[start..].find('>') {
            let tag_open_end = start + gt_offset + 1;
            let header = &text[start..tag_open_end];
            if header.trim_end().ends_with("/>") {
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, tag_open_end);
                text.replace_range(exp_start..exp_end, "");
            } else if let Some(close_offset) = text[start..].find(&close_tag) {
                let end = start + close_offset + close_tag.len();
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
                text.replace_range(exp_start..exp_end, "");
            } else {
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, tag_open_end);
                text.replace_range(exp_start..exp_end, "");
            }
        } else {
            text.replace_range(start.., "");
            break;
        }
    }

    // 6. Remove standalone <replace path="...">...</replace> if any
    while let Some((start, tag_name)) = find_base_tag_start(&text, "replace") {
        if let Some(gt_offset) = text[start..].find('>') {
            let header = &text[start..start + gt_offset + 1];
            if header.contains("path") {
                let close_tag = format!("</{}>", tag_name);
                if let Some(close_offset) = text[start..].find(&close_tag) {
                    let end = start + close_offset + close_tag.len();
                    let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
                    text.replace_range(exp_start..exp_end, "");
                    continue;
                }
            }
        }
        break;
    }

    let text = strip_user_language_tags(&text);
    let text = strip_verify_tags(&text);
    let text = strip_context_request_tags(&text);
    clean_conversational_lines(&text)
}

fn expand_fence_bounds(text: &str, start: usize, end: usize) -> (usize, usize) {
    let before = &text[..start];
    let after = &text[end..];

    let mut exp_start = start;
    let mut exp_end = end;

    let trimmed_before = before.trim_end();
    if let Some(last_newline) = trimmed_before.rfind('\n') {
        let line_before = &trimmed_before[last_newline + 1..];
        if line_before.trim().starts_with("```") {
            exp_start = last_newline + 1;
        }
    } else if trimmed_before.trim().starts_with("```") {
        exp_start = 0;
    }

    let after_lines: Vec<&str> = after.lines().collect();
    let mut has_closing_fence = false;
    for line in &after_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "```" {
            has_closing_fence = true;
        }
        break;
    }

    if has_closing_fence {
        if let Some(fence_pos) = after.find("```") {
            let offset = fence_pos + 3;
            let skip_nl = if after[offset..].starts_with("\r\n") {
                2
            } else if after[offset..].starts_with('\n') {
                1
            } else {
                0
            };
            exp_end = end + offset + skip_nl;
        }
    }

    (exp_start, exp_end)
}

fn clean_conversational_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result_lines = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.starts_with("```") && trimmed.len() > 3 && !trimmed.ends_with("```") {
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j < lines.len() && lines[j].trim() == "```" {
                i = j + 1;
                continue;
            }
        }

        if trimmed == "```" {
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j < lines.len() && lines[j].trim() == "```" {
                i = j + 1;
                continue;
            }
        }

        result_lines.push(line.trim_end());
        i += 1;
    }

    let mut final_lines: Vec<&str> = Vec::new();
    let mut last_was_empty = true;

    for line in result_lines {
        if line.trim().is_empty() {
            if !last_was_empty {
                final_lines.push("");
                last_was_empty = true;
            }
        } else {
            final_lines.push(line);
            last_was_empty = false;
        }
    }

    while let Some(last) = final_lines.last() {
        if last.is_empty() {
            final_lines.pop();
        } else {
            break;
        }
    }

    final_lines.join("\n")
}

fn extract_summary_from_xml(text: &str) -> Option<String> {
    // 1. Try finding attribute summary="..." in <tauqe_edits ...>
    if let Some(idx) = text.find("<tauqe_edits") {
        if let Some(end) = text[idx..].find('>') {
            let header = &text[idx..idx + end];
            if let Some(s) = extract_attribute(header, "summary") {
                if !s.trim().is_empty() {
                    return Some(s.trim().to_string());
                }
            }
        }
    }

    // 2. Try finding <summary>...</summary> tag
    if let Some((content, _)) = find_tag(text, "summary") {
        let clean = content.trim();
        if !clean.is_empty() {
            return Some(clean.to_string());
        }
    }

    None
}

fn parse_xml_edits(text: &str, editable_paths: &[String]) -> Result<Vec<EditOperation>, String> {
    let mut edits = Vec::new();
    let mut pos = 0;

    while pos < text.len() {
        let remaining = &text[pos..];

        let next_tag = find_next_edit_tag(remaining);
        let (tag_type, offset) = match next_tag {
            Some(res) => res,
            None => break,
        };

        let tag_start = pos + offset;
        let tag_slice = &text[tag_start..];

        match tag_type {
            TagType::Edit | TagType::Replace => {
                let open_tag_end = tag_slice
                    .find('>')
                    .ok_or_else(|| format!("Unterminated opening tag at offset {}", tag_start))?;
                let header = &tag_slice[..open_tag_end];
                let path_attr = extract_attribute(header, "path")
                    .ok_or_else(|| format!("Missing 'path' attribute in tag: {}", header))?;
                let target_path = resolve_target_path_for_op(&path_attr, editable_paths, false)?;

                let closing_tag = if tag_type == TagType::Edit {
                    "</edit>"
                } else {
                    "</replace>"
                };

                let close_idx = tag_slice.find(closing_tag).ok_or_else(|| {
                    format!(
                        "Missing closing tag '{}' for file '{}'",
                        closing_tag, target_path
                    )
                })?;

                let inner = &tag_slice[open_tag_end + 1..close_idx];

                let mut inner_pos = 0;
                let mut found_any = false;

                while let Some((search_content, after_search)) =
                    find_tag(&inner[inner_pos..], "search")
                {
                    let search_abs_end = inner_pos + after_search;
                    let remaining_inner = &inner[search_abs_end..];

                    let (replace_content, after_replace) =
                        if let Some(res) = find_tag(remaining_inner, "replace") {
                            res
                        } else if let Some(res) = find_tag(remaining_inner, "with") {
                            res
                        } else {
                            return Err(format!(
                                "Missing <replace> or <with> tag after <search> in file '{}'",
                                target_path
                            ));
                        };

                    edits.push(EditOperation::Replace {
                        path: target_path.clone(),
                        old_text: normalize_content(search_content),
                        new_text: normalize_content(replace_content),
                    });

                    found_any = true;
                    inner_pos = search_abs_end + after_replace;
                }

                if !found_any {
                    return Err(format!(
                        "Tag for file '{}' must contain <search> and <replace> blocks",
                        target_path
                    ));
                }

                pos = tag_start + close_idx + closing_tag.len();
            }
            TagType::Create => {
                let open_tag_end = tag_slice
                    .find('>')
                    .ok_or_else(|| format!("Unterminated <create> tag at offset {}", tag_start))?;
                let header = &tag_slice[..open_tag_end];
                let path_attr = extract_attribute(header, "path").ok_or_else(|| {
                    format!("Missing 'path' attribute in <create> tag: {}", header)
                })?;
                let target_path = resolve_target_path_for_op(&path_attr, editable_paths, true)?;

                let close_idx = tag_slice
                    .find("</create>")
                    .ok_or_else(|| format!("Missing </create> tag for file '{}'", target_path))?;

                let content = &tag_slice[open_tag_end + 1..close_idx];
                edits.push(EditOperation::Create {
                    path: target_path,
                    content: normalize_content(content),
                });

                pos = tag_start + close_idx + "</create>".len();
            }
            TagType::Move => {
                let open_tag_end = tag_slice
                    .find('>')
                    .ok_or_else(|| format!("Unterminated <move> tag at offset {}", tag_start))?;
                let header = &tag_slice[..open_tag_end];
                let from_attr = extract_attribute(header, "from")
                    .or_else(|| extract_attribute(header, "path"))
                    .ok_or_else(|| format!("Missing 'from' or 'path' attribute in <move> tag: {}", header))?;
                let to_attr = extract_attribute(header, "to")
                    .ok_or_else(|| format!("Missing 'to' attribute in <move> tag: {}", header))?;

                let target_from = resolve_target_path_for_op(&from_attr, editable_paths, false)?;
                let target_to = resolve_target_path_for_op(&to_attr, editable_paths, true)?;

                if header.ends_with('/') {
                    pos = tag_start + open_tag_end + 1;
                } else if let Some(close_idx) = tag_slice.find("</move>") {
                    pos = tag_start + close_idx + "</move>".len();
                } else {
                    pos = tag_start + open_tag_end + 1;
                }

                edits.push(EditOperation::Move {
                    from: target_from,
                    to: target_to,
                });
            }
            TagType::Delete => {
                let open_tag_end = tag_slice
                    .find('>')
                    .ok_or_else(|| format!("Unterminated <delete> tag at offset {}", tag_start))?;
                let header = &tag_slice[..open_tag_end];
                let path_attr = extract_attribute(header, "path").ok_or_else(|| {
                    format!("Missing 'path' attribute in <delete> tag: {}", header)
                })?;
                let target_path = resolve_target_path_for_op(&path_attr, editable_paths, false)?;

                if header.ends_with('/') {
                    pos = tag_start + open_tag_end + 1;
                } else if let Some(close_idx) = tag_slice.find("</delete>") {
                    pos = tag_start + close_idx + "</delete>".len();
                } else {
                    pos = tag_start + open_tag_end + 1;
                }

                edits.push(EditOperation::Delete { path: target_path });
            }
        }
    }

    Ok(edits)
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum TagType {
    Edit,
    Replace,
    Create,
    Delete,
    Move,
}

fn find_next_edit_tag(text: &str) -> Option<(TagType, usize)> {
    let mut candidates = Vec::new();

    if let Some(idx) = find_tag_start(text, "edit") {
        candidates.push((TagType::Edit, idx));
    }
    if let Some(idx) = find_tag_start(text, "replace") {
        if let Some(end_bracket) = text[idx..].find('>') {
            let header = &text[idx..idx + end_bracket];
            if header.contains("path") {
                candidates.push((TagType::Replace, idx));
            }
        }
    }
    if let Some(idx) = find_tag_start(text, "create") {
        candidates.push((TagType::Create, idx));
    }
    if let Some(idx) = find_tag_start(text, "delete") {
        candidates.push((TagType::Delete, idx));
    }
    if let Some(idx) = find_tag_start(text, "move") {
        candidates.push((TagType::Move, idx));
    }

    candidates.into_iter().min_by_key(|(_, idx)| *idx)
}

/// Locates an opening tag by base name with support for optional turn marker suffixes (e.g. `<edit_18DC...>`).
/// Returns the start position and the full matched tag name.
fn find_base_tag_start(text: &str, tag_base: &str) -> Option<(usize, String)> {
    let prefix = format!("<{}", tag_base);
    let mut search_from = 0;
    while let Some(idx) = text[search_from..].find(&prefix) {
        let abs_idx = search_from + idx;
        let after = &text[abs_idx + prefix.len()..];
        let bytes = after.as_bytes();
        let mut i = 0;
        if i < bytes.len() && (bytes[i] == b'_' || bytes[i] == b'-') {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
            {
                i += 1;
            }
        }
        let rest = &after[i..];
        if let Some(c) = rest.chars().next() {
            if c.is_whitespace() || c == '>' || c == '/' {
                let full_tag = text[abs_idx + 1..abs_idx + prefix.len() + i].to_string();
                return Some((abs_idx, full_tag));
            }
        }
        search_from = abs_idx + prefix.len();
    }
    None
}

fn find_tag_start(text: &str, tag_name: &str) -> Option<usize> {
    find_base_tag_start(text, tag_name).map(|(idx, _)| idx)
}

fn find_tag<'a>(text: &'a str, tag: &str) -> Option<(&'a str, usize)> {
    let open_prefix = format!("<{}", tag);
    let mut search_from = 0;
    while let Some(idx) = text[search_from..].find(&open_prefix) {
        let abs_start = search_from + idx;
        let after = &text[abs_start + open_prefix.len()..];
        if let Some(c) = after.chars().next() {
            if c == '>' || c.is_whitespace() {
                if let Some(close_bracket) = after.find('>') {
                    let content_start = abs_start + open_prefix.len() + close_bracket + 1;
                    let close_tag = format!("</{}>", tag);
                    if let Some(end_idx) = text[content_start..].find(&close_tag) {
                        let content = &text[content_start..content_start + end_idx];
                        let next_pos = content_start + end_idx + close_tag.len();
                        return Some((content, next_pos));
                    }
                }
            }
        }
        search_from = abs_start + open_prefix.len();
    }
    None
}

fn extract_attribute(tag_header: &str, attr: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(pos) = tag_header[search_from..].find(attr) {
        let abs_pos = search_from + pos;
        let before_ok = abs_pos == 0
            || tag_header[..abs_pos]
                .chars()
                .last()
                .map(|c| c.is_whitespace() || c == '<')
                .unwrap_or(false);
        let after_attr = &tag_header[abs_pos + attr.len()..];
        let trimmed = after_attr.trim_start();
        if before_ok && trimmed.starts_with('=') {
            let after_eq = trimmed[1..].trim_start();
            if after_eq.is_empty() {
                return None;
            }
            let quote_char = after_eq.chars().next()?;
            if quote_char == '"' || quote_char == '\'' {
                let rest = &after_eq[quote_char.len_utf8()..];
                let end_quote = rest.find(quote_char)?;
                let val = &rest[..end_quote];
                return Some(val.trim().trim_matches('`').to_string());
            } else {
                let val: String = after_eq
                    .chars()
                    .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
                    .collect();
                if !val.is_empty() {
                    return Some(val.trim_matches('`').to_string());
                }
            }
        }
        search_from = abs_pos + attr.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xml_protocol_edit_with_summary_attribute() {
        let proto = XmlEditProtocol;
        let editable = vec!["crates/core/src/lib.rs".to_string()];

        let output = r#"I have updated the code:

<tauqe_edits summary="Refactor core initialization">
  <edit path="crates/core/src/lib.rs">
    <search>
pub fn old_fn() {}
    </search>
    <replace>
pub fn new_fn() {}
    </replace>
  </edit>
</tauqe_edits>
"#;

        let res = proto.parse_output(output, &editable);
        match res {
            ModelResult::Edit { summary, edits, .. } => {
                assert_eq!(summary, "Refactor core initialization");
                assert_eq!(edits.len(), 1);
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_extract_conversational_text_removes_xml_blocks() {
        let output = r#"I have updated the code to fix the issue.

<tauqe_edits summary="Fix issue">
  <edit path="crates/core/src/lib.rs">
    <search>
old_code();
    </search>
    <replace>
new_code();
    </replace>
  </edit>
</tauqe_edits>

Everything is tested and working properly."#;

        let extracted = extract_conversational_text(output);
        assert_eq!(
            extracted,
            "I have updated the code to fix the issue.\n\nEverything is tested and working properly."
        );
    }

    #[test]
    fn test_extract_conversational_text_individual_tags_and_fences() {
        let output = r#"Here is the change:

```xml
<create path="crates/core/src/helper.rs">
pub fn help() {}
</create>
```

And deleting obsolete:
<delete path="crates/core/src/old.rs" />

All done!"#;

        let extracted = extract_conversational_text(output);
        assert_eq!(
            extracted,
            "Here is the change:\n\nAnd deleting obsolete:\n\nAll done!"
        );
    }

    #[test]
    fn test_extract_conversational_text_only_edits() {
        let output = r#"<tauqe_edits summary="Pure edit">
  <delete path="test.rs" />
</tauqe_edits>"#;
        let extracted = extract_conversational_text(output);
        assert_eq!(extracted, "");
    }

    #[test]
    fn test_extract_conversational_text_removes_suffixed_xml_blocks_and_context_requests() {
        let output = r#"I have updated the code to fix the issue.

<context_request_TESTMARKER123 path="crates/tui/src/main.rs" access="editable" />

<tauqe_edits_TESTMARKER123 summary="Fix issue">
  <edit_TESTMARKER123 path="crates/core/src/lib.rs">
    <search_TESTMARKER123>
old_code();
    </search_TESTMARKER123>
    <replace_TESTMARKER123>
new_code();
    </replace_TESTMARKER123>
  </edit_TESTMARKER123>
</tauqe_edits_TESTMARKER123>

Everything is tested and working properly."#;

        let extracted = extract_conversational_text(output);
        assert_eq!(
            extracted,
            "I have updated the code to fix the issue.\n\nEverything is tested and working properly."
        );
    }

    #[test]
    fn test_strip_context_request_tags_suffixed() {
        let raw = "Intro\n<context_request_A1B2C3 path=\"src/a.rs\" access=\"read_only\" />\nMiddle\n<context_request_A1B2C3 path=\"src/b.rs\"></context_request_A1B2C3>\nOutro";
        let stripped = strip_context_request_tags(raw);
        assert_eq!(stripped.trim(), "Intro\n\nMiddle\n\nOutro");
    }

    #[test]
    fn test_extract_and_strip_verify_request() {
        let text = "Here are changes.\n<verify target=\"clippy\" on_success=\"silent\" />\nAll done.";
        let req = extract_verify_request(text).unwrap();
        assert_eq!(req.target, VerifyTarget::Clippy);
        assert_eq!(req.on_success, VerifyOnSuccess::Silent);
        assert_eq!(
            strip_verify_tags(text).trim(),
            "Here are changes.\n\nAll done."
        );
    }

    #[test]
    fn test_extract_and_strip_user_language() {
        let text = "Hello!\n<user_language>Russian</user_language>\nHere is my explanation.";
        assert_eq!(extract_user_language(text), Some("Russian".to_string()));
        assert_eq!(
            strip_user_language_tags(text).trim(),
            "Hello!\n\nHere is my explanation."
        );

        let suffixed = "Hi\n<user_language_ABC123>en</user_language_ABC123>\nDone.";
        assert_eq!(extract_user_language(suffixed), Some("en".to_string()));
        assert_eq!(strip_user_language_tags(suffixed).trim(), "Hi\n\nDone.");
    }

    #[test]
    fn test_marked_xml_with_literal_tags_in_content() {
        let marker = "M12345";
        let proto = MarkedXmlEditProtocol::new(marker);
        let editable = vec!["src/test.rs".to_string()];

        let output = format!(
            "Explanation\n\
            <tauqe_edits_{m} summary=\"Fix tags\">\n  \
              <edit_{m} path=\"src/test.rs\">\n    \
                <search_{m}>\n\
let s = \"</search>\";\n\
<replace>\n\
                </search_{m}>\n    \
                <replace_{m}>\n\
let s = \"clean\";\n\
                </replace_{m}>\n  \
              </edit_{m}>\n\
            </tauqe_edits_{m}>",
            m = marker
        );

        let res = proto.parse_output(&output, &editable);
        match res {
            ModelResult::Edit { edits, .. } => {
                assert_eq!(edits.len(), 1);
                match &edits[0] {
                    EditOperation::Replace { old_text, new_text, .. } => {
                        assert!(old_text.contains("let s = \"</search>\";\n<replace>"));
                        assert!(new_text.contains("let s = \"clean\";"));
                    }
                    _ => panic!("Expected Replace"),
                }
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }
}
