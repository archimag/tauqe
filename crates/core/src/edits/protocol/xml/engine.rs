//! Parametric XML tag engine for the marker-bound edit protocol.
//!
//! Every structural tag the model may emit is declared exactly once as an
//! [`XmlTagSpec`] in [`XML_TAG_SPECS`]. Boundary scanning, attribute reading,
//! text stripping, and stream suppression are then implemented once and
//! parametrized by that declaration, so a tag cannot drift between its
//! scanner, its stripper, and its stream suppressor.

use tauqe_protocol::LlmCapability;

use super::extract_attribute;
use super::tags::{is_inside_markdown_code_with_fences, CodeFences};

/// How a tag's closing boundary is delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagCloseMode {
    /// Self-closing, or paired with a closing tag directly after the header.
    Simple,
    /// Self-closing, or paired with a closing tag found after the header.
    Control,
    /// Block container whose body nests further protocol tags.
    Container,
}

/// How strictly a tag must carry the active turn marker to take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerPolicy {
    /// Only a marker-bound tag is honored.
    Strict,
    /// An unmarked tag is honored when the model dropped the marker suffix.
    TolerantFallback,
}

/// Declarative description of one XML tag family.
///
/// A single declaration owns the tag name, the capability it realizes, its
/// close mode, its marker tolerance, and the children it may contain. Consumers
/// never re-declare those facts.
#[derive(Debug, Clone, Copy)]
pub struct XmlTagSpec {
    /// Base tag name without a turn-marker suffix.
    pub base_name: &'static str,
    /// Capability the tag realizes, when it is a capability control tag.
    pub capability: Option<LlmCapability>,
    /// How the tag's closing boundary is delimited.
    pub close_mode: TagCloseMode,
    /// Whether an unmarked tag may still take effect for this base name.
    pub marker_policy: MarkerPolicy,
    /// Child tags accepted without a marker suffix inside this container.
    pub child_tags: &'static [&'static str],
    /// Whether the tag is declared as a protocol-only control tag.
    pub is_control_tag: bool,
    /// Whether a still-open tag must be withheld from the user-facing stream.
    pub stream_suppress_unmarked: bool,
}

impl XmlTagSpec {
    /// Reports whether the tag carries protocol control intent.
    ///
    /// A tag is a control tag when it realizes a model capability, because
    /// capability tags never appear in conversational prose, or when it is an
    /// explicit protocol-only marker such as the plan container.
    pub(crate) fn is_control(&self) -> bool {
        self.is_control_tag || self.capability.is_some()
    }
}

/// Builds a code-modification or plan-child tag spec.
const fn spec(
    base_name: &'static str,
    close_mode: TagCloseMode,
    stream_suppress_unmarked: bool,
) -> XmlTagSpec {
    XmlTagSpec {
        base_name,
        capability: None,
        close_mode,
        marker_policy: MarkerPolicy::Strict,
        child_tags: &[],
        is_control_tag: false,
        stream_suppress_unmarked,
    }
}

/// Builds a capability control tag spec.
const fn control_spec(
    base_name: &'static str,
    capability: LlmCapability,
    close_mode: TagCloseMode,
    marker_policy: MarkerPolicy,
) -> XmlTagSpec {
    XmlTagSpec {
        base_name,
        capability: Some(capability),
        close_mode,
        marker_policy,
        child_tags: &[],
        is_control_tag: true,
        stream_suppress_unmarked: true,
    }
}

/// Builds a nested container tag spec.
const fn container_spec(
    base_name: &'static str,
    child_tags: &'static [&'static str],
    is_control_tag: bool,
    stream_suppress_unmarked: bool,
) -> XmlTagSpec {
    XmlTagSpec {
        base_name,
        capability: None,
        close_mode: TagCloseMode::Container,
        marker_policy: MarkerPolicy::Strict,
        child_tags,
        is_control_tag,
        stream_suppress_unmarked,
    }
}

