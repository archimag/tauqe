//! Capability registry and the autonomous capability-module contract.
//!
//! A [`CapabilityModule`] is the single source of truth for one protocol
//! capability: its XML tag schema, its prompt fragments, its output decoder,
//! and its sanitizer. The [`CapabilityRegistry`] holds the fixed set of
//! built-in modules and exposes declarative composition guarded by a
//! [`CapabilitySet`](tauqe_protocol::CapabilitySet).

use std::sync::OnceLock;

use tauqe_protocol::{
    CapabilityPhase, CapabilitySet, DirListRequest, DocTopic, LlmCapability, LlmCommand,
    ModelResult, RepoSearchRequest, TurnIntent,
};

use crate::edits::protocol::xml::engine::{self, XmlTagSpec};
use crate::edits::protocol::xml::normalize_marked_xml;
use crate::edits::protocol::xml::tags::ParsedPlanTag;
use crate::edits::protocol::{
    ContextRequest, MarkedXmlEditProtocol, TerminalOutcome, VerifyRequest, XmlEditProtocol,
};

use super::builtin::{
    BlockedModule, DiscoveryModule, DocRequestModule, FileEditsModule, OutcomeModule,
    PlanCreationModule, PlanUpdateModule, ScratchpadModule, TextMessageModule,
    ToolchainVerifyModule, UserLanguageModule,
};
use super::regime;

/// Context handed to capability modules when they render prompt fragments.
///
/// Both fields are pure render inputs: the permitted editable paths and the
/// optional per-turn marker suffix. Rendering never performs I/O.
pub struct PromptRenderContext<'a> {
    /// Repository-relative paths the model may modify this turn.
    pub editable_paths: &'a [String],
    /// Active turn-marker suffix, when the protocol is marker-bound.
    pub marker: Option<&'a str>,
}

impl<'a> PromptRenderContext<'a> {
    /// Creates a render context for the given editable paths and marker.
    pub fn new(editable_paths: &'a [String], marker: Option<&'a str>) -> Self {
        Self {
            editable_paths,
            marker,
        }
    }

    /// Returns a tag name with the active turn-marker suffix applied.
    pub fn tag_name(&self, base: &str) -> String {
        match self.marker {
            Some(marker) => format!("{}_{}", base, marker),
            None => base.to_string(),
        }
    }
}

/// One rule of the `Core System Contract` section.
///
/// A rule is data, never a numbered string literal. The number is derived from
/// the rule's position by [`render_contract_rules`], so the rendered sequence is
/// always dense `1..N` and no surrogate numbering (`1a.`, `10a.`) can appear.
pub struct ContractRule {
    /// Short title stating the invariant.
    pub title: String,
    /// Body text rendered after the title.
    pub body: String,
}

impl ContractRule {
    /// Creates a contract rule from its title and body.
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
        }
    }
}

/// Renders contract rules as a densely numbered `1..N` list.
pub fn render_contract_rules(rules: &[ContractRule]) -> String {
    let mut rendered = String::new();
    for (index, rule) in rules.iter().enumerate() {
        rendered.push_str(&format!(
            "{}. {}: {}\n",
            index + 1,
            rule.title,
            rule.body.trim_end()
        ));
    }
    rendered
}

/// Render inputs available to a contract rule.
///
/// Tags keep folding through [`PromptRenderContext`], and the rule variant is
/// selected by typed inputs — the declared turn intent and the concrete target
/// language — never by inspecting rendered text.
pub struct ContractContext<'a> {
    /// Tag-rendering context shared with the protocol instruction composers.
    pub render: PromptRenderContext<'a>,
    /// Declared intent of the turn this contract is rendered for.
    pub intent: TurnIntent,
    /// Concrete response language, when the caller already resolved it.
    pub target_language: Option<&'a str>,
}

impl<'a> ContractContext<'a> {
    /// Creates a contract context for the given intent and render context.
    pub fn new(intent: TurnIntent, render: PromptRenderContext<'a>) -> Self {
        Self {
            render,
            intent,
            target_language: None,
        }
    }

    /// Sets the concrete target language, when the caller already resolved it.
    pub fn with_target_language(mut self, target_language: Option<&'a str>) -> Self {
        self.target_language = target_language;
        self
    }

    /// Returns a tag name with the active turn-marker suffix applied.
    pub fn tag_name(&self, base: &str) -> String {
        self.render.tag_name(base)
    }
}

