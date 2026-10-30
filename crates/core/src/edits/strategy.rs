//! Patch-strategy profile: the single owner of everything strategy-specific.
//!
//! A [`PatchStrategy`] decides how a change set addresses the lines it replaces:
//! `SearchReplace` echoes the replaced text back, while `LineAnchor` names the
//! range through the opaque anchors rendered beside every context line. The
//! choice changes what the model is told, which fields a change carries, and how
//! a replacement is described; it never changes which capabilities a turn
//! exposes.
//!
//! Every artifact that depends on that choice lives here: the fields of one
//! change entry, the descriptions the wire schema publishes for them, and the
//! prompt texts of both contours. Consumers read the profile instead of restating
//! strategy vocabulary, so a strategy can never drift from its own instruction.
//!
//! Extending the surface stays compiler-checked: a new [`PatchStrategy`] variant
//! does not compile until [`StrategyProfile::of`] maps it to a profile, and every
//! field of a profile is required, so a partially described strategy cannot be
//! introduced by accident.

use tauqe_protocol::PatchStrategy;

use super::EditError;

/// One property that addresses the replaced lines of a change entry.
///
/// Such fields, together with the description of the `content` property, are the
/// whole schema vocabulary a strategy contributes: everything else a change
/// entry declares — its operation, its target path, and the move destination —
/// reads identically for every strategy.
pub(crate) struct ReplacedLineField {
    /// Wire name of the property.
    pub(crate) name: &'static str,
    /// Description the JSON schema publishes for the property.
    pub(crate) description: &'static str,
}

/// Renders one strategy's XML code-modification instruction.
///
/// The renderer receives a resolver that maps a control tag base name to the
/// spelling of the active turn, so one instruction template serves both
/// marker-bound and unbound prompts. The alias is the single declaration of
/// that function-pointer shape, which keeps the profile field readable and
/// lets every consumer name the same type.
type XmlInstructionRenderer = fn(&dyn Fn(&str) -> String) -> String;

/// Renders the ambiguity explanation of one strategy for a given match count.
///
/// The count is the single piece of runtime data an ambiguity carries, so the
/// renderer stays a plain function pointer: the profile itself is pure data.
type AmbiguityHintRenderer = fn(usize) -> String;

/// Everything the harness states about one [`PatchStrategy`].
///
/// A profile is pure data plus one rendering function per contour: it reads no
/// file, keeps no state, and produces identical text for identical input, so the
/// same declaration drives prompt assembly, the wire schema, and the diagnostics
/// of a failed patch.
pub(crate) struct StrategyProfile {
    /// Properties that address the replaced lines, in declaration order.
    pub(crate) replaced_line_fields: &'static [ReplacedLineField],
    /// Description the schema publishes for the `content` property.
    pub(crate) content_description: &'static str,
    /// Instruction of the XML code-modification contour.
    xml_instruction: XmlInstructionRenderer,
    /// Instruction of the structured-output contour.
    pub(crate) structured_instruction: &'static str,
    /// Sentence explaining how a field that does not apply is filled in.
    pub(crate) empty_field_guidance: &'static str,
    /// Contract invariant describing a replacement.
    pub(crate) replace_invariant: &'static str,
    /// Explanation of a replacement that matched no line of the file.
    no_match_hint: &'static str,
    /// Explanation of a replacement that names no line range.
    no_range_hint: &'static str,
    /// Renderer of the explanation of an ambiguous match.
    ambiguous_match_hint: AmbiguityHintRenderer,
    /// Correction guidance appended to the retry request of a failed patch.
    pub(crate) retry_instruction: &'static str,
}