/// Authoritative registry of every structural XML tag.
///
/// Order matters only for prefix overlap: a longer base name that starts with a
/// shorter one must be declared after it so the shorter name is tried first.
pub(crate) const XML_TAG_SPECS: &[XmlTagSpec] = &[
    // Code-modification tags.
    container_spec(
        "tauqe_edits",
        &["edit", "create", "overwrite", "delete", "move"],
        false,
        true,
    ),
    container_spec("edit", &["search", "replace", "with"], false, true),
    spec("search", TagCloseMode::Control, false),
    spec("replace", TagCloseMode::Control, false),
    spec("with", TagCloseMode::Control, false),
    spec("create", TagCloseMode::Control, true),
    spec("overwrite", TagCloseMode::Control, true),
    spec("delete", TagCloseMode::Simple, true),
    spec("move", TagCloseMode::Simple, true),
    spec("summary", TagCloseMode::Control, false),
    // Capability control tags.
    control_spec(
        "context_request",
        LlmCapability::Discovery,
        TagCloseMode::Simple,
        MarkerPolicy::Strict,
    ),
    control_spec(
        "repo_search",
        LlmCapability::Discovery,
        TagCloseMode::Simple,
        MarkerPolicy::Strict,
    ),
    control_spec(
        "repo_dirs",
        LlmCapability::Discovery,
        TagCloseMode::Simple,
        MarkerPolicy::Strict,
    ),
    control_spec(
        "repo_map",
        LlmCapability::Discovery,
        TagCloseMode::Simple,
        MarkerPolicy::Strict,
    ),
    control_spec(
        "dir_list",
        LlmCapability::Discovery,
        TagCloseMode::Simple,
        MarkerPolicy::Strict,
    ),
    control_spec(
        "doc_request",
        LlmCapability::DocRequest,
        TagCloseMode::Simple,
        MarkerPolicy::Strict,
    ),
    control_spec(
        "verify",
        LlmCapability::ToolchainVerify,
        TagCloseMode::Simple,
        MarkerPolicy::TolerantFallback,
    ),
    control_spec(
        "user_language",
        LlmCapability::UserLanguage,
        TagCloseMode::Control,
        MarkerPolicy::TolerantFallback,
    ),
    control_spec(
        "scratchpad",
        LlmCapability::Scratchpad,
        TagCloseMode::Control,
        MarkerPolicy::TolerantFallback,
    ),
    control_spec(
        "success",
        LlmCapability::Outcome,
        TagCloseMode::Control,
        MarkerPolicy::Strict,
    ),
    control_spec(
        "error",
        LlmCapability::Outcome,
        TagCloseMode::Control,
        MarkerPolicy::Strict,
    ),
    control_spec(
        "blocked",
        LlmCapability::Blocked,
        TagCloseMode::Control,
        MarkerPolicy::Strict,
    ),
    // Project custom tools travel as a self-closing tag naming one declared
    // command. The invocation stays a closed allowlist: the tag names a command
    // and never carries a command line of its own.
    control_spec(
        "custom_tool",
        LlmCapability::CustomTools,
        TagCloseMode::Simple,
        MarkerPolicy::Strict,
    ),
    // Parse-only outcome tag. The review contour reads it back from the
    // cleaned assistant text to record why a finding was skipped, so
    // `strip_all_control_tags` must never remove it.
    spec("skip", TagCloseMode::Control, false),
    XmlTagSpec {
        base_name: "plan",
        capability: Some(LlmCapability::PlanUpdate),
        close_mode: TagCloseMode::Container,
        marker_policy: MarkerPolicy::Strict,
        child_tags: &[
            "concept",
            "concept_item",
            "item",
            "description",
            "details",
            "title",
            "summary",
        ],
        is_control_tag: true,
        stream_suppress_unmarked: true,
    },
    container_spec(
        "concept",
        &["concept_item", "item", "title", "details", "description"],
        false,
        false,
    ),
    container_spec(
        "concept_item",
        &["concept_item", "title", "details", "description"],
        false,
        false,
    ),
    container_spec(
        "item",
        &["item", "title", "details", "description"],
        false,
        false,
    ),
    spec("description", TagCloseMode::Control, false),
    spec("details", TagCloseMode::Control, false),
    spec("title", TagCloseMode::Control, false),
    // Retired discovery tag kept only so marker normalization stays tolerant.
    spec("context_drop", TagCloseMode::Control, false),
];

/// Resolves the registry entry of a base tag name.
pub(crate) fn tag_spec(base: &str) -> Option<&'static XmlTagSpec> {
    XML_TAG_SPECS.iter().find(|spec| spec.base_name == base)
}

/// Iterates every declared base tag name in registry order.
pub(crate) fn tag_bases() -> impl Iterator<Item = &'static str> {
    XML_TAG_SPECS.iter().map(|spec| spec.base_name)
}

