//! Capability registry and the autonomous capability-module contract.
//!
//! A [`CapabilityModule`] is the single source of truth for one protocol
//! capability: its XML tag schema, its prompt fragments, its output decoder,
//! and its sanitizer. The [`CapabilityRegistry`] holds the fixed set of
//! built-in modules and exposes declarative composition guarded by a
//! [`CapabilitySet`](tauqe_protocol::CapabilitySet).

use std::sync::OnceLock;

use tauqe_protocol::{
    CapabilityPhase, CapabilitySet, DirListRequest, DocTopic, EditProtocolKind, LlmCapability,
    LlmCommand, ModelResult, PatchStrategy, ProjectConfig, PromptLayoutKind, RepoSearchRequest,
    TurnIntent,
};

use crate::edits::protocol::xml::engine::{self, XmlTagSpec};
use crate::edits::protocol::xml::normalize_marked_xml;
use crate::edits::protocol::xml::tags::ParsedPlanTag;
use crate::edits::protocol::{
    ContextRequest, MarkedXmlEditProtocol, TerminalOutcome, VerifyRequest, XmlEditProtocol,
};

use super::builtin::{
    context_surface_rule, AbortModule, BlockedModule, CustomToolsModule, DiscoveryModule,
    DocRequestModule, FileEditsModule, OutcomeModule, PlanCreationModule, PlanUpdateModule,
    ScratchpadModule, TextMessageModule, ToolchainVerifyModule, UserLanguageModule,
};
use super::regime;

/// Splitting a regime surface into the byte-stable head and the contour delta.
///
/// A regime profile already declares two typed lists, so the registry needs
/// exactly two passes over them: the core pass renders what every dialogue
/// shares, and the delta pass renders what only the active regime states. Both
/// passes select modules through the profile lists and never through the text a
/// module renders, and each pass numbers its own rules densely from one.
impl CapabilityRegistry {
    /// Iterates the modules enabled by `set` that also belong to `allowed`.
    fn enabled_in<'a>(
        &'a self,
        set: &'a CapabilitySet,
        allowed: &'a CapabilitySet,
    ) -> impl Iterator<Item = &'static dyn CapabilityModule> + 'a {
        self.modules.iter().copied().filter(move |module| {
            set.contains(module.capability()) && allowed.contains(module.capability())
        })
    }

    /// Composes the core rules that render into the byte-stable head.
    ///
    /// The pass opens with the shared trunk, states the context surface of the
    /// active layout, adds the operational tag rule for the XML contour, and then
    /// folds the core rules of the modules the regime profile places in the head.
    /// Those rules are intent-independent by contract, so the head stays
    /// byte-identical for the REPL, a plan discussion, and a review discussion.
    pub fn compose_core_contract_rules(
        &self,
        set: &CapabilitySet,
        ctx: &ContractContext<'_>,
    ) -> String {
        let profile = ctx.intent.regime_profile();
        let mut rules = regime::shared_trunk_rules(ctx, set);
        rules.insert(rules.len().min(1), context_surface_rule(ctx));
        if ctx.protocol == EditProtocolKind::Xml {
            if let Some(rule) = self.operational_tags_rule(set, ctx) {
                rules.insert(rules.len().min(2), rule);
            }
        }
        for module in self.enabled_in(set, profile.core()) {
            if let Some(rule) = module.render_contract_rule(ctx) {
                rules.push(rule);
            }
        }
        render_contract_rules(&rules)
    }

    /// Composes the contour delta that renders into the trailing user message.
    ///
    /// The pass carries the intent delta of the declared regime plus the hooks of
    /// the profile lists: a core capability states there what only its regime
    /// authorizes, while a delta capability states the whole surface it adds to
    /// the head. A rule therefore lands in exactly one of the two passes.
    pub fn compose_delta_contract_rules(
        &self,
        set: &CapabilitySet,
        ctx: &ContractContext<'_>,
    ) -> String {
        let profile = ctx.intent.regime_profile();
        let mut rules = regime::intent_delta_rules(ctx, set);
        for module in self.enabled_in(set, profile.core()) {
            if let Some(rule) = module.render_delta_rule(ctx) {
                rules.push(rule);
            }
        }
        for module in self.enabled_in(set, profile.delta()) {
            if let Some(rule) = module.render_contract_rule(ctx) {
                rules.push(rule);
            }
            if let Some(rule) = module.render_delta_rule(ctx) {
                rules.push(rule);
            }
        }
        render_contract_rules(&rules)
    }

    /// Composes the delta XML instructions of the declared regime.
    ///
    /// Only the modules of the profile delta contribute, so the head keeps the
    /// instruction block every dialogue shares while the tail carries the
    /// regime-specific one.
    pub fn compose_delta_xml_instructions(
        &self,
        set: &CapabilitySet,
        ctx: &PromptRenderContext<'_>,
        intent: TurnIntent,
    ) -> String {
        let profile = intent.regime_profile();
        self.compose_in(set, profile.delta(), ctx, |module, render_ctx| {
            module.render_xml_instruction(render_ctx)
        })
    }

    /// Composes the delta structured-output instructions of the declared regime.
    pub fn compose_delta_structured_protocol_instructions(
        &self,
        set: &CapabilitySet,
        ctx: &PromptRenderContext<'_>,
        intent: TurnIntent,
    ) -> String {
        let profile = intent.regime_profile();
        self.compose_in(set, profile.delta(), ctx, |module, render_ctx| {
            module.render_structured_instruction(render_ctx)
        })
    }

    /// Folds the rendered fragments of the modules enabled by both sets.
    fn compose_in(
        &self,
        set: &CapabilitySet,
        allowed: &CapabilitySet,
        ctx: &PromptRenderContext<'_>,
        render: impl Fn(&dyn CapabilityModule, &PromptRenderContext<'_>) -> Option<String>,
    ) -> String {
        let mut blocks: Vec<String> = Vec::new();
        for module in self.enabled_in(set, allowed) {
            if let Some(block) = render(module, ctx) {
                blocks.push(block);
            }
        }
        blocks.join("\n")
    }
}

#[cfg(test)]
mod delta_tests {
    use super::*;