/// One autonomous capability: tags, prompt text, decoding, and sanitization.
///
/// A module advertises exactly one [`LlmCapability`] and belongs to exactly one
/// [`CapabilityPhase`]. Consumers never branch on individual tag names; they
/// compose modules through a [`CapabilitySet`].
pub trait CapabilityModule: Send + Sync {
    /// Capability realized by this module.
    fn capability(&self) -> LlmCapability;

    /// Phase of the bounded turn this capability belongs to.
    fn phase(&self) -> CapabilityPhase {
        self.capability().phase()
    }

    /// Tag specs the module recognizes, or an empty slice for tag-less modules.
    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        &[]
    }

    /// Structured-output property this module populates, when it maps to one.
    fn json_property_name(&self) -> Option<&'static str> {
        None
    }

    /// Rule contributed to the `Core System Contract` section.
    fn render_contract_rule(&self, _ctx: &ContractContext<'_>) -> Option<ContractRule> {
        None
    }

    /// Instruction contributed to the XML code-modification protocol section.
    fn render_xml_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        None
    }

    /// Instruction contributed to the structured-output protocol section.
    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        None
    }

    /// Decodes the module's capability from model output into normalized commands.
    fn decode_xml(
        &self,
        _normalized_text: &str,
        _raw_text: &str,
        _marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        Vec::new()
    }

    /// Removes the module's tags from conversational text.
    fn strip_xml(&self, text: &str) -> String {
        let mut out = text.to_string();
        for spec in self.xml_tag_specs() {
            out = engine::strip_tag_by_spec(&out, spec);
        }
        out
    }
}

/// Marker-aware adapter over the two XML edit-protocol variants.
///
/// Both variants decode the same grammar; the marker-bound variant normalizes
/// the turn marker internally and restores marker-mangled literals afterward.
pub(crate) enum XmlParser {
    Unmarked(XmlEditProtocol),
    Marked(MarkedXmlEditProtocol),
}

impl XmlParser {
    /// Selects the marker-bound or unbound parser for the active turn.
    pub(crate) fn new(marker: Option<&str>) -> Self {
        match marker {
            Some(marker) => Self::Marked(MarkedXmlEditProtocol::new(marker)),
            None => Self::Unmarked(XmlEditProtocol),
        }
    }

    pub(crate) fn clean_assistant_text(&self, text: &str) -> String {
        match self {
            Self::Unmarked(protocol) => protocol.clean_assistant_text(text),
            Self::Marked(protocol) => protocol.clean_assistant_text(text),
        }
    }

    pub(crate) fn parse_output(&self, normalized: &str, editable_paths: &[String]) -> ModelResult {
        match self {
            Self::Unmarked(protocol) => protocol.parse_output(normalized, editable_paths),
            Self::Marked(protocol) => protocol.parse_output_normalized(normalized, editable_paths),
        }
    }

    pub(crate) fn parse_context_requests(&self, normalized: &str) -> Vec<ContextRequest> {
        match self {
            Self::Unmarked(protocol) => protocol.parse_context_requests(normalized),
            Self::Marked(protocol) => protocol.parse_context_requests_normalized(normalized),
        }
    }

    pub(crate) fn parse_repo_searches(&self, normalized: &str) -> Vec<RepoSearchRequest> {
        match self {
            Self::Unmarked(protocol) => protocol.parse_repo_searches(normalized),
            Self::Marked(protocol) => protocol.parse_repo_searches_normalized(normalized),
        }
    }

    pub(crate) fn parse_repo_dirs(&self, normalized: &str) -> bool {
        match self {
            Self::Unmarked(protocol) => protocol.parse_repo_dirs(normalized),
            Self::Marked(protocol) => protocol.parse_repo_dirs_normalized(normalized),
        }
    }

    pub(crate) fn parse_repo_maps(&self, normalized: &str) -> Vec<String> {
        match self {
            Self::Unmarked(protocol) => protocol.parse_repo_maps(normalized),
            Self::Marked(protocol) => protocol.parse_repo_maps_normalized(normalized),
        }
    }

    pub(crate) fn parse_dir_lists(&self, normalized: &str) -> Vec<DirListRequest> {
        match self {
            Self::Unmarked(protocol) => protocol.parse_dir_lists(normalized),
            Self::Marked(protocol) => protocol.parse_dir_lists_normalized(normalized),
        }
    }