/// One open structural frame tracked while normalizing marker-suffixed input.
///
/// A frame remembers whether its opening tag carried the turn marker. That bit
/// lets normalization keep a literal unmarked closing tag, such as a `</search>`
/// quoted inside a marked search body, from closing a marker-bound block.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OpenTag {
    /// Base name of the open tag.
    pub base: &'static str,
    /// Whether the opening tag carried the active turn marker.
    pub marked: bool,
}

/// Reports whether an unmarked child tag is accepted inside the innermost frame.
///
/// The innermost open container declares which of its children may omit the
/// turn-marker suffix, which keeps plan bodies readable without weakening the
/// marker guarantee for every other tag. Only the innermost frame is consulted:
/// consulting outer containers would reactivate a tag that is merely quoted
/// inside the body of a nested leaf block.
pub(crate) fn accepts_unmarked_child(stack: &[OpenTag], name: &str) -> bool {
    stack
        .last()
        .and_then(|open| tag_spec(open.base))
        .is_some_and(|container| container.child_tags.contains(&name))
}

/// Reports whether an unmarked tag of this base name may still take effect.
pub(crate) fn allows_unmarked_fallback(base: &str) -> bool {
    tag_spec(base).is_some_and(|spec| spec.marker_policy == MarkerPolicy::TolerantFallback)
}

/// One matched tag occurrence inside a text buffer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct XmlTagMatch<'a> {
    /// Byte offset of the opening `<`.
    pub start: usize,
    /// Byte offset one past the closing boundary of the match.
    pub end: usize,
    /// Tag header without the trailing `>`.
    pub header: &'a str,
    /// Trimmed body text of a paired tag, when it is not empty.
    pub body: Option<&'a str>,
}

impl<'a> XmlTagMatch<'a> {
    /// Reads one attribute value from the tag header.
    pub(crate) fn attr(&self, name: &str) -> Option<String> {
        extract_attribute(self.header, name)
    }

    /// Reads the first present attribute among `names`, in order.
    pub(crate) fn attr_any(&self, names: &[&str]) -> Option<String> {
        names.iter().find_map(|name| self.attr(name))
    }

    /// Reads the first present attribute among `names`, falling back to the body.
    pub(crate) fn attr_or_body(&self, attr_names: &[&str]) -> Option<String> {
        self.attr_any(attr_names)
            .or_else(|| self.body.map(str::to_string))
    }
}

/// Locates the next complete tag of `spec`, honoring its close mode.
///
/// An optional turn-marker suffix is accepted, occurrences inside markdown code
/// are ignored, and an incomplete tag yields `None` so streaming callers can
/// wait for more bytes.
pub(crate) fn find_next_tag_bounds_with_fences(
    text: &str,
    spec: &XmlTagSpec,
    from_pos: usize,
    fences: &CodeFences,
) -> Option<(usize, usize)> {
    let mut from = from_pos;
    while let Some(offset) = text[from..].find('<') {
        let start = from + offset;
        let after_lt = &text[start + 1..];
        if let Some(after_base) = after_lt.strip_prefix(spec.base_name) {
            let suffix_len = marker_suffix_len(after_base);
            let rest = &after_base[suffix_len..];
            let boundary = rest
                .chars()
                .next()
                .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
            if boundary && !is_inside_markdown_code_with_fences(text, start, fences) {
                let name_end = start + 1 + spec.base_name.len() + suffix_len;
                if let Some(bounds) = tag_close_bounds(text, start, name_end, spec) {
                    return Some(bounds);
                }
            }
        }
        from = start + 1;
    }
    None
}

#[cfg(test)]
pub(crate) fn find_next_tag_bounds(text: &str, spec: &XmlTagSpec) -> Option<(usize, usize)> {
    find_next_tag_bounds_with_fences(text, spec, 0, &CodeFences::find(text))
}