    /// Builds the contract context of one intent for the XML contour.
    fn xml_context(intent: TurnIntent) -> ContractContext<'static> {
        ContractContext::new(
            intent,
            EditProtocolKind::Xml,
            PromptRenderContext::new(&[], None),
        )
    }

    /// Collects the rule numbers rendered at the start of contract lines.
    fn rule_numbers(rendered: &str) -> Vec<usize> {
        rendered
            .lines()
            .filter_map(|line| {
                let (prefix, rest) = line.split_once(". ")?;
                let starts_title = rest
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_uppercase())
                    .unwrap_or(false);
                if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_digit()) || !starts_title
                {
                    return None;
                }
                prefix.parse::<usize>().ok()
            })
            .collect()
    }

    #[test]
    fn core_rules_are_identical_for_every_dialogue_contour() {
        let registry = CapabilityRegistry::global();

        let repl = registry.compose_core_contract_rules(
            &CapabilitySet::interactive_repl(),
            &xml_context(TurnIntent::InteractiveRepl),
        );
        // A review discussion runs through the interactive intent, so its head is
        // the very same list by construction. The plan discussion is the contour
        // whose head must never diverge from it.
        let discussion = registry.compose_core_contract_rules(
            &CapabilitySet::discussion(),
            &xml_context(TurnIntent::PlanDiscussion),
        );

        assert_eq!(repl, discussion);
        assert!(repl.contains("Authoritative Source"));
        assert!(repl.contains("Minimal Coherent Change"));
        // The regime-specific plan actions belong to the delta, never to the head.
        assert!(!repl.contains("Local Engineering Plan Actions"));
    }

    #[test]
    fn delta_carries_only_what_the_declared_regime_adds() {
        let registry = CapabilityRegistry::global();

        let repl = registry.compose_delta_contract_rules(
            &CapabilitySet::interactive_repl(),
            &xml_context(TurnIntent::InteractiveRepl),
        );
        let discussion = registry.compose_delta_contract_rules(
            &CapabilitySet::discussion(),
            &xml_context(TurnIntent::PlanDiscussion),
        );
        let step = registry.compose_delta_contract_rules(
            &CapabilitySet::isolated_step(),
            &xml_context(TurnIntent::IsolatedStep),
        );

        // The plan-action rule is regime-specific in its body, so it renders
        // into the delta of every dialogue that may author plans: a develop
        // turn may only propose a new plan, while a plan discussion may also
        // modify or delete an existing one. Both contours therefore carry the
        // rule and each states only the actions its own regime authorizes.
        assert!(repl.contains("Local Engineering Plan Actions"));
        assert!(repl.contains("ONLY propose new plans"));
        assert!(!repl.contains("<plan action=\"update\" ...>"));
        assert!(discussion.contains("Local Engineering Plan Actions"));
        assert!(discussion.contains("<plan action=\"save\" ...>"));
        assert!(discussion.contains("<plan action=\"update\" ...>"));
        assert!(discussion.contains("<plan action=\"delete\" ...>"));
        // An isolated step cannot touch a plan, so its delta never carries the
        // plan-action semantics at all.
        assert!(!step.contains("Local Engineering Plan Actions"));

        // An isolated step inherits no dialogue-only rule.
        assert!(!step.contains("Action Authorization"));

        // A delta never repeats a rule of the head it follows.
        for delta in [&repl, &discussion, &step] {
            assert!(!delta.contains("Minimal Coherent Change"));
            assert!(!delta.contains("Context Surface"));
        }

        // Each pass numbers its own rules densely from one.
        for delta in [&repl, &discussion, &step] {
            let numbers = rule_numbers(delta);
            assert_eq!(
                numbers,
                (1..=numbers.len()).collect::<Vec<_>>(),
                "non-monotonic delta numbering: {delta}"
            );
        }
    }

    #[test]
    fn core_selection_follows_the_typed_profile_lists() {
        let registry = CapabilityRegistry::global();

        let dialogue = registry.compose_core_contract_rules(
            &CapabilitySet::interactive_repl(),
            &xml_context(TurnIntent::InteractiveRepl),
        );
        let step = registry.compose_core_contract_rules(
            &CapabilitySet::isolated_step(),
            &xml_context(TurnIntent::IsolatedStep),
        );

        // Plan authoring belongs to the headline surface of every dialogue, and it
        // disappears from an isolated step because the profile list omits it,
        // never because the rule text was filtered.
        assert!(dialogue.contains("Local Engineering Plans"));
        assert!(!step.contains("Local Engineering Plans"));

        for rendered in [&dialogue, &step] {
            let numbers = rule_numbers(rendered);
            assert!(!numbers.is_empty());
            assert_eq!(numbers, (1..=numbers.len()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn delta_instructions_follow_the_profile_delta() {
        let registry = CapabilityRegistry::global();
        let render = PromptRenderContext::new(&[], None);

        // A plan discussion declares only plan mutation in its delta, and that
        // capability renders no protocol instruction of its own, so the delta
        // instruction block stays empty while the mechanism remains in place for
        // a regime-specific capability that does render one.
        assert!(registry
            .compose_delta_xml_instructions(
                &CapabilitySet::discussion(),
                &render,
                TurnIntent::PlanDiscussion,
            )
            .is_empty());
        assert!(registry
            .compose_delta_structured_protocol_instructions(
                &CapabilitySet::discussion(),
                &render,
                TurnIntent::PlanDiscussion,
            )
            .is_empty());

        // The REPL and the review discussion carry an empty delta list, so they
        // inherit the head instructions only.
        let intent = TurnIntent::InteractiveRepl;
        let xml_delta = registry.compose_delta_xml_instructions(
            &CapabilitySet::interactive_repl(),
            &render,
            intent,
        );
        assert!(xml_delta.is_empty());
        let structured_delta = registry.compose_delta_structured_protocol_instructions(
            &CapabilitySet::interactive_repl(),
            &render,
            intent,
        );
        assert!(structured_delta.is_empty());
    }
}

/// Context handed to capability modules when they render prompt fragments.
///
/// Every field is a pure render input: the permitted editable paths, the
/// optional per-turn marker suffix, whether an eager symbol map is already
/// rendered, and the project command allowlist declared by `tauqe.toml`.
/// Rendering never performs I/O.
pub struct PromptRenderContext<'a> {
    /// Repository-relative paths the model may modify this turn.
    pub editable_paths: &'a [String],
    /// Active turn-marker suffix, when the protocol is marker-bound.
    pub marker: Option<&'a str>,
    /// Whether an eager repository symbol map is already rendered this turn.
    ///
    /// A module conditioned on this flag never advertises an on-demand map
    /// request for contours the prompt already carries.
    pub has_eager_repo_map: bool,
    /// The project command allowlist declared by `tauqe.toml`.
    ///
    /// A module that publishes project operations reads the declaration from
    /// here instead of loading configuration while rendering, so rendering
    /// stays a pure projection of typed state and an identical declaration
    /// always produces identical prompt bytes.
    pub project: Option<&'a ProjectConfig>,
    /// Strategy that decides how a change set addresses the lines it replaces.
    ///
    /// The strategy is orthogonal to the wire format: the transport decides how
    /// an edit is encoded, while the strategy decides whether the replaced lines
    /// are echoed back or named by the opaque anchors rendered in context. A
    /// module that publishes the change set reads it from here instead of
    /// re-deriving it, so an identical declaration renders identical bytes.
    pub patch_strategy: PatchStrategy,
}

impl<'a> PromptRenderContext<'a> {
    /// Creates a render context for the given editable paths and marker.
    ///
    /// The eager-map flag defaults to `false`; a caller that already rendered a
    /// symbol map declares it through [`PromptRenderContext::with_eager_repo_map`].
    pub fn new(editable_paths: &'a [String], marker: Option<&'a str>) -> Self {
        Self {
            editable_paths,
            marker,
            has_eager_repo_map: false,
            project: None,
            patch_strategy: PatchStrategy::SearchReplace,
        }
    }

    /// Declares whether an eager repository symbol map is already rendered.
    pub fn with_eager_repo_map(mut self, has_eager_repo_map: bool) -> Self {
        self.has_eager_repo_map = has_eager_repo_map;
        self
    }

    /// Declares the project command allowlist available to this turn.
    ///
    /// The declaration is borrowed from the loaded configuration, so a module
    /// that publishes project commands never re-reads `tauqe.toml` while
    /// rendering a prompt.
    pub fn with_project(mut self, project: Option<&'a ProjectConfig>) -> Self {
        self.project = project;
        self
    }

    /// Declares how a rendered change set addresses the lines it replaces.
    pub fn with_patch_strategy(mut self, patch_strategy: PatchStrategy) -> Self {
        self.patch_strategy = patch_strategy;
        self
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
    /// Wire format the turn serializes, declared by the active protocol.
    ///
    /// The format is a transport fact, never a capability decision: rules read
    /// it only to avoid advertising the other contour's spellings.
    pub protocol: EditProtocolKind,
    /// Layout policy that composes the message sequence of this turn.
    ///
    /// The policy decides how the context material reaches the model, so the
    /// rule that describes that surface renders it in the vocabulary of the
    /// active policy. A context that composes no transcript renders the
    /// semantic vocabulary, the layout of a caller that sends one block.
    pub prompt_layout: PromptLayoutKind,
    /// Concrete response language, when the caller already resolved it.
    pub target_language: Option<&'a str>,
    /// Whether an eager repository symbol map is already rendered this turn.
    ///
    /// Mirrors [`PromptRenderContext::has_eager_repo_map`]; contract rules read
    /// this field directly instead of reaching into the render context.
    pub has_eager_repo_map: bool,
}

impl<'a> ContractContext<'a> {
    /// Creates a contract context for the given intent, wire format, and render
    /// context.
    ///
    /// The format is an explicit argument because it is a transport fact the
    /// renderer cannot derive from the capability surface, and the eager-map
    /// flag is inherited from the render context, so a contract rule and a
    /// protocol instruction composed for the same turn observe one value.
    /// The layout starts at the semantic policy — the block order of a caller
    /// that composes no transcript — and a caller that renders a transcript
    /// declares it through [`ContractContext::with_prompt_layout`].
    pub fn new(
        intent: TurnIntent,
        protocol: EditProtocolKind,
        render: PromptRenderContext<'a>,
    ) -> Self {
        let has_eager_repo_map = render.has_eager_repo_map;
        Self {
            render,
            intent,
            protocol,
            prompt_layout: PromptLayoutKind::Semantic,
            target_language: None,
            has_eager_repo_map,
        }
    }

    /// Declares whether an eager repository symbol map is already rendered.
    ///
    /// The flag is written to both contexts so a contract rule and its matching
    /// protocol instruction can never disagree.
    pub fn with_eager_repo_map(mut self, has_eager_repo_map: bool) -> Self {
        self.has_eager_repo_map = has_eager_repo_map;
        self.render.has_eager_repo_map = has_eager_repo_map;
        self
    }

    /// Declares the layout policy that composes the message sequence of the turn.
    ///
    /// The value is typed rather than a policy name: the rule describing the
    /// context surface matches the variant and spells only that policy's own
    /// vocabulary, so the two dictionaries never mix.
    pub fn with_prompt_layout(mut self, prompt_layout: PromptLayoutKind) -> Self {
        self.prompt_layout = prompt_layout;
        self
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

    /// Base name of the primary protocol tag that names this capability.
    ///
    /// The canonical tag is what prompts, diagnostics, and tag scanners print
    /// for the capability, so the module stays the single source of truth for
    /// its own spelling. A capability that owns no protocol tag, such as the
    /// purely textual response, returns `None`.
    fn canonical_tag(&self) -> Option<&'static str> {
        None
    }

    /// Legacy base names accepted as an alias of [`Self::canonical_tag`].
    ///
    /// An alias decodes, streams, and renders exactly like the canonical tag it
    /// stands for, so renaming a tag keeps older transcripts and sessions
    /// reproducible. An alias never carries a spelling of its own: it resolves
    /// strictly through the canonical name of the module that declares it.
    fn tag_aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// Tag specs the module recognizes, or an empty slice for tag-less modules.
    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        &[]
    }

    /// Structured-output property this module populates, when it maps to one.
    fn json_property_name(&self) -> Option<&'static str> {
        None
    }

    /// JSON-schema properties this module contributes to the structured response.
    ///
    /// The fragment is an object keyed by the property names the capability
    /// owns, with the schema of each property as its value. The registry folds
    /// the fragments of the enabled modules into one strict schema, so a
    /// property is declared by the same module that renders its prompt
    /// instruction and decodes it. A capability without a structured field
    /// contributes no fragment, and a property declared by several modules is
    /// inserted once, by the first declaring module.
    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        None
    }

    /// JSON-schema properties this module contributes under a patch strategy.
    ///
    /// A capability whose structured surface depends on how a change addresses
    /// the lines it replaces overrides this method and describes the fields of
    /// the declared strategy. The default projects
    /// [`CapabilityModule::json_schema_fragment`], so a strategy-neutral
    /// capability declares its fragment exactly once.
    fn json_schema_fragment_for(
        &self,
        _patch_strategy: PatchStrategy,
    ) -> Option<serde_json::Value> {
        self.json_schema_fragment()
    }

    /// Shared JSON-schema definitions referenced by [`Self::json_schema_fragment`].
    ///
    /// A capability whose structured payload nests recursively — a plan is a
    /// tree of concept nodes and implementation steps — cannot inline that
    /// shape, because a nested object would have to contain itself. Such a
    /// module declares the shared definitions here and references them from its
    /// fragment with `#/$defs/<Name>`; the registry lifts them to the root of
    /// the composed schema, which is the reference form strict structured
    /// output accepts. A capability whose payload holds no recursion
    /// contributes no definition.
    fn json_schema_defs(&self) -> Option<serde_json::Value> {
        None
    }

    /// Rule contributed to the byte-stable head of the `Core System Contract`.
    ///
    /// The core rule must be intent-independent: for one capability set, one
    /// transport, and one set of render inputs it renders byte-identically for
    /// the interactive REPL, a plan discussion, and a review discussion, because
    /// those three contours share the head that the provider caches. Anything
    /// the declared regime authorizes is declared by
    /// [`CapabilityModule::render_delta_rule`] instead.
    fn render_contract_rule(&self, _ctx: &ContractContext<'_>) -> Option<ContractRule> {
        None
    }

    /// Rule contributed to the contour delta of the trailing user message.
    ///
    /// A delta rule is the sanctioned place to read
    /// [`ContractContext::intent`]: it describes what the declared regime
    /// authorizes rather than an invariant every dialogue states, so it renders
    /// outside the byte-stable head. The default is no delta, because most
    /// capabilities state the same rule in every regime that enables them.
    fn render_delta_rule(&self, _ctx: &ContractContext<'_>) -> Option<ContractRule> {
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
    &AbortModule,
    &UserLanguageModule,
    &CustomToolsModule,
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

    /// Returns the capability that declares `base`, canonical or aliased.
    ///
    /// Lookup follows the module declarations, so the registry carries no tag
    /// table of its own. When two modules share one spelling — plan creation and
    /// plan update both own the plan container — the first declaring module wins
    /// and both agree on the phase and the delivery mode of the shared tag.
    pub fn capability_for_tag(&self, base: &str) -> Option<LlmCapability> {
        self.modules.iter().copied().find_map(|module| {
            let declared = module.canonical_tag() == Some(base)
                || module.tag_aliases().contains(&base)
                || module
                    .xml_tag_specs()
                    .iter()
                    .any(|spec| spec.base_name == base);
            declared.then(|| module.capability())
        })
    }

    /// Resolves any accepted spelling of a tag to its canonical base name.
    ///
    /// A canonical spelling resolves to itself, a legacy alias resolves to the
    /// canonical tag of the module that still accepts it, and a secondary tag
    /// such as one discovery request resolves to its own name instead of
    /// collapsing into the primary spelling of its capability. An undeclared
    /// spelling resolves to nothing, so a scanner rejects it instead of
    /// guessing.
    pub fn canonical_tag(&self, base: &str) -> Option<&'static str> {
        for module in self.modules {
            if let Some(canonical) = module.canonical_tag() {
                if canonical == base || module.tag_aliases().contains(&base) {
                    return Some(canonical);
                }
            }
            if let Some(spec) = module
                .xml_tag_specs()
                .iter()
                .find(|spec| spec.base_name == base)
            {
                return Some(spec.base_name);
            }
        }
        None
    }

    /// Resolves the tag specification of any accepted spelling.
    ///
    /// The lookup passes through the canonical name, so an alias yields the
    /// spec of the tag it stands for rather than a spec of its own.
    pub fn tag_spec(&self, base: &str) -> Option<&'static XmlTagSpec> {
        engine::tag_spec(self.canonical_tag(base)?)
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
        let example = tags
            .first()
            .map(|tag| format!("<{} ...>", ctx.tag_name(tag)))
            .unwrap_or_default();
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

    /// Composes the strict JSON schema of the structured response.
    ///
    /// The properties are folded from the fragments of the enabled modules, so
    /// the wire schema is derived from the same declaration that renders the
    /// prompt instructions and scopes the decoder: a disabled capability
    /// contributes no property, and a property can never drift from the rule
    /// that describes it. `required` mirrors the emitted properties, because a
    /// strict schema must require exactly what it declares, and
    /// `additionalProperties` stays closed so an undeclared field is rejected
    /// instead of silently dropped. A payload that nests recursively ships its
    /// shared definitions through [`CapabilityModule::json_schema_defs`], and
    /// the registry lifts them to the root of the composed schema, where the
    /// `#/$defs/<Name>` references of that fragment resolve.
    pub fn compose_structured_schema(
        &self,
        set: &CapabilitySet,
        patch_strategy: PatchStrategy,
    ) -> serde_json::Value {
        let mut properties = serde_json::Map::new();
        let mut definitions = serde_json::Map::new();
        for module in self.enabled(set) {
            if let Some(fragment) = module.json_schema_fragment_for(patch_strategy) {
                if let Some(fields) = fragment.as_object() {
                    for (name, schema) in fields {
                        properties
                            .entry(name.clone())
                            .or_insert_with(|| schema.clone());
                    }
                }
            }
            if let Some(fragment) = module.json_schema_defs() {
                if let Some(entries) = fragment.as_object() {
                    for (name, schema) in entries {
                        definitions
                            .entry(name.clone())
                            .or_insert_with(|| schema.clone());
                    }
                }
            }
        }

        let required: Vec<serde_json::Value> = properties
            .keys()
            .map(|name| serde_json::Value::String(name.clone()))
            .collect();

        let mut schema = serde_json::json!({
            "type": "object",
            "properties": serde_json::Value::Object(properties),
            "required": required,
            "additionalProperties": false
        });
        if !definitions.is_empty() {
            schema["$defs"] = serde_json::Value::Object(definitions);
        }
        schema
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
    use tauqe_protocol::{CapabilityDelivery, PatchStrategy};

    /// Iterates every built-in module, in registry order.
    fn all_modules() -> impl Iterator<Item = &'static dyn CapabilityModule> {
        LlmCapability::ALL
            .iter()
            .filter_map(|capability| CapabilityRegistry::global().module(*capability))
    }

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
            EditProtocolKind::Xml,
            PromptRenderContext::new(&[], None),
        )
    }

    #[test]
    fn render_context_eager_repo_map_defaults_off_and_propagates() {
        let render = PromptRenderContext::new(&[], None);
        assert!(!render.has_eager_repo_map);

        let eager = render.with_eager_repo_map(true);
        assert!(eager.has_eager_repo_map);

        // The contract context inherits the flag from the render context, so a
        // contract rule and a protocol instruction never disagree.
        let ctx = ContractContext::new(TurnIntent::InteractiveRepl, EditProtocolKind::Xml, eager);
        assert!(ctx.has_eager_repo_map);
        assert!(ctx.render.has_eager_repo_map);

        // The builder keeps both contexts in sync on later changes.
        let reset = ctx.with_eager_repo_map(false);
        assert!(!reset.has_eager_repo_map);
        assert!(!reset.render.has_eager_repo_map);

        let declared = plain_context().with_eager_repo_map(true);
        assert!(declared.has_eager_repo_map);
        assert!(declared.render.has_eager_repo_map);
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
        let develop = registry.operational_xml_tags(&CapabilitySet::interactive_repl());
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
                "abort".to_string(),
                "user_language".to_string(),
                "custom_tool".to_string(),
            ]
        );
        assert_eq!(
            registry.operational_xml_tags(&CapabilitySet::interactive_repl()),
            develop
        );

        let isolated = registry.operational_xml_tags(&CapabilitySet::isolated_step());
        assert!(isolated.iter().any(|tag| tag == "context_request"));
        assert!(!isolated.iter().any(|tag| tag == "plan"));
    }

    #[test]
    fn contract_rules_reflect_the_capability_set() {
        let registry = CapabilityRegistry::global();
        let ctx = plain_context();

        let develop =
            registry.compose_core_contract_rules(&CapabilitySet::interactive_repl(), &ctx);
        assert!(develop.contains("context_request"));
        assert!(develop.contains("Minimal Coherent Change"));
        assert!(develop.contains("1. Authoritative Source: "));
        assert!(develop.contains("Operational Protocol Tags: "));
        // The plan actions a regime authorizes are regime-specific, so they
        // travel in the delta instead of conditioning the shared head.
        assert!(!develop.contains("Local Engineering Plan Actions"));

        let develop_delta =
            registry.compose_delta_contract_rules(&CapabilitySet::interactive_repl(), &ctx);
        assert!(develop_delta.contains("Local Engineering Plan Actions"));
        assert!(!develop_delta.contains("Minimal Coherent Change"));

        // A plan discussion carries the very same head: the plan mutation it
        // authorizes is the only difference, and it travels in the delta.
        let discussion = registry.compose_core_contract_rules(&CapabilitySet::discussion(), &ctx);
        assert_eq!(discussion, develop);
        assert!(discussion.contains("Minimal Coherent Change"));
        assert!(discussion.contains("Local Engineering Plans"));
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
        // File-edit tags are not control tags, so the operational rule is
        // skipped instead of advertising an empty protocol surface. The
        // context-surface rule describes payload rather than a command, so it
        // stays and keeps the numbering dense.
        let core = registry.compose_core_contract_rules(&narrowed, &plain_context());
        assert!(!core.contains("Operational Protocol Tags"));
        assert_eq!(rule_numbers(&core), vec![1, 2, 3, 4, 5, 6]);
        assert!(core.contains("1. Authoritative Source: "));
        assert!(core.contains("2. Context Surface: "));
        assert!(core.contains("3. No Arbitrary Shell: "));
        assert!(core.contains("4. Reasoning in English: "));
        assert!(core.contains("5. Conversational Response: "));
        assert!(core.contains("6. Minimal Coherent Change: "));

        // The intent-specific rules own their numbering, so the delta numbers
        // densely from one independently of the head it follows.
        let delta = registry.compose_delta_contract_rules(&narrowed, &plain_context());
        assert_eq!(rule_numbers(&delta), vec![1, 2, 3]);
        assert!(delta.contains("1. Read-Only Scope: "));
        assert!(delta.contains("2. Editable Scope: "));
        assert!(delta.contains("3. Action Authorization: "));
        assert!(!delta.contains("Plan-First Proposal"));
    }

    #[test]
    fn context_surface_rule_follows_the_declared_layout() {
        let registry = CapabilityRegistry::global();
        let set = CapabilitySet::interactive_repl();

        let semantic = registry.compose_core_contract_rules(&set, &plain_context());
        assert!(semantic.contains("2. Context Surface: "));
        assert!(semantic.contains("one `<context>` block"));
        assert!(!semantic.contains("<context_manifest>"));

        let chronological = plain_context().with_prompt_layout(PromptLayoutKind::Chronological);
        let rendered = registry.compose_core_contract_rules(&set, &chronological);
        assert!(rendered.contains("2. Context Surface: "));
        assert!(rendered.contains("<context_manifest>"));
        assert!(!rendered.contains("one `<context>` block"));

        // The `<file>` element and its `access` attribute are prompt-side
        // payload both policies describe identically, so the shared sentence
        // survives in either vocabulary.
        let shared = "`<file path=\"...\" access=\"...\">`";
        assert!(semantic.contains(shared));
        assert!(rendered.contains(shared));
    }

    #[test]
    fn contract_rules_stay_dense_in_every_layout() {
        let registry = CapabilityRegistry::global();
        for layout in [PromptLayoutKind::Semantic, PromptLayoutKind::Chronological] {
            let ctx = plain_context().with_prompt_layout(layout);
            let rendered =
                registry.compose_core_contract_rules(&CapabilitySet::interactive_repl(), &ctx);
            let numbers = rule_numbers(&rendered);
            assert!(!numbers.is_empty());
            assert_eq!(
                numbers,
                (1..=numbers.len()).collect::<Vec<_>>(),
                "non-monotonic numbering for {layout:?}"
            );
        }
    }

    #[test]
    fn develop_contract_has_no_lettered_rule_numbers() {
        let registry = CapabilityRegistry::global();
        let set = CapabilitySet::interactive_repl();
        let core = registry.compose_core_contract_rules(&set, &plain_context());
        let delta = registry.compose_delta_contract_rules(&set, &plain_context());

        for rendered in [&core, &delta] {
            for surrogate in ["1a.", "2a.", "4a.", "8a.", "10a.", "11a."] {
                assert!(!rendered.contains(surrogate), "surrogate {surrogate}");
            }
            let numbers = rule_numbers(rendered);
            assert!(!numbers.is_empty());
            assert_eq!(numbers, (1..=numbers.len()).collect::<Vec<_>>());
        }

        // The plan invariants belong to the shared head, while the planning
        // objective a regime pursues travels in its delta.
        assert!(core.contains("Conceptual Foundation Invariant"));
        assert!(core.contains("Leaf-Only Files Invariant"));
        assert!(core.contains("File Attributes Contract"));
        assert!(core.contains("Structured Plan XML Format:"));
        assert!(core.contains("Step Formulation Contract: "));
        assert!(!core.contains("Planning Objective: "));
        assert!(delta.contains("Planning Objective: "));
        assert!(!delta.contains("Step Formulation Contract: "));
    }

    #[test]
    fn contract_rules_follow_typed_inputs() {
        let registry = CapabilityRegistry::global();
        let set = CapabilitySet::interactive_repl();

        let rendered = registry.compose_core_contract_rules(&set, &plain_context());
        assert!(rendered.contains("Language Consistency: Always formulate"));

        let localized = plain_context().with_target_language(Some("Russian"));
        let rendered = registry.compose_core_contract_rules(&set, &localized);
        assert!(rendered.contains("Language Consistency: The user's language is Russian."));

        let discussion = ContractContext::new(
            TurnIntent::PlanDiscussion,
            EditProtocolKind::Structured,
            PromptRenderContext::new(&[], None),
        );
        let rendered =
            registry.compose_core_contract_rules(&CapabilitySet::discussion(), &discussion);
        assert!(rendered.contains("Language Consistency: Always formulate"));
        assert!(rendered.contains("`user_language` field"));
    }

    #[test]
    fn operational_tag_rule_stays_within_the_enabled_set() {
        let registry = CapabilityRegistry::global();
        let narrowed =
            CapabilitySet::from_slice(&[LlmCapability::ToolchainVerify, LlmCapability::Outcome]);
        let rendered = registry.compose_core_contract_rules(&narrowed, &plain_context());

        assert!(rendered.contains("Operational Protocol Tags"));
        assert!(rendered.contains("`<verify ...>`"));
        assert!(!rendered.contains("context_request"));
    }

    #[test]
    fn operational_tag_rule_is_rendered_only_for_the_xml_contour() {
        let registry = CapabilityRegistry::global();
        let set = CapabilitySet::interactive_repl();

        // The XML transport decodes control tags, so its contract enumerates
        // them.
        let xml = registry.compose_core_contract_rules(&set, &plain_context());
        assert!(xml.contains("Operational Protocol Tags"));
        assert!(xml.contains("All operational protocol tags"));

        // Structured Output decodes JSON fields, so the rule enumerating
        // control tags is never rendered.
        let structured_ctx = ContractContext::new(
            TurnIntent::InteractiveRepl,
            EditProtocolKind::Structured,
            PromptRenderContext::new(&[], None),
        );
        let structured = registry.compose_core_contract_rules(&set, &structured_ctx);
        assert!(!structured.contains("Operational Protocol Tags"));
        assert!(!structured.contains("All operational protocol tags"));

        // The omitted rule leaves no gap: the remaining rules stay dense.
        let numbers = rule_numbers(&structured);
        assert!(!numbers.is_empty());
        assert_eq!(numbers, (1..=numbers.len()).collect::<Vec<_>>());
    }

    #[test]
    fn contract_rules_apply_the_turn_marker() {
        let registry = CapabilityRegistry::global();
        let ctx = ContractContext::new(
            TurnIntent::InteractiveRepl,
            EditProtocolKind::Xml,
            PromptRenderContext::new(&[], Some("M12345")),
        );
        let rendered =
            registry.compose_core_contract_rules(&CapabilitySet::interactive_repl(), &ctx);
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
            registry.compose_xml_protocol_instructions(&CapabilitySet::interactive_repl(), &ctx);
        assert!(marked.contains(&format!("edits_{marker}")));
        assert!(marked.contains(&format!("context_request_{marker}")));

        let plain_ctx = PromptRenderContext::new(&[], None);
        let plain = registry
            .compose_xml_protocol_instructions(&CapabilitySet::interactive_repl(), &plain_ctx);
        assert!(plain.contains("<edits summary"));
    }

    #[test]
    fn structured_instructions_reflect_the_capability_set() {
        let registry = CapabilityRegistry::global();
        let ctx = PromptRenderContext::new(&[], None);

        let json = registry
            .compose_structured_protocol_instructions(&CapabilitySet::interactive_repl(), &ctx);
        assert!(json.contains("changes"));
        assert!(json.contains("message"));
        assert!(json.contains("`doc_requests`"));
        assert!(json.contains("`scratchpad`"));
        assert!(json.contains("`outcome`"));
        assert!(json.contains("`blocked`"));
        assert!(json.contains("`verify`"));

        let discussion =
            registry.compose_structured_protocol_instructions(&CapabilitySet::discussion(), &ctx);
        // A plan discussion advertises the whole dialogue surface, so its
        // instructions name every field that surface permits.
        for field in [
            "`changes`",
            "`doc_requests`",
            "`scratchpad`",
            "`outcome`",
            "`blocked`",
            "`verify`",
        ] {
            assert!(discussion.contains(field), "missing {field}");
        }

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

    /// Collects the property names of a composed structured schema.
    fn schema_properties(schema: &serde_json::Value) -> Vec<String> {
        schema["properties"]
            .as_object()
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Collects the `required` entries of a composed structured schema.
    fn schema_required(schema: &serde_json::Value) -> Vec<String> {
        schema["required"]
            .as_array()
            .map(|required| {
                required
                    .iter()
                    .filter_map(|field| field.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn structured_schema_is_composed_from_the_enabled_modules() {
        let registry = CapabilityRegistry::global();
        let schema = registry.compose_structured_schema(
            &CapabilitySet::interactive_repl(),
            PatchStrategy::SearchReplace,
        );

        assert_eq!(schema["type"], serde_json::json!("object"));
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));

        let mut properties = schema_properties(&schema);
        properties.sort();
        let mut expected: Vec<String> = [
            "abort",
            "blocked",
            "changes",
            "context_requests",
            "custom_tools",
            "dir_lists",
            "doc_requests",
            "message",
            "outcome",
            "plan_update",
            "repo_dirs",
            "repo_maps",
            "repo_searches",
            "scratchpad",
            "user_language",
            "verify",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        expected.sort();
        assert_eq!(properties, expected);

        // A strict schema requires exactly the properties it declares, so a
        // pruned property can never stay required.
        assert_eq!(schema_required(&schema).len(), properties.len());

        // `suggested_actions` belongs to no capability, so the registry never
        // advertises it.
        assert!(schema["properties"].get("suggested_actions").is_none());
    }

    #[test]
    fn structured_schema_lifts_the_shared_definitions_of_enabled_modules() {
        let registry = CapabilityRegistry::global();
        let with_plans = CapabilitySet::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::FileEdits,
            LlmCapability::PlanCreation,
        ]);
        let schema = registry.compose_structured_schema(&with_plans, PatchStrategy::SearchReplace);

        assert!(schema_properties(&schema).contains(&"plan_update".to_string()));
        let definitions = schema["$defs"]
            .as_object()
            .expect("the plan payload references shared definitions");
        assert!(definitions.contains_key("ConceptItem"));
        assert!(definitions.contains_key("PlanItem"));

        // A surface that cannot author plans carries no nested payload, so its
        // composed schema keeps the flat shape.
        let plain = registry.compose_structured_schema(
            &CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::FileEdits]),
            PatchStrategy::SearchReplace,
        );
        assert!(plain.get("$defs").is_none());

        // A plan update declares the very same property and definitions, so
        // both plan capabilities describe one payload.
        let updating = registry.compose_structured_schema(
            &CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::PlanUpdate]),
            PatchStrategy::SearchReplace,
        );
        assert!(schema_properties(&updating).contains(&"plan_update".to_string()));
        assert!(updating["$defs"].is_object());
    }

    #[test]
    fn structured_schema_prunes_disabled_capabilities_and_their_required_entries() {
        let registry = CapabilityRegistry::global();
        let narrowed =
            CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::FileEdits]);
        let schema = registry.compose_structured_schema(&narrowed, PatchStrategy::SearchReplace);

        let mut properties = schema_properties(&schema);
        properties.sort();
        assert_eq!(
            properties,
            vec!["changes".to_string(), "message".to_string()]
        );

        let mut required = schema_required(&schema);
        required.sort();
        assert_eq!(required, properties);

        // The schema stays strict: no property admits an explicit null.
        assert!(!schema.to_string().contains("\"null\""));
    }

    #[test]
    fn json_schema_fragments_are_declared_by_the_modules_that_own_the_property() {
        let registry = CapabilityRegistry::global();
        for capability in LlmCapability::ALL {
            let module = match registry.module(*capability) {
                Some(module) => module,
                None => panic!("missing module for {}", capability),
            };
            let Some(fragment) = module.json_schema_fragment_for(PatchStrategy::SearchReplace)
            else {
                continue;
            };
            let fields = fragment
                .as_object()
                .unwrap_or_else(|| panic!("{} returns an object fragment", capability));
            assert!(
                !fields.is_empty(),
                "{} declares at least one property",
                capability
            );
            if let Some(property) = module.json_property_name() {
                assert!(
                    fields.contains_key(property),
                    "{} declares its own property '{property}'",
                    capability
                );
            }
        }
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
            registry.decode_xml_commands(&CapabilitySet::interactive_repl(), raw, Some("AB"), &[]);
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
            registry.decode_xml_commands(&CapabilitySet::isolated_step(), raw, Some("AB"), &[]);
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
        let cleaned = registry.clean_text(&CapabilitySet::interactive_repl(), raw, None);
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

    /// Builds a `[project]` declaration carrying one custom command per name.
    fn project_with_commands(names: &[&str]) -> ProjectConfig {
        let mut project = ProjectConfig::default();
        for name in names {
            project.commands.insert(
                (*name).to_string(),
                tauqe_protocol::ProjectCommandSpec {
                    command: "cargo".to_string(),
                    args: vec![(*name).to_string()],
                    ..Default::default()
                },
            );
        }
        project
    }

    /// Renders the contract for a set that exposes the custom tool surface.
    fn custom_tools_contract(project: Option<&ProjectConfig>) -> String {
        let render = PromptRenderContext::new(&[], None).with_project(project);
        let ctx = ContractContext::new(TurnIntent::InteractiveRepl, EditProtocolKind::Xml, render);
        let set =
            CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::CustomTools]);
        CapabilityRegistry::global().compose_core_contract_rules(&set, &ctx)
    }

    #[test]
    fn custom_tools_module_publishes_the_declared_allowlist() {
        let registry = CapabilityRegistry::global();
        assert!(
            registry.module(LlmCapability::CustomTools).is_some(),
            "every capability needs a module, including the custom tool surface"
        );

        let project = project_with_commands(&["docs", "audit"]);
        let rendered = custom_tools_contract(Some(&project));

        assert!(rendered.contains("Project Custom Tools (No Arbitrary Shell)"));
        assert!(rendered.contains("Runs `cargo docs`"));
        assert!(rendered.contains("Runs `cargo audit`"));
        assert!(rendered.contains("no shell to fall back on"));
        assert!(rendered.contains("A declared parameter may be supplied by name"));

        // The allowlist renders in declaration order, so identical state
        // produces identical prompt bytes.
        let audit = rendered.find("`audit`").expect("audit listed");
        let docs = rendered.find("`docs`").expect("docs listed");
        assert!(audit < docs);

        // The rule never advertises a control tag the decoder would reject.
        assert!(!rendered.contains("<blocked"));
    }

    #[test]
    fn custom_tools_module_stays_silent_without_declarations() {
        // An empty allowlist and an absent declaration both contribute no rule,
        // because there is no tool surface to describe.
        assert!(!custom_tools_contract(Some(&project_with_commands(&[])))
            .contains("Project Custom Tools"));
        let rendered = custom_tools_contract(None);
        assert!(!rendered.contains("Project Custom Tools"));
        assert!(!rendered.contains("`custom_tools`"));
    }

    #[test]
    fn custom_tools_structured_instruction_follows_the_declaration() {
        let registry = CapabilityRegistry::global();
        let set =
            CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::CustomTools]);
        let project = project_with_commands(&["docs"]);

        let declared = registry.compose_structured_protocol_instructions(
            &set,
            &PromptRenderContext::new(&[], None).with_project(Some(&project)),
        );
        assert!(declared.contains("`custom_tools`"));
        assert!(declared.contains("`docs`"));

        let bare = registry
            .compose_structured_protocol_instructions(&set, &PromptRenderContext::new(&[], None));
        assert!(!bare.contains("`custom_tools`"));
    }

    #[test]
    fn custom_tools_module_renders_xml_instruction_and_decodes_invocations() {
        let registry = CapabilityRegistry::global();
        let set =
            CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::CustomTools]);
        let project = project_with_commands(&["docs"]);
        let ctx = PromptRenderContext::new(&[], None).with_project(Some(&project));

        let instruction = registry.compose_xml_protocol_instructions(&set, &ctx);
        assert!(instruction.contains("<custom_tool name"));
        assert!(instruction.contains("`docs`"));
        assert!(instruction.contains("Runs `cargo docs`"));

        // Without a declaration the module advertises no invocation and no
        // command line for the model to imitate.
        let bare =
            registry.compose_xml_protocol_instructions(&set, &PromptRenderContext::new(&[], None));
        assert!(!bare.contains("<custom_tool"));

        // A declared invocation decodes into a typed command with no arguments,
        // because the declaration already names every literal argument.
        let commands =
            registry.decode_xml_commands(&set, "Run it.\n<custom_tool name=\"docs\" />", None, &[]);
        assert!(commands.contains(&LlmCommand::CustomTool {
            name: "docs".to_string(),
            args: serde_json::Value::Null,
        }));

        // A marker-bound tag decodes exactly like an unbound one, so the tool
        // invocation participates in the turn-marker protection.
        let marked = registry.decode_xml_commands(
            &set,
            "<custom_tool_AB12 name=\"docs\" />",
            Some("AB12"),
            &[],
        );
        assert!(marked.contains(&LlmCommand::CustomTool {
            name: "docs".to_string(),
            args: serde_json::Value::Null,
        }));
    }

    #[test]
    fn abort_module_is_registered_and_decodes_invocations() {
        let registry = CapabilityRegistry::global();
        let module = match registry.module(LlmCapability::Abort) {
            Some(module) => module,
            None => panic!("the abort capability needs a module"),
        };
        assert_eq!(module.canonical_tag(), Some("abort"));
        assert_eq!(
            registry.capability_for_tag("abort"),
            Some(LlmCapability::Abort)
        );
        assert_eq!(registry.canonical_tag("abort"), Some("abort"));
        let spec = match registry.tag_spec("abort") {
            Some(spec) => spec,
            None => panic!("the abort tag is declared"),
        };
        // A paired form is declared so the recommendation may carry a longer
        // instruction, and the tag stays a control tag so it never leaks into
        // conversational text.
        assert_eq!(spec.close_mode, engine::TagCloseMode::Control);
        assert!(spec.is_control());

        let set = CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::Abort]);
        let raw = "The native build needs approval.\n<abort reason=\"Native build approval required\" recommendation=\"pnpm approve-builds\" />";
        let commands = registry.decode_xml_commands(&set, raw, None, &[]);
        assert!(commands.contains(&LlmCommand::Abort {
            reason: "Native build approval required".to_string(),
            recommendation: "pnpm approve-builds".to_string(),
        }));

        // The body of a paired tag carries the instruction, and the turn-marker
        // suffix protects the invocation like every other control tag.
        let paired = "<abort_AB12 reason=\"Toolchain missing\">Install the SDK, then rerun the step.</abort_AB12>";
        let commands = registry.decode_xml_commands(&set, paired, Some("AB12"), &[]);
        assert!(commands.contains(&LlmCommand::Abort {
            reason: "Toolchain missing".to_string(),
            recommendation: "Install the SDK, then rerun the step.".to_string(),
        }));

        // Decoding is scoped to the enabled capability set, so a profile
        // without the abort surface never stops a turn on one.
        let narrowed = CapabilitySet::from_slice(&[LlmCapability::TextMessage]);
        assert!(!registry
            .decode_xml_commands(&narrowed, raw, None, &[])
            .iter()
            .any(|command| matches!(command, LlmCommand::Abort { .. })));
    }

    #[test]
    fn canonical_tags_lead_the_specs_of_their_own_module() {
        for module in all_modules() {
            let Some(canonical) = module.canonical_tag() else {
                assert!(
                    module.xml_tag_specs().is_empty(),
                    "{} declares protocol tags but no canonical tag",
                    module.capability()
                );
                continue;
            };
            assert_eq!(
                module.xml_tag_specs().first().map(|spec| spec.base_name),
                Some(canonical),
                "{} lists its canonical tag before every other tag",
                module.capability()
            );
        }
    }

    #[test]
    fn aliases_resolve_to_the_canonical_tag_of_their_module() {
        let registry = CapabilityRegistry::global();
        for module in all_modules() {
            for alias in module.tag_aliases() {
                let canonical = match module.canonical_tag() {
                    Some(canonical) => canonical,
                    None => panic!(
                        "{} declares alias {alias} without a canonical tag",
                        module.capability()
                    ),
                };
                assert_ne!(
                    *alias,
                    canonical,
                    "an alias never repeats the canonical name of {}",
                    module.capability()
                );
                assert_eq!(registry.canonical_tag(alias), Some(canonical));
                assert_eq!(
                    registry.capability_for_tag(alias),
                    Some(module.capability())
                );
            }
        }
    }

    #[test]
    fn canonical_lookup_covers_canonical_secondary_and_unknown_spellings() {
        let registry = CapabilityRegistry::global();

        // A canonical spelling resolves to itself.
        assert_eq!(registry.canonical_tag("edits"), Some("edits"));
        // The pre-canonical edit container resolves through its canonical
        // spelling instead of carrying one of its own.
        assert_eq!(registry.canonical_tag("tauqe_edits"), Some("edits"));
        assert_eq!(
            registry.canonical_tag("context_request"),
            Some("context_request")
        );
        assert_eq!(registry.canonical_tag("success"), Some("success"));
        // A secondary tag of a multi-tag capability resolves to its own name
        // instead of collapsing into the primary spelling.
        assert_eq!(registry.canonical_tag("repo_search"), Some("repo_search"));
        assert_eq!(registry.canonical_tag("edit"), Some("edit"));
        assert_eq!(registry.canonical_tag("error"), Some("error"));
        // An undeclared spelling resolves to nothing, so a scanner rejects it
        // instead of guessing.
        assert_eq!(registry.canonical_tag("repo"), None);
        assert_eq!(registry.canonical_tag(""), None);

        assert_eq!(
            registry
                .tag_spec("context_request")
                .map(|spec| spec.base_name),
            Some("context_request")
        );
        assert_eq!(
            registry.tag_spec("repo_map").map(|spec| spec.close_mode),
            Some(engine::TagCloseMode::Simple)
        );
        assert_eq!(
            registry.tag_spec("edits").map(|spec| spec.close_mode),
            Some(engine::TagCloseMode::Container)
        );
        assert_eq!(
            registry.tag_spec("plan").map(|spec| spec.close_mode),
            Some(engine::TagCloseMode::Container)
        );
        assert!(registry.tag_spec("repo").is_none());
    }

    #[test]
    fn capability_lookup_reports_the_declaring_module() {
        let registry = CapabilityRegistry::global();

        assert_eq!(
            registry.capability_for_tag("context_request"),
            Some(LlmCapability::Discovery)
        );
        assert_eq!(
            registry.capability_for_tag("repo_dirs"),
            Some(LlmCapability::Discovery)
        );
        assert_eq!(
            registry.capability_for_tag("edits"),
            Some(LlmCapability::FileEdits)
        );
        // The legacy spelling resolves to the module that still accepts it.
        assert_eq!(
            registry.capability_for_tag("tauqe_edits"),
            Some(LlmCapability::FileEdits)
        );
        assert_eq!(
            registry.capability_for_tag("success"),
            Some(LlmCapability::Outcome)
        );
        assert_eq!(
            registry.capability_for_tag("custom_tool"),
            Some(LlmCapability::CustomTools)
        );
        // Plan creation and plan update share one container spelling; creation
        // is declared first and both agree on its phase and delivery.
        assert_eq!(
            registry.capability_for_tag("plan"),
            Some(LlmCapability::PlanCreation)
        );
        assert_eq!(registry.capability_for_tag("repo"), None);
        assert_eq!(registry.capability_for_tag(""), None);
    }

    #[test]
    fn tag_lookup_exposes_the_declared_delivery_mode() {
        let registry = CapabilityRegistry::global();
        let delivery = |base: &str| {
            registry
                .capability_for_tag(base)
                .map(|capability| capability.delivery())
        };

        assert_eq!(delivery("edits"), Some(CapabilityDelivery::LiveStreaming));
        assert_eq!(
            delivery("tauqe_edits"),
            Some(CapabilityDelivery::LiveStreaming)
        );
        assert_eq!(
            delivery("context_request"),
            Some(CapabilityDelivery::RoundDeferred)
        );
        assert_eq!(
            delivery("scratchpad"),
            Some(CapabilityDelivery::RoundDeferred)
        );
        assert_eq!(delivery("repo"), None);
    }
    #[test]
    fn contract_rules_carry_the_module_delta_hooks() {
        let registry = CapabilityRegistry::global();

        let set = CapabilitySet::interactive_repl();
        let core = registry.compose_core_contract_rules(&set, &plain_context());
        assert!(core.contains("Local Engineering Plans"));
        assert!(!core.contains("Local Engineering Plan Actions"));

        let delta = registry.compose_delta_contract_rules(&set, &plain_context());
        assert!(delta.contains("Local Engineering Plan Actions"));
        assert!(delta.contains("ONLY propose new plans"));

        // A plan discussion reaches the same core rules and swaps only the delta
        // hook, so the actions it authorizes never leak into the head.
        let discussion_ctx = ContractContext::new(
            TurnIntent::PlanDiscussion,
            EditProtocolKind::Xml,
            PromptRenderContext::new(&[], None),
        );
        let discussion_core =
            registry.compose_core_contract_rules(&CapabilitySet::discussion(), &discussion_ctx);
        assert_eq!(discussion_core, core);

        let discussion =
            registry.compose_delta_contract_rules(&CapabilitySet::discussion(), &discussion_ctx);
        assert!(discussion.contains("Local Engineering Plan Actions"));
        assert!(discussion.contains("ordinary transactional path"));
        assert!(!discussion.contains("ONLY propose new plans"));

        // The delta numbers its own list densely.
        let numbers = rule_numbers(&discussion);
        assert_eq!(numbers, (1..=numbers.len()).collect::<Vec<_>>());
    }
}