impl StrategyProfile {
    /// Returns the profile of `strategy`.
    ///
    /// The match is exhaustive on purpose: a new strategy does not compile until
    /// it names its profile, so a strategy can never reach a consumer without
    /// the vocabulary that describes it.
    pub(crate) fn of(strategy: PatchStrategy) -> &'static StrategyProfile {
        match strategy {
            PatchStrategy::SearchReplace => &SEARCH_REPLACE,
            PatchStrategy::LineAnchor => &LINE_ANCHOR,
        }
    }

    /// Renders the XML code-edit instruction, resolving control tags by
    /// `tag_name`.
    pub(crate) fn render_xml_instruction(&self, tag_name: &dyn Fn(&str) -> String) -> String {
        (self.xml_instruction)(tag_name)
    }

    /// Explains a failed change-set application in this strategy's dialect.
    ///
    /// The rendered wording reaches the retrying event and the retry prompt
    /// alike, so the two surfaces can never drift into divergent paraphrases.
    /// Only the rejections that carry strategy vocabulary are reworded here:
    /// every other rejection already names its own defect precisely and is
    /// returned unchanged.
    pub(crate) fn describe_failure(&self, err: &EditError) -> String {
        match err {
            EditError::NoMatch { .. } => self.no_match_hint.to_string(),
            EditError::AmbiguousMatch { count, .. } => (self.ambiguous_match_hint)(*count),
            EditError::ReplaceWithoutRange { .. } => self.no_range_hint.to_string(),
            other => other.to_string(),
        }
    }
}

/// Fields of a change entry that echoes the replaced text.
static SEARCH_REPLACE_FIELDS: &[ReplacedLineField] = &[
    ReplacedLineField {
        name: "old_text",
        description:
            "Exact text to replace in the file (required for 'replace', empty string otherwise)",
    },
    ReplacedLineField {
        name: "new_text",
        description: "Replacement text (for 'replace', empty string otherwise)",
    },
];

/// Fields of a change entry that names the replaced range by anchors.
static LINE_ANCHOR_FIELDS: &[ReplacedLineField] = &[
    ReplacedLineField {
        name: "start",
        description: "Opaque anchor of the first replaced line, copied verbatim from <context> (required for 'replace', empty string otherwise)",
    },
    ReplacedLineField {
        name: "end",
        description: "Opaque anchor of the last replaced line, copied verbatim from <context> (required for 'replace', empty string otherwise)",
    },
];

/// Profile of the search/replace strategy.
static SEARCH_REPLACE: StrategyProfile = StrategyProfile {
    replaced_line_fields: SEARCH_REPLACE_FIELDS,
    content_description: "Complete file content (required for 'create' and 'overwrite', empty string otherwise)",
    xml_instruction: search_replace_xml_instruction,
    structured_instruction: "- `changes`: array of edit operations with `op` ('replace'|'create'|'delete'|'move'|'overwrite') plus `path`, `to`, `old_text`, `new_text`, and `content`.",
    empty_field_guidance: "Every field of a change is required: use an empty string \"\" for fields that do not apply to the operation (e.g. `old_text`/`new_text` for 'create'/'delete', `content` for 'replace').\n\n",
    replace_invariant: "For 'replace', `old_text` must match EXACTLY ONE location in the target file, including indentation and whitespace.\n",
    no_match_hint: "Search block not found (0 matches). Verify exact line content, indentation, and context.",
    no_range_hint: SEARCH_REPLACE_NO_RANGE_HINT,
    ambiguous_match_hint: search_replace_ambiguous_match_hint,
    retry_instruction: "Re-read the <context> block and copy the exact text of the lines you replace, including indentation and whitespace, so the pattern matches exactly one location.",
};