/// Locates the next complete tag of `spec` and decomposes it using precomputed code fences.
pub(crate) fn find_next_tag_with_fences<'a>(
    text: &'a str,
    spec: &XmlTagSpec,
    from_pos: usize,
    fences: &CodeFences,
) -> Option<XmlTagMatch<'a>> {
    let (start, end) = find_next_tag_bounds_with_fences(text, spec, from_pos, fences)?;
    let tag = text.get(start..end)?;
    let header_end = tag.find('>')?;
    let header = &tag[..header_end];
    let tag_name = header
        .strip_prefix('<')
        .and_then(|rest| {
            rest.split(|c: char| c.is_whitespace() || c == '>' || c == '/')
                .next()
        })
        .unwrap_or(spec.base_name);
    let body = if header.trim_end().ends_with('/') {
        None
    } else {
        let close_tag = format!("</{}>", tag_name);
        tag.get(header_end + 1..)
            .and_then(|rest| rest.strip_suffix(close_tag.as_str()))
            .map(str::trim)
            .filter(|body| !body.is_empty())
    };
    Some(XmlTagMatch {
        start,
        end,
        header,
        body,
    })
}

/// Locates the next complete tag of `spec` and decomposes it.
pub(crate) fn find_next_tag<'a>(text: &'a str, spec: &XmlTagSpec) -> Option<XmlTagMatch<'a>> {
    find_next_tag_with_fences(text, spec, 0, &CodeFences::find(text))
}

/// Iterates every match of `spec` in `text`, in order.
pub(crate) fn iter_tags<'a>(
    text: &'a str,
    spec: &'static XmlTagSpec,
) -> std::vec::IntoIter<XmlTagMatch<'a>> {
    let fences = CodeFences::find(text);
    let mut matches = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        let Some(found) = find_next_tag_with_fences(text, spec, pos, &fences) else {
            break;
        };
        pos = found.end.max(pos + 1);
        matches.push(found);
    }
    matches.into_iter()
}