    pub(crate) fn parse_doc_requests(&self, normalized: &str) -> Vec<DocTopic> {
        match self {
            Self::Unmarked(protocol) => protocol.parse_doc_requests(normalized),
            Self::Marked(protocol) => protocol.parse_doc_requests_normalized(normalized),
        }
    }

    pub(crate) fn parse_plan_tags(&self, normalized: &str) -> (Vec<ParsedPlanTag>, Vec<String>) {
        match self {
            Self::Unmarked(protocol) => protocol.parse_plan_tags(normalized),
            Self::Marked(protocol) => protocol.parse_plan_tags_normalized(normalized),
        }
    }

    pub(crate) fn parse_verify_request(
        &self,
        normalized: &str,
        raw_text: &str,
    ) -> Option<VerifyRequest> {
        match self {
            Self::Unmarked(protocol) => protocol.parse_verify_request(normalized),
            Self::Marked(protocol) => {
                protocol.parse_verify_request_normalized(normalized, raw_text)
            }
        }
    }

    pub(crate) fn parse_scratchpad(&self, normalized: &str, raw_text: &str) -> Option<String> {
        match self {
            Self::Unmarked(protocol) => protocol.parse_scratchpad(normalized),
            Self::Marked(protocol) => protocol.parse_scratchpad_normalized(normalized, raw_text),
        }
    }

    pub(crate) fn parse_terminal_outcome(&self, normalized: &str) -> Option<TerminalOutcome> {
        match self {
            Self::Unmarked(protocol) => protocol.parse_terminal_outcome(normalized),
            Self::Marked(protocol) => protocol.parse_terminal_outcome_normalized(normalized),
        }
    }

    pub(crate) fn parse_user_language(&self, normalized: &str, raw_text: &str) -> Option<String> {
        match self {
            Self::Unmarked(protocol) => protocol.parse_user_language(normalized),
            Self::Marked(protocol) => protocol.parse_user_language_normalized(normalized, raw_text),
        }
    }
}

/// Resolves tag specs by base name, caching the result for the process lifetime.
pub(crate) fn resolve_specs(
    cache: &'static OnceLock<Vec<&'static XmlTagSpec>>,
    bases: &[&'static str],
) -> &'static [&'static XmlTagSpec] {
    cache
        .get_or_init(|| {
            bases
                .iter()
                .filter_map(|base| engine::tag_spec(base))
                .collect()
        })
        .as_slice()
}

/// Deterministic registry of every built-in capability module.
pub struct CapabilityRegistry {
    modules: &'static [&'static dyn CapabilityModule],
}

static MODULES: &[&dyn CapabilityModule] = &[
    &TextMessageModule,
    &FileEditsModule,
    &DiscoveryModule,
    &DocRequestModule,
    &ToolchainVerifyModule,
    &PlanCreationModule,
    &PlanUpdateModule,
    &ScratchpadModule,
    &OutcomeModule,
    &BlockedModule,
    &UserLanguageModule,
];