/// Profile of the opaque line-anchor strategy.
static LINE_ANCHOR: StrategyProfile = StrategyProfile {
    replaced_line_fields: LINE_ANCHOR_FIELDS,
    content_description: "Complete replacement of the addressed line range for 'replace' (the replaced lines are never echoed back), complete file content for 'create', empty string otherwise",
    xml_instruction: line_anchor_xml_instruction,
    structured_instruction: "- `changes`: array of edit operations with `op` ('replace'|'create'|'delete'|'move') plus `path`, `to`, `start`, `end`, and `content`. A 'replace' entry names the replaced lines by the opaque anchors rendered beside every context line, `start` for the first replaced line and `end` for the last one, and carries the complete replacement of that range in `content`; both anchors must be copied character for character, and the replaced lines are never echoed back. 'create' carries the whole file in `content`, 'move' names its destination in `to`, and both leave `start` and `end` empty. A whole-file rewrite is a 'replace' entry whose range spans the file from its first line to its last one.",
    empty_field_guidance: "Every field of a change is required: use an empty string \"\" for fields that do not apply to the operation (e.g. `start`/`end` for 'create'/'delete', `to` for every operation except 'move').\n\n",
    replace_invariant: "For 'replace', `start` and `end` MUST be anchors rendered in <context> for that same file, the named range MUST NOT overlap another entry, and `content` replaces exactly the addressed lines.\n",
    no_match_hint: "The change set addressed no line of the file. Copy the anchor of the first and of the last replaced line from the <context> block of that same file.",
    no_range_hint: LINE_ANCHOR_NO_RANGE_HINT,
    ambiguous_match_hint: line_anchor_ambiguous_match_hint,
    retry_instruction: "Re-read the <context> block and copy the `start` and `end` anchors character for character: an anchor names a line only in the file it was rendered for, and the ranges of one change set must not overlap.",
};

/// Explanation of a change set that addresses no replaced line, in the
/// search/replace dialect.
const SEARCH_REPLACE_NO_RANGE_HINT: &str =
    "The change set carried no search block. A 'replace' operation requires a non-empty 'old_text' field holding the exact text it replaces.";

/// Explanation of a change set that addresses no replaced line, in the
/// line-anchor dialect.
const LINE_ANCHOR_NO_RANGE_HINT: &str = "The change set addressed no line range. Copy the `start` and `end` anchors of the lines you replace from the <context> block of that same file, or replace the whole file by naming its first and its last line with `start` and `end` when most of the content changes.";

/// Suffix of the search/replace explanation of an ambiguous match.
const SEARCH_REPLACE_AMBIGUOUS_SUFFIX: &str =
    "Include more surrounding lines to make the match uniquely identifiable.";

/// Suffix of the line-anchor explanation of an ambiguous range.
const ANCHOR_AMBIGUOUS_SUFFIX: &str =
    "Send each range exactly once: the ranges of one change set must be disjoint.";

/// Renders the search/replace explanation of an ambiguous match.
fn search_replace_ambiguous_match_hint(count: usize) -> String {
    format!("Search block matches {count} times. {SEARCH_REPLACE_AMBIGUOUS_SUFFIX}")
}

/// Renders the line-anchor explanation of an ambiguous range.
fn line_anchor_ambiguous_match_hint(count: usize) -> String {
    format!("The change set addressed one line range {count} times. {ANCHOR_AMBIGUOUS_SUFFIX}")
}

/// Renders the XML instruction of the search/replace strategy.
fn search_replace_xml_instruction(tag_name: &dyn Fn(&str) -> String) -> String {
    let edits = tag_name("edits");
    let edit = tag_name("edit");
    let search = tag_name("search");
    let replace = tag_name("replace");
    let create = tag_name("create");
    let overwrite = tag_name("overwrite");
    let move_tag = tag_name("move");
    let delete_tag = tag_name("delete");
    format!("- Code edits: wrap operations in <{edits} summary=\"...\">...</{edits}>. Use <{edit} path=\"...\"> with a <{search}>...</{search}> block and a <{replace}>...</{replace}> block for existing files, <{create} path=\"...\">...</{create}> for new files, <{overwrite} path=\"...\">...</{overwrite}> for a full rewrite, <{move_tag} from=\"...\" to=\"...\" /> to move or rename, and <{delete_tag} path=\"...\" /> to delete.\n  - The `summary` attribute of <{edits}> is the Git commit message: imperative mood, English, lowercase, no trailing period.\n  - An <{edit}> `path` MUST match a file present in <context>; a <{create}> path MUST be a valid repository-relative path.\n  - Files inside <context> declare their status through the `access` attribute: `access=\"editable\"` files may be modified directly, while a file marked `access=\"read_only\"` is provided for reference and becomes editable automatically once you propose a change to it.\n  - A <{search}> block MUST match EXACTLY ONE location in the target file, including whitespace and line breaks, and MUST stay as concise as possible while unique.\n  - Prefer <{edit}> for local changes and <{overwrite}> with the COMPLETE new content when most of a file changes; never use <{create}> for a path that already exists.\n  - Several operations may appear inside one <{edits}> block; if no code changes are needed, answer in plain text without any XML edit tags.")
}