/// Removes every occurrence of the tags declared by `spec`.
pub(crate) fn strip_tag_by_spec(text: &str, spec: &'static XmlTagSpec) -> String {
    let mut out = String::new();
    let mut cursor = 0;
    for found in iter_tags(text, spec) {
        out.push_str(&text[cursor..found.start]);
        cursor = found.end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Removes every control tag from text, leaving edit blocks untouched.
///
/// Code-modification tags belong to the edit contour and are stripped by that
/// contour, so only capability and protocol-only control tags are removed here.
pub(crate) fn strip_all_control_tags(text: &str) -> String {
    let mut text = text.to_string();
    for spec in XML_TAG_SPECS.iter().filter(|spec| spec.is_control()) {
        text = strip_tag_by_spec(&text, spec);
    }
    text
}

/// Locates the earliest complete control tag in `text`.
pub(crate) fn find_earliest_control_tag(text: &str) -> Option<(usize, usize)> {
    let fences = CodeFences::find(text);
    let mut from = 0;
    while let Some(offset) = text[from..].find('<') {
        let start = from + offset;
        if !is_inside_markdown_code_with_fences(text, start, &fences) {
            let after_lt = &text[start + 1..];
            for spec in XML_TAG_SPECS.iter().filter(|spec| spec.is_control()) {
                if let Some(after_base) = after_lt.strip_prefix(spec.base_name) {
                    let suffix_len = marker_suffix_len(after_base);
                    let rest = &after_base[suffix_len..];
                    let boundary = rest
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
                    if boundary {
                        let name_end = start + 1 + spec.base_name.len() + suffix_len;
                        if let Some(bounds) = tag_close_bounds(text, start, name_end, spec) {
                            return Some(bounds);
                        }
                    }
                }
            }
        }
        from = start + 1;
    }
    None
}

/// Byte length of `buf` that is safe to emit as conversational text.
///
/// A trailing partial tag prefix and any still-open suppressed tag are
/// withheld so the interface never shows protocol scaffolding mid-tag.
pub(crate) fn compute_safe_stream_emit_len(buf: &str) -> usize {
    let fences = CodeFences::find(buf);
    let mut limit = buf.len();

    if let Some(last_lt) = buf.rfind('<') {
        if !is_inside_markdown_code_with_fences(buf, last_lt, &fences) {
            let trailing = &buf[last_lt..];
            let partial = XML_TAG_SPECS
                .iter()
                .filter(|spec| spec.stream_suppress_unmarked)
                .any(|spec| is_partial_tag_open(trailing, spec.base_name));
            if partial {
                limit = limit.min(last_lt);
            }
        }
    }

    for spec in XML_TAG_SPECS
        .iter()
        .filter(|spec| spec.stream_suppress_unmarked)
    {
        let Some(idx) = rfind_tag_open(buf, spec.base_name, &fences) else {
            continue;
        };
        let rest = &buf[idx + 1 + spec.base_name.len()..];
        let boundary = rest
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/' || c == '_' || c == '-');
        if !boundary {
            continue;
        }
        let closed = if spec.is_control() {
            matches!(
                find_next_tag_bounds_with_fences(buf, spec, idx, &fences),
                Some((s, _)) if s == idx
            )
        } else {
            rest.contains('>')
        };
        if !closed {
            limit = limit.min(idx);
        }
    }

    limit
}

/// Consumes an optional turn-marker suffix directly after the base name.
fn marker_suffix_len(after_base: &str) -> usize {
    let bytes = after_base.as_bytes();
    let mut idx = 0;
    if idx < bytes.len() && (bytes[idx] == b'_' || bytes[idx] == b'-') {
        idx += 1;
        while idx < bytes.len()
            && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-')
        {
            idx += 1;
        }
    }
    idx
}

/// Computes the closing boundary of a tag whose header starts at `start`.
///
/// Returning `None` means this candidate does not form a complete tag, so the
/// caller keeps scanning instead of aborting the search.
fn tag_close_bounds(
    text: &str,
    start: usize,
    name_end: usize,
    spec: &XmlTagSpec,
) -> Option<(usize, usize)> {
    let rest = &text[name_end..];
    let gt = rest.find('>')?;
    let header_end = name_end + gt + 1;
    if rest[..gt].trim_end().ends_with('/') {
        return Some((start, header_end));
    }
    let close_tag = format!("</{}>", &text[start + 1..name_end]);
    match spec.close_mode {
        TagCloseMode::Simple => {
            let after_open = &text[header_end..];
            let trimmed = after_open.trim_start();
            if !trimmed.starts_with(&close_tag) {
                return None;
            }
            let end = header_end + (after_open.len() - trimmed.len()) + close_tag.len();
            Some((start, end))
        }
        TagCloseMode::Control | TagCloseMode::Container => {
            let offset = text[header_end..].find(&close_tag)?;
            Some((start, header_end + offset + close_tag.len()))
        }
    }
}

/// Reports whether `trailing` is an incomplete opening tag of `base`.
fn is_partial_tag_open(trailing: &str, base: &str) -> bool {
    match trailing.strip_prefix('<') {
        Some(rest) => base.starts_with(rest),
        None => false,
    }
}

/// Finds the byte offset of the last `"<base"` occurrence in `buf` outside markdown code.
fn rfind_tag_open(buf: &str, base: &str, fences: &CodeFences) -> Option<usize> {
    let mut last = None;
    let mut from = 0;
    while let Some(offset) = buf[from..].find('<') {
        let start = from + offset;
        if !is_inside_markdown_code_with_fences(buf, start, fences)
            && buf[start + 1..].starts_with(base)
        {
            last = Some(start);
        }
        from = start + 1;
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_of(base: &str) -> &'static XmlTagSpec {
        match tag_spec(base) {
            Some(found) => found,
            None => panic!("missing spec for {base}"),
        }
    }

    #[test]
    fn registry_declares_unique_base_names() {
        for (index, spec) in XML_TAG_SPECS.iter().enumerate() {
            assert!(!spec.base_name.is_empty());
            let duplicate = XML_TAG_SPECS
                .iter()
                .skip(index + 1)
                .any(|other| other.base_name == spec.base_name);
            assert!(!duplicate, "duplicate base name {}", spec.base_name);
        }
    }

    #[test]
    fn registry_assigns_capabilities_policies_and_close_modes() {
        assert_eq!(
            spec_of("context_request").capability,
            Some(LlmCapability::Discovery)
        );
        assert_eq!(
            spec_of("verify").capability,
            Some(LlmCapability::ToolchainVerify)
        );
        assert_eq!(spec_of("context_request").close_mode, TagCloseMode::Simple);
        assert_eq!(spec_of("plan").close_mode, TagCloseMode::Container);
        assert!(spec_of("plan").child_tags.contains(&"item"));
        assert!(spec_of("plan").is_control());
        assert!(spec_of("context_request").is_control());
        assert!(!spec_of("edit").is_control());
        assert!(allows_unmarked_fallback("verify"));
        assert!(allows_unmarked_fallback("user_language"));
        assert!(allows_unmarked_fallback("scratchpad"));
        assert_eq!(
            spec_of("repo_dirs").capability,
            Some(LlmCapability::Discovery)
        );
        assert_eq!(
            spec_of("repo_map").capability,
            Some(LlmCapability::Discovery)
        );
        assert_eq!(spec_of("repo_map").close_mode, TagCloseMode::Simple);
        assert!(spec_of("repo_map").is_control());
        assert!(!allows_unmarked_fallback("context_request"));
        assert!(!allows_unmarked_fallback("repo_dirs"));
        assert!(!allows_unmarked_fallback("repo_map"));
        assert!(!allows_unmarked_fallback("dir_list"));
        assert!(!allows_unmarked_fallback("success"));

        assert_eq!(
            spec_of("dir_list").capability,
            Some(LlmCapability::Discovery)
        );
        assert_eq!(spec_of("dir_list").close_mode, TagCloseMode::Simple);
        assert!(spec_of("dir_list").is_control());

        assert_eq!(
            spec_of("custom_tool").capability,
            Some(LlmCapability::CustomTools)
        );
        assert_eq!(spec_of("custom_tool").close_mode, TagCloseMode::Simple);
        assert!(spec_of("custom_tool").is_control());
    }

    #[test]
    fn registry_declares_skip_as_a_parse_only_outcome_tag() {
        // The review contour reads the skip tag back from the cleaned assistant
        // text, so the tag stays parseable but is never stripped as a control
        // tag.
        let spec = spec_of("skip");
        assert_eq!(spec.capability, None);
        assert!(!spec.is_control());
        assert!(!spec.stream_suppress_unmarked);
        assert!(!allows_unmarked_fallback("skip"));

        let text = "<skip reason=\"False positive\">already handled</skip>";
        let found = match find_next_tag(text, spec) {
            Some(found) => found,
            None => panic!("skip tag not found"),
        };
        assert_eq!(found.attr("reason").as_deref(), Some("False positive"));
        assert_eq!(found.body, Some("already handled"));
    }

    #[test]
    fn find_next_tag_bounds_honors_every_close_mode() {
        let self_closing = "intro <verify target=\"all\" /> outro";
        let (start, end) = match find_next_tag_bounds(self_closing, spec_of("verify")) {
            Some(bounds) => bounds,
            None => panic!("self-closing tag not found"),
        };
        assert_eq!(&self_closing[start..end], "<verify target=\"all\" />");

        let paired = "<success>done</success>";
        let (start, end) = match find_next_tag_bounds(paired, spec_of("success")) {
            Some(bounds) => bounds,
            None => panic!("paired tag not found"),
        };
        assert_eq!(&paired[start..end], paired);

        let immediately_closed = "<doc_request topic=\"git\"></doc_request>";
        assert!(find_next_tag_bounds(immediately_closed, spec_of("doc_request")).is_some());

        let suffixed = "<context_request_AB12 path=\"a.rs\" />";
        assert!(find_next_tag_bounds(suffixed, spec_of("context_request")).is_some());

        let map = "<repo_map_AB12 path=\"crates/core\" />";
        let (start, end) = match find_next_tag_bounds(map, spec_of("repo_map")) {
            Some(bounds) => bounds,
            None => panic!("targeted map tag not found"),
        };
        assert_eq!(&map[start..end], map);

        assert!(find_next_tag_bounds("<verify", spec_of("verify")).is_none());
        assert!(find_next_tag_bounds("<verif", spec_of("verify")).is_none());
    }

    #[test]
    fn find_next_tag_bounds_ignores_markdown_code() {
        let text = "```xml\n<context_request path=\"fenced.rs\" />\n```\n<context_request path=\"real.rs\" />";
        let found = match find_next_tag(text, spec_of("context_request")) {
            Some(found) => found,
            None => panic!("context request not found"),
        };
        assert_eq!(found.attr("path").as_deref(), Some("real.rs"));
    }

    #[test]
    fn attribute_readers_prefer_attributes_over_body() {
        let text = "<scratchpad>unused body</scratchpad>";
        let found = match find_next_tag(text, spec_of("scratchpad")) {
            Some(found) => found,
            None => panic!("scratchpad not found"),
        };
        assert_eq!(found.attr("note"), None);
        assert_eq!(found.body, Some("unused body"));
        assert_eq!(
            found.attr_or_body(&["note"]).as_deref(),
            Some("unused body")
        );

        let attributed = "<scratchpad note=\"from attribute\">body</scratchpad>";
        let found = match find_next_tag(attributed, spec_of("scratchpad")) {
            Some(found) => found,
            None => panic!("attributed scratchpad not found"),
        };
        assert_eq!(
            found.attr_any(&["note", "message"]).as_deref(),
            Some("from attribute")
        );
        assert_eq!(
            found.attr_or_body(&["note"]).as_deref(),
            Some("from attribute")
        );
    }

    #[test]
    fn strip_all_control_tags_removes_only_control_tags() {
        let text = "Before\n<context_request path=\"a.rs\" />\n<verify target=\"all\" />\n<success />\nAfter";
        assert_eq!(strip_all_control_tags(text), "Before\n\n\n\nAfter");

        // Code-modification tags belong to the edit contour and stay untouched.
        let edits = "<create path=\"a.rs\">x</create>";
        assert_eq!(strip_all_control_tags(edits), edits);

        // The skip tag is parse-only: the review contour reads it back from the
        // cleaned assistant text, so it survives conversational cleaning.
        let skip = "<skip reason=\"False positive\">already handled</skip>";
        assert_eq!(strip_all_control_tags(skip), skip);
    }

    #[test]
    fn single_spec_stripping_and_iteration_agree_with_the_spec() {
        let text = "<plan id=\"p\"><item id=\"1\" />\n</plan>\n<verify target=\"all\"/>";
        assert_eq!(
            strip_tag_by_spec(text, spec_of("plan")),
            "\n<verify target=\"all\"/>"
        );
        assert_eq!(iter_tags(text, spec_of("context_request")).count(), 0);
        assert_eq!(iter_tags(text, spec_of("verify")).count(), 1);
    }

    #[test]
    fn earliest_control_tag_reports_the_first_declared_control_tag() {
        let text = "text <verify target=\"all\" /> then <context_request path=\"a.rs\" />";
        let (start, _) = match find_earliest_control_tag(text) {
            Some(found) => found,
            None => panic!("control tag not found"),
        };
        assert!(text[start..].starts_with("<verify"));
        assert_eq!(find_earliest_control_tag("no tags here"), None);
    }

    #[test]
    fn safe_stream_emit_len_withholds_incomplete_tags() {
        let complete = "hello <verify target=\"all\" />";
        assert_eq!(compute_safe_stream_emit_len(complete), complete.len());

        let partial = "hello <veri";
        assert_eq!(compute_safe_stream_emit_len(partial), "hello ".len());

        let open = "hello <success>done";
        assert_eq!(compute_safe_stream_emit_len(open), "hello ".len());
    }

    #[test]
    fn safe_stream_emit_len_ignores_markdown_fences_and_inline_code() {
        let fenced_with_fake = "Text\n```rust\nif a < b && c < d {\n    <verify\n}\n```\nOutside";
        assert_eq!(
            compute_safe_stream_emit_len(fenced_with_fake),
            fenced_with_fake.len()
        );

        let inline_code_with_partial = "Note: `<verify` is not active";
        assert_eq!(
            compute_safe_stream_emit_len(inline_code_with_partial),
            inline_code_with_partial.len()
        );
    }

    #[test]
    fn earliest_control_tag_ignores_code_blocks_with_comparisons() {
        let text = r#"
Explanation:
```rust
let x = a < b;
let y = c < d;
<error message="fake" />
```
Real outcome:
<verify target="all" />
"#;
        let (start, end) = find_earliest_control_tag(text).expect("should find real control tag");
        assert_eq!(&text[start..end], "<verify target=\"all\" />");
    }

    #[test]
    fn iter_tags_honors_code_fences_and_returns_absolute_offsets() {
        let text = "First <verify target=\"check\" /> inside\n```\n<verify target=\"test\" />\n```\nand second <verify target=\"all\" />";
        let matches: Vec<_> = iter_tags(text, spec_of("verify")).collect();
        assert_eq!(matches.len(), 2);
        assert_eq!(
            &text[matches[0].start..matches[0].end],
            "<verify target=\"check\" />"
        );
        assert_eq!(
            &text[matches[1].start..matches[1].end],
            "<verify target=\"all\" />"
        );
    }
}