impl CapabilityRegistry {
    /// Returns the process-wide registry singleton.
    pub fn global() -> &'static CapabilityRegistry {
        static REGISTRY: OnceLock<CapabilityRegistry> = OnceLock::new();
        REGISTRY.get_or_init(|| CapabilityRegistry { modules: MODULES })
    }

    /// Returns the module that realizes `capability`, if any.
    pub fn module(&self, capability: LlmCapability) -> Option<&'static dyn CapabilityModule> {
        self.modules
            .iter()
            .copied()
            .find(|module| module.capability() == capability)
    }

    /// Iterates the modules enabled by `set`, in registry order.
    fn enabled<'a>(
        &'a self,
        set: &'a CapabilitySet,
    ) -> impl Iterator<Item = &'static dyn CapabilityModule> + 'a {
        self.modules
            .iter()
            .copied()
            .filter(move |module| set.contains(module.capability()))
    }

    /// Collects the control tag names the enabled modules may emit.
    ///
    /// Only control tags are returned; code-edit tags are omitted because the
    /// operational tag list describes protocol control tags, not edit blocks.
    pub fn operational_xml_tags(&self, set: &CapabilitySet) -> Vec<String> {
        let mut tags: Vec<String> = Vec::new();
        for module in self.enabled(set) {
            for spec in module.xml_tag_specs() {
                if spec.is_control()
                    && !tags
                        .iter()
                        .any(|existing| existing.as_str() == spec.base_name)
                {
                    tags.push(spec.base_name.to_string());
                }
            }
        }
        tags
    }

    /// Composes `Core System Contract` rules for the enabled capabilities.
    ///
    /// The section is assembled from typed inputs only: the regime trunk
    /// selected by the declared turn intent and the capability surface, the
    /// operational tag rule derived from the enabled capabilities, and every
    /// enabled module's rule in registry order. The whole sequence is rendered
    /// by one loop, so rule numbers come purely from position.
    pub fn compose_contract_rules(&self, set: &CapabilitySet, ctx: &ContractContext<'_>) -> String {
        let mut rules = regime::regime_contract_rules(ctx, set);
        if let Some(rule) = self.operational_tags_rule(set, ctx) {
            // Position 0 always holds the authoritative-source rule, so the
            // operational-tag rule becomes the second entry of the contract.
            rules.insert(rules.len().min(1), rule);
        }
        for module in self.enabled(set) {
            if let Some(rule) = module.render_contract_rule(ctx) {
                rules.push(rule);
            }
        }
        render_contract_rules(&rules)
    }

    /// Builds the operational tag rule from the enabled capability set.
    ///
    /// The rule is skipped when the set exposes no control tags, so a narrowed
    /// surface never advertises an operational protocol it cannot accept.
    fn operational_tags_rule(
        &self,
        set: &CapabilitySet,
        ctx: &ContractContext<'_>,
    ) -> Option<ContractRule> {
        let tags = self.operational_xml_tags(set);
        if tags.is_empty() {
            return None;
        }
        let listed = tags
            .iter()
            .map(|tag| format!("<{}>", ctx.tag_name(tag)))
            .collect::<Vec<_>>()
            .join(", ");
        let example = format!("<{} ...>", ctx.tag_name("context_request"));
        Some(ContractRule::new(
            "Operational Protocol Tags",
            format!(
                "All operational protocol tags ({}) must be emitted strictly at the end of your response outside markdown code blocks. If you mention, quote, or discuss tags in conversational text, you MUST enclose them in backticks (e.g. `{}`) so they are treated as literal text and not operational commands.",
                listed, example
            ),
        ))
    }

    /// Composes XML code-modification instructions for the enabled capabilities.
    pub fn compose_xml_protocol_instructions(
        &self,
        set: &CapabilitySet,
        ctx: &PromptRenderContext<'_>,
    ) -> String {
        self.compose(set, ctx, |module, render_ctx| {
            module.render_xml_instruction(render_ctx)
        })
    }

    /// Composes structured-output instructions for the enabled capabilities.
    pub fn compose_structured_protocol_instructions(
        &self,
        set: &CapabilitySet,
        ctx: &PromptRenderContext<'_>,
    ) -> String {
        self.compose(set, ctx, |module, render_ctx| {
            module.render_structured_instruction(render_ctx)
        })
    }

    /// Decodes model output into normalized commands for the enabled capabilities.
    ///
    /// Each enabled module decodes its own capability; the resulting command list
    /// is then filtered so that a command whose capability is disabled (for
    /// example a plan save while only plan updates are allowed) never survives.
    pub fn decode_xml_commands(
        &self,
        set: &CapabilitySet,
        raw_text: &str,
        marker: Option<&str>,
        editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let normalized_storage;
        let normalized_text = match marker {
            Some(m) => {
                normalized_storage = normalize_marked_xml(raw_text, m, true);
                normalized_storage.as_str()
            }
            None => raw_text,
        };
        let mut commands = Vec::new();
        for module in self.enabled(set) {
            commands.extend(module.decode_xml(normalized_text, raw_text, marker, editable_paths));
        }
        commands.retain(|command| match command.capability() {
            Some(capability) => set.contains(capability),
            None => true,
        });
        commands
    }

    /// Produces conversational text with the enabled capability tags removed.
    ///
    /// When the text capability is enabled the full protocol cleaning is used;
    /// otherwise stripping is scoped strictly to the enabled tag specs.
    pub fn clean_text(&self, set: &CapabilitySet, raw_text: &str, marker: Option<&str>) -> String {
        if set.contains(LlmCapability::TextMessage) {
            return XmlParser::new(marker).clean_assistant_text(raw_text);
        }
        let mut text = raw_text.to_string();
        for module in self.enabled(set) {
            text = module.strip_xml(&text);
        }
        text
    }

    fn compose(
        &self,
        set: &CapabilitySet,
        ctx: &PromptRenderContext<'_>,
        render: impl Fn(&dyn CapabilityModule, &PromptRenderContext<'_>) -> Option<String>,
    ) -> String {
        let mut blocks: Vec<String> = Vec::new();
        for module in self.enabled(set) {
            if let Some(block) = render(module, ctx) {
                blocks.push(block);
            }
        }
        blocks.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects the rule numbers rendered at the start of contract lines.
    fn rule_numbers(rendered: &str) -> Vec<usize> {
        rendered
            .lines()
            .filter_map(|line| {
                let (prefix, _) = line.split_once(". ")?;
                prefix.parse::<usize>().ok()
            })
            .collect()
    }

    /// Builds the default contract context for a plain interactive turn.
    fn plain_context() -> ContractContext<'static> {
        ContractContext::new(
            TurnIntent::InteractiveRepl,
            PromptRenderContext::new(&[], None),
        )
    }

    #[test]
    fn registry_covers_every_capability() {
        let registry = CapabilityRegistry::global();
        for capability in LlmCapability::ALL {
            assert!(
                registry.module(*capability).is_some(),
                "missing module for {}",
                capability
            );
        }
    }

    #[test]
    fn module_capability_and_phase_match() {
        let registry = CapabilityRegistry::global();
        for capability in LlmCapability::ALL {
            let module = match registry.module(*capability) {
                Some(module) => module,
                None => panic!("missing module for {}", capability),
            };
            assert_eq!(module.capability(), *capability);
            assert_eq!(module.phase(), capability.phase());
        }
    }

    #[test]
    fn operational_xml_tags_are_filtered_and_deterministic() {
        let registry = CapabilityRegistry::global();
        let develop = registry.operational_xml_tags(&CapabilitySet::develop_xml());
        assert_eq!(
            develop,
            vec![
                "context_request".to_string(),
                "repo_search".to_string(),
                "repo_dirs".to_string(),
                "repo_map".to_string(),
                "dir_list".to_string(),
                "doc_request".to_string(),
                "verify".to_string(),
                "plan".to_string(),
                "scratchpad".to_string(),
                "success".to_string(),
                "error".to_string(),
                "blocked".to_string(),
                "user_language".to_string(),
            ]
        );
        assert_eq!(
            registry.operational_xml_tags(&CapabilitySet::develop_xml()),
            develop
        );

        let isolated = registry.operational_xml_tags(&CapabilitySet::isolated_step_xml());
        assert!(isolated.iter().any(|tag| tag == "context_request"));
        assert!(!isolated.iter().any(|tag| tag == "plan"));
    }

    #[test]
    fn contract_rules_reflect_the_capability_set() {
        let registry = CapabilityRegistry::global();
        let ctx = plain_context();

        let develop = registry.compose_contract_rules(&CapabilitySet::develop_xml(), &ctx);
        assert!(develop.contains("context_request"));
        assert!(develop.contains("Minimal Coherent Change"));
        assert!(develop.contains("1. Authoritative Source: "));
        assert!(develop.contains("Operational Protocol Tags: "));

        let discussion = registry.compose_contract_rules(&CapabilitySet::discussion(), &ctx);
        assert!(!discussion.contains("Minimal Coherent Change"));
    }

    #[test]
    fn contract_rules_are_numbered_monotonically() {
        let rules = vec![
            ContractRule::new("First", "Alpha."),
            ContractRule::new("Second", "Beta.\n"),
        ];
        assert_eq!(
            render_contract_rules(&rules),
            "1. First: Alpha.\n2. Second: Beta.\n"
        );
    }

    #[test]
    fn contract_rules_keep_dense_numbering_for_narrowed_sets() {
        let registry = CapabilityRegistry::global();
        let narrowed =
            CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::FileEdits]);
        let rendered = registry.compose_contract_rules(&narrowed, &plain_context());

        // File-edit tags are not control tags, so the operational rule is
        // skipped instead of advertising an empty protocol surface.
        assert!(!rendered.contains("Operational Protocol Tags"));
        assert_eq!(rule_numbers(&rendered), vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(rendered.contains("1. Authoritative Source: "));
        assert!(rendered.contains("2. Read-Only Scope: "));
        assert!(rendered.contains("3. Editable Scope: "));
        assert!(rendered.contains("4. No Arbitrary Shell: "));
        assert!(rendered.contains("5. Action Authorization: "));
        assert!(rendered.contains("6. Reasoning in English: "));
        assert!(rendered.contains("7. Conversational Response: "));
        assert!(rendered.contains("8. Minimal Coherent Change: "));
    }

    #[test]
    fn develop_contract_has_no_lettered_rule_numbers() {
        let registry = CapabilityRegistry::global();
        let rendered =
            registry.compose_contract_rules(&CapabilitySet::develop_xml(), &plain_context());

        for surrogate in ["1a.", "2a.", "4a.", "8a.", "10a.", "11a."] {
            assert!(!rendered.contains(surrogate), "surrogate {surrogate}");
        }
        let numbers = rule_numbers(&rendered);
        assert!(numbers.len() > 5);
        assert_eq!(numbers, (1..=numbers.len()).collect::<Vec<_>>());
        assert!(rendered.contains("Conceptual Foundation Invariant"));
        assert!(rendered.contains("Leaf-Only Files Invariant"));
        assert!(rendered.contains("File Attributes Contract"));
        assert!(rendered.contains("Planning Objective: "));
        assert!(rendered.contains("Step Formulation Contract: "));
        assert!(rendered.contains("Structured Plan XML Format:"));
    }

    #[test]
    fn contract_rules_follow_typed_inputs() {
        let registry = CapabilityRegistry::global();
        let set = CapabilitySet::develop_xml();

        let rendered = registry.compose_contract_rules(&set, &plain_context());
        assert!(rendered.contains("Language Consistency: Always formulate"));

        let localized = plain_context().with_target_language(Some("Russian"));
        let rendered = registry.compose_contract_rules(&set, &localized);
        assert!(rendered.contains("Language Consistency: The user's language is Russian."));

        let discussion = ContractContext::new(
            TurnIntent::ReadonlyDiscussion,
            PromptRenderContext::new(&[], None),
        );
        let rendered = registry.compose_contract_rules(&CapabilitySet::discussion(), &discussion);
        assert!(rendered.contains("Language Consistency: Always formulate"));
        assert!(rendered.contains("`user_language` field"));
    }

    #[test]
    fn contract_rules_apply_the_turn_marker() {
        let registry = CapabilityRegistry::global();
        let ctx = ContractContext::new(
            TurnIntent::InteractiveRepl,
            PromptRenderContext::new(&[], Some("M12345")),
        );
        let rendered = registry.compose_contract_rules(&CapabilitySet::develop_xml(), &ctx);
        assert!(rendered.contains("<context_request_M12345 path"));
        assert!(rendered.contains("<scratchpad_M12345>"));
        assert!(rendered.contains("<plan_M12345 action"));
        assert!(!rendered.contains("<context_request path"));
    }

    #[test]
    fn xml_instructions_apply_the_turn_marker() {
        let registry = CapabilityRegistry::global();
        let marker = "M12345";
        let ctx = PromptRenderContext::new(&[], Some(marker));
        let marked =
            registry.compose_xml_protocol_instructions(&CapabilitySet::develop_xml(), &ctx);
        assert!(marked.contains(&format!("tauqe_edits_{marker}")));
        assert!(marked.contains(&format!("context_request_{marker}")));

        let plain_ctx = PromptRenderContext::new(&[], None);
        let plain =
            registry.compose_xml_protocol_instructions(&CapabilitySet::develop_xml(), &plain_ctx);
        assert!(plain.contains("<tauqe_edits summary"));
    }

    #[test]
    fn structured_instructions_reflect_the_capability_set() {
        let registry = CapabilityRegistry::global();
        let ctx = PromptRenderContext::new(&[], None);

        let json = registry
            .compose_structured_protocol_instructions(&CapabilitySet::develop_structured(), &ctx);
        assert!(json.contains("changes"));
        assert!(json.contains("message"));
        assert!(json.contains("`doc_requests`"));
        assert!(json.contains("`scratchpad`"));
        assert!(json.contains("`outcome`"));
        assert!(json.contains("`blocked`"));
        assert!(json.contains("`verify`"));

        let discussion =
            registry.compose_structured_protocol_instructions(&CapabilitySet::discussion(), &ctx);
        assert!(!discussion.contains("`changes`"));
        assert!(discussion.contains("message"));
        assert!(!discussion.contains("`doc_requests`"));
        assert!(!discussion.contains("`scratchpad`"));
        assert!(!discussion.contains("`outcome`"));
        assert!(!discussion.contains("`blocked`"));

        let narrowed = registry.compose_structured_protocol_instructions(
            &CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::FileEdits]),
            &ctx,
        );
        assert!(narrowed.contains("`changes`"));
        assert!(!narrowed.contains("`doc_requests`"));
        assert!(!narrowed.contains("`scratchpad`"));
        assert!(!narrowed.contains("`outcome`"));
        assert!(!narrowed.contains("`blocked`"));
    }

    #[test]
    fn decode_filters_commands_by_capability_set() {
        let registry = CapabilityRegistry::global();
        let raw = concat!(
            "Applying the plan.\n",
            "<context_request_AB path=\"src/a.rs\" access=\"editable\" />\n",
            "<dir_list_AB path=\"crates/core\" recursive=\"true\" />\n",
            "<plan_AB action=\"save\" id=\"p1\" title=\"Plan\">",
            "<concept_AB><concept_item_AB id=\"c-1\" title=\"Concept\" /></concept_AB>",
            "<item id=\"1\" title=\"Step\" status=\"todo\" />",
            "</plan_AB>\n",
        );

        let develop =
            registry.decode_xml_commands(&CapabilitySet::develop_xml(), raw, Some("AB"), &[]);
        assert!(develop
            .iter()
            .any(|command| matches!(command, LlmCommand::Plan { .. })));
        assert!(develop
            .iter()
            .any(|command| matches!(command, LlmCommand::ContextRequest { .. })));
        assert!(develop.iter().any(|command| matches!(
            command,
            LlmCommand::DirList { path, recursive }
                if path == "crates/core" && *recursive
        )));
        assert!(develop
            .iter()
            .any(|command| matches!(command, LlmCommand::Text { .. })));

        let isolated =
            registry.decode_xml_commands(&CapabilitySet::isolated_step_xml(), raw, Some("AB"), &[]);
        assert!(!isolated
            .iter()
            .any(|command| matches!(command, LlmCommand::Plan { .. })));
        assert!(isolated
            .iter()
            .any(|command| matches!(command, LlmCommand::ContextRequest { .. })));
        assert!(isolated
            .iter()
            .any(|command| matches!(command, LlmCommand::DirList { .. })));
    }

    #[test]
    fn clean_text_removes_enabled_capability_tags() {
        let registry = CapabilityRegistry::global();
        let raw = "Context gathered.\n<context_request path=\"src/a.rs\" />\n<dir_list path=\"crates/core\" />\n<scratchpad>\nnotes\n</scratchpad>\nDone.";
        let cleaned = registry.clean_text(&CapabilitySet::develop_xml(), raw, None);
        assert!(!cleaned.contains("context_request"));
        assert!(!cleaned.contains("dir_list"));
        assert!(!cleaned.contains("<scratchpad"));
        assert!(cleaned.contains("Context gathered."));
        assert!(cleaned.contains("Done."));
    }

    #[test]
    fn clean_text_falls_back_to_capability_scoped_stripping() {
        let registry = CapabilityRegistry::global();
        let raw = "Keep <success /> but strip <context_request path=\"a.rs\" />.";
        let set = CapabilitySet::from_slice(&[LlmCapability::Discovery]);
        let cleaned = registry.clean_text(&set, raw, None);
        assert!(!cleaned.contains("context_request"));
        assert!(cleaned.contains("success"));
    }

    #[test]
    fn json_property_names_are_exposed_for_structured_capabilities() {
        let registry = CapabilityRegistry::global();
        let expectations = [
            (LlmCapability::TextMessage, "message"),
            (LlmCapability::FileEdits, "changes"),
            (LlmCapability::DocRequest, "doc_requests"),
            (LlmCapability::PlanCreation, "plan_update"),
            (LlmCapability::PlanUpdate, "plan_update"),
            (LlmCapability::ToolchainVerify, "verify"),
            (LlmCapability::Scratchpad, "scratchpad"),
            (LlmCapability::Outcome, "outcome"),
            (LlmCapability::Blocked, "blocked"),
            (LlmCapability::UserLanguage, "user_language"),
        ];
        for (capability, expected) in expectations {
            let property = registry
                .module(capability)
                .and_then(|module| module.json_property_name());
            assert_eq!(property, Some(expected), "property name for {}", capability);
        }
    }
}