/// Renders the XML instruction of the opaque line-anchor strategy.
fn line_anchor_xml_instruction(tag_name: &dyn Fn(&str) -> String) -> String {
    let file_edits = tag_name("file_edits");
    format!("- Code edits: send the whole change set as one JSON array inside <{file_edits}>...</{file_edits}>, as in <{file_edits}>[{{\"op\":\"replace\",\"path\":\"src/lib.rs\",\"to\":\"\",\"start\":\"<first-anchor>\",\"end\":\"<last-anchor>\",\"content\":\"<complete replacement>\"}}]</{file_edits}>.\n  - Every entry names its operation in `op` ('replace', 'create', 'delete', or 'move'), its file in `path`, and, for 'move', its destination in `to`.\n  - A 'replace' entry names the replaced lines by the opaque anchors rendered beside every context line: `start` is the anchor of the first replaced line, `end` the anchor of the last one, and `content` is the complete replacement of that range. Copy both anchors character for character from <context>: they are decoded deterministically, so a re-typed, reordered, or reformatted anchor names a different line and is rejected with a diagnostic. Never echo the replaced text and never renumber the file yourself.\n  - A 'create' entry carries the whole file content in `content`, 'move' names its destination in `to`, and 'delete' needs neither; these operations leave `start` and `end` empty. A whole-file rewrite is a 'replace' entry whose range spans the file from its first line to its last one.\n  - Files inside <context> declare their status through the `access` attribute: `access=\"editable\"` files may be modified directly, while a file marked `access=\"read_only\"` is provided for reference and becomes editable automatically once you propose a change to it. A 'replace' entry `path` MUST match a file present in <context>, while a 'create' path MUST be a valid repository-relative path.\n  - Ranges of one change set MUST NOT overlap; they are applied from the bottom of the file upwards, so every anchor stays valid while the rest of the change set is applied.\n  - The array MUST be valid JSON: a trailing comma or a comment makes the whole change set undecodable.\n  - If no code changes are needed, answer in plain text without the change-set tag.")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolves a tag name without a turn marker.
    fn bare_tag(base: &str) -> String {
        base.to_string()
    }

    #[test]
    fn every_strategy_declares_the_fields_it_addresses_lines_with() {
        for strategy in [PatchStrategy::SearchReplace, PatchStrategy::LineAnchor] {
            let profile = StrategyProfile::of(strategy);
            assert_eq!(
                profile.replaced_line_fields.len(),
                2,
                "{strategy} addresses a range by exactly two fields"
            );
            assert!(
                !profile.content_description.is_empty(),
                "{strategy} describes its content field"
            );
        }
    }

    #[test]
    fn each_strategy_speaks_only_its_own_vocabulary() {
        let search_replace = StrategyProfile::of(PatchStrategy::SearchReplace);
        let line_anchor = StrategyProfile::of(PatchStrategy::LineAnchor);

        let anchored_xml = line_anchor.render_xml_instruction(&bare_tag);
        let anchored_structured = line_anchor.structured_instruction;
        let anchored_guidance = line_anchor.empty_field_guidance;
        let anchored_invariant = line_anchor.replace_invariant;
        let anchored_no_match = line_anchor.no_match_hint;
        let anchored_no_range = line_anchor.no_range_hint;
        let anchored_retry = line_anchor.retry_instruction;
        let anchored_ambiguous = (line_anchor.ambiguous_match_hint)(2);
        for text in [
            anchored_xml.as_str(),
            anchored_structured,
            anchored_guidance,
            anchored_invariant,
            anchored_no_match,
            anchored_no_range,
            anchored_retry,
            anchored_ambiguous.as_str(),
        ] {
            assert!(!text.contains("old_text"), "anchored text names old_text");
            assert!(!text.contains("new_text"), "anchored text names new_text");
            assert!(
                !text.contains("<search>"),
                "anchored text names a search block"
            );
        }

        let echoed_xml = search_replace.render_xml_instruction(&bare_tag);
        let echoed_ambiguous = (search_replace.ambiguous_match_hint)(2);
        for text in [
            echoed_xml.as_str(),
            search_replace.structured_instruction,
            search_replace.empty_field_guidance,
            search_replace.replace_invariant,
            search_replace.no_match_hint,
            search_replace.no_range_hint,
            search_replace.retry_instruction,
            echoed_ambiguous.as_str(),
        ] {
            assert!(!text.contains("anchor"), "echoed text names an anchor");
            assert!(
                !text.contains("file_edits"),
                "echoed text names the change-set vehicle"
            );
        }
    }

    #[test]
    fn the_anchored_profile_never_advertises_the_full_rewrite_operation() {
        // A full rewrite under the line-anchor strategy is a range spanning the
        // whole file, so the separate operation is absent from the vocabulary
        // of the profile, exactly as the strict `op` enum of its schema omits
        // it. The search/replace strategy keeps the operation.
        let anchored = StrategyProfile::of(PatchStrategy::LineAnchor);
        let xml = anchored.render_xml_instruction(&bare_tag);
        let ambiguous = (anchored.ambiguous_match_hint)(2);
        for text in [
            xml.as_str(),
            anchored.structured_instruction,
            anchored.content_description,
            anchored.empty_field_guidance,
            anchored.replace_invariant,
            anchored.no_match_hint,
            anchored.no_range_hint,
            anchored.retry_instruction,
            ambiguous.as_str(),
        ] {
            assert!(
                !text.contains("overwrite"),
                "anchored text names the full rewrite operation: {text}"
            );
        }

        let echoed = StrategyProfile::of(PatchStrategy::SearchReplace);
        let echoed_xml = echoed.render_xml_instruction(&bare_tag);
        assert!(echoed_xml.contains("overwrite"), "{echoed_xml}");
        assert!(echoed.structured_instruction.contains("overwrite"));
        assert!(echoed.content_description.contains("overwrite"));
    }

    #[test]
    fn the_xml_instruction_resolves_the_declared_tags() {
        let profile = StrategyProfile::of(PatchStrategy::SearchReplace);
        let marked = profile.render_xml_instruction(&|base: &str| format!("{base}_M1"));
        assert!(marked.contains("<edits_M1 summary"));
        assert!(!marked.contains("<edits summary"));

        let anchored = StrategyProfile::of(PatchStrategy::LineAnchor)
            .render_xml_instruction(&|base: &str| format!("{base}_M1"));
        assert!(anchored.contains("<file_edits_M1>"));
    }
    #[test]
    fn describe_failure_speaks_the_dialect_of_the_active_strategy() {
        let echoed = StrategyProfile::of(PatchStrategy::SearchReplace).describe_failure(
            &EditError::NoMatch {
                path: "a.rs".to_string(),
            },
        );
        assert!(echoed.starts_with("Search block not found"), "{echoed}");

        let anchored = StrategyProfile::of(PatchStrategy::LineAnchor).describe_failure(
            &EditError::AmbiguousMatch {
                path: "a.rs".to_string(),
                count: 3,
            },
        );
        assert!(!anchored.contains("Search block"), "{anchored}");
        assert!(anchored.contains("line range"), "{anchored}");

        // A rejection that carries no strategy vocabulary keeps its own precise
        // message under every strategy.
        for strategy in [PatchStrategy::SearchReplace, PatchStrategy::LineAnchor] {
            let described = StrategyProfile::of(strategy)
                .describe_failure(&EditError::FileNotFound("a.rs".to_string()));
            assert!(described.contains("does not exist on disk"), "{described}");
        }
    }

    #[test]
    fn describe_failure_rewords_only_the_rejections_that_carry_strategy_vocabulary() {
        // `NoMatch`, `AmbiguousMatch`, and `ReplaceWithoutRange` are the
        // rejections that carry strategy vocabulary, so each dialect rewords
        // them in its own terms. Every other rejection below keeps its own
        // `Display` wording, so both dialects must state one identical,
        // non-empty reason.
        let path = "a.rs".to_string();
        let rejections = [
            EditError::NotInContext(path.clone()),
            EditError::FileNotFound(path.clone()),
            EditError::NoMatch { path: path.clone() },
            EditError::AmbiguousMatch {
                path: path.clone(),
                count: 3,
            },
            EditError::ReplaceWithoutRange { path: path.clone() },
            EditError::AnchorOutOfRange {
                path: path.clone(),
                anchor: "kR7t".to_string(),
                lines: 12,
            },
            EditError::OverlappingAnchorRanges {
                path,
                start: "kR7t".to_string(),
                end: "Qw2n".to_string(),
                other_start: "ZbY9".to_string(),
                other_end: "mDgH".to_string(),
            },
        ];
        let search_replace = StrategyProfile::of(PatchStrategy::SearchReplace);
        let line_anchor = StrategyProfile::of(PatchStrategy::LineAnchor);

        for rejection in &rejections {
            let echoed = search_replace.describe_failure(rejection);
            let anchored = line_anchor.describe_failure(rejection);
            assert!(!echoed.is_empty(), "empty search/replace reason");
            assert!(!anchored.is_empty(), "empty line-anchor reason");
            match rejection {
                EditError::NoMatch { .. } | EditError::AmbiguousMatch { .. } => {
                    assert!(echoed.contains("Search block"), "{echoed}");
                    assert!(!anchored.contains("Search block"), "{anchored}");
                }
                EditError::ReplaceWithoutRange { .. } => {
                    assert!(echoed.contains("old_text"), "{echoed}");
                    assert!(anchored.contains("`start`"), "{anchored}");
                    assert!(!anchored.contains("old_text"), "{anchored}");
                }
                _ => assert_eq!(
                    echoed, anchored,
                    "divergent wordings for a verbatim rejection"
                ),
            }
        }
    }
    #[test]
    fn the_retry_event_and_the_retry_prompt_state_one_reason() {
        // The retrying event and the retry request are rendered from one
        // profile, so a rejected change set is explained identically on both
        // surfaces instead of drifting into divergent paraphrases.
        for strategy in [PatchStrategy::SearchReplace, PatchStrategy::LineAnchor] {
            let reason = crate::workflow::retry::format_patch_retry_reason(
                &EditError::ReplaceWithoutRange {
                    path: "src/lib.rs".to_string(),
                },
                strategy,
            );
            let failed = std::collections::HashMap::from([(
                "src/lib.rs".to_string(),
                EditError::ReplaceWithoutRange {
                    path: "src/lib.rs".to_string(),
                },
            )]);
            let prompt = crate::workflow::retry::build_patch_retry_prompt(
                &std::collections::HashSet::new(),
                &failed,
                &[],
                None,
                strategy,
            );
            assert!(!reason.is_empty(), "{strategy} states no reason");
            assert!(
                prompt.contains(&reason),
                "{strategy} stated divergent wordings: {prompt}"
            );
        }
    }
}
