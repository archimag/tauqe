//! Typed composition of the turn-regime trunk of the `Core System Contract`.
//!
//! Every trunk rule is selected by typed inputs: the declared turn intent and
//! the enabled capability surface. No rule is chosen by inspecting rendered
//! text, and no rule carries a hand-written number — the numbering is assigned
//! once, by position, when the registry renders the whole contract.
//!
//! The trunk lives here rather than inside prompt assembly so the regime has a
//! single source of truth: prompt rendering, the structured response schema,
//! and command decoding all observe the same typed surface.

use tauqe_protocol::{CapabilitySet, LlmCapability};

use super::registry::{ContractContext, ContractRule};

/// Returns the ordered trunk rules of the `Core System Contract` section.
///
/// The sequence always opens with the authoritative-source rule and continues
/// with the rules selected by the declared intent. The registry inserts the
/// operational-tag rule right after the opening rule and appends every enabled
/// capability module, so the final numbering is dense `1..N`.
pub fn regime_contract_rules(
    ctx: &ContractContext<'_>,
    capabilities: &CapabilitySet,
) -> Vec<ContractRule> {
    let mut rules = vec![ContractRule::new(
        "Authoritative Source",
        "The files provided in <context> represent the authoritative current state of the project. Do not invent missing code.",
    )];

    if ctx.intent.is_readonly_discussion() {
        rules.push(ContractRule::new(
            "Discussion Mode Scope",
            "You are strictly in Discussion mode. Code modifications, file creations, file edits, and file deletions are strictly prohibited. Do NOT output XML edit blocks (<edit>, <create>, <replace>, <overwrite>, <delete>) or any code patch proposals.",
        ));
        rules.push(ContractRule::new(
            "Read-Only Context Exploration",
            "every file inside <context> is rendered strictly with `access=\"read_only\"` and is meant for reference and architectural understanding only. If you need to inspect additional files from the repository map to answer the developer's questions, specify them in the `context_requests` array with `access: \"read_only\"`. To search across repository files, specify queries in `repo_searches`.",
        ));
    } else {
        rules.push(ContractRule::new(
            "Read-Only Scope",
            "files inside <context> carry a declarative `access` attribute. Files marked `access=\"read_only\"` are provided for reference, context, and structural comprehension; prefer modifying files marked `access=\"editable\"`. If completing the task requires editing a read-only file, you may propose edits to it directly without an extra discovery round — the environment will automatically promote it to editable upon applying the changes.",
        ));
        rules.push(ContractRule::new(
            "Editable Scope",
            format!(
                "files inside <context> marked `access=\"editable\"` are permitted for modification. You may also create new files using <{create} path=\"...\"> or move/rename files using <{moved} from=\"...\" to=\"...\" /> when required by the task.",
                create = ctx.tag_name("create"),
                moved = ctx.tag_name("move"),
            ),
        ));
    }

    rules.push(ContractRule::new(
        "No Arbitrary Shell",
        "You do not have shell execution capabilities. Work strictly through the context and actions provided.",
    ));

    if ctx.intent.is_interactive_repl() && capabilities.contains(LlmCapability::FileEdits) {
        rules.push(ContractRule::new(
            "Action Authorization",
            "When the required change is clear, specific, and unambiguous (e.g. direct instructions, straightforward bug fixes, or obvious adjustments), proceed directly with minimal coherent edits. Propose, analyze, and seek confirmation first ONLY when there is architectural ambiguity, competing trade-offs, multiple viable designs, or when the user asks an open exploratory question without a clear direction.",
        ));
    }

    rules.push(ContractRule::new(
        "Reasoning in English",
        "Always conduct internal reasoning, planning, and thinking strictly in English to preserve token budget and maximize reasoning quality.",
    ));

    if ctx.intent.is_isolated_step() {
        rules.push(ContractRule::new(
            "Code Verification",
            "Verification on code modifications is handled automatically and deterministically by the harness at the end of each step. Do NOT emit verification tags (the harness runs toolchain verification and gating automatically).",
        ));
        rules.push(ContractRule::new(
            "Step Execution Scope",
            "You are executing a single isolated plan step. Plan creation, modification, status updates, or plan tags are strictly prohibited (step lifecycle and plan tracking are handled deterministically by the harness).",
        ));
    }

    rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::PromptRenderContext;
    use tauqe_protocol::TurnIntent;

    /// Builds the trunk rules for one typed regime.
    fn trunk(intent: TurnIntent, set: &CapabilitySet, marker: Option<&str>) -> Vec<ContractRule> {
        let render = PromptRenderContext::new(&[], marker);
        let ctx = ContractContext::new(intent, render);
        regime_contract_rules(&ctx, set)
    }

    /// Collects the titles of the given rules.
    fn titles(rules: &[ContractRule]) -> Vec<&str> {
        rules.iter().map(|rule| rule.title.as_str()).collect()
    }

    /// Returns the body of the rule carrying the given title.
    fn body<'a>(rules: &'a [ContractRule], title: &str) -> &'a str {
        rules
            .iter()
            .find(|rule| rule.title == title)
            .map(|rule| rule.body.as_str())
            .unwrap_or_default()
    }

    #[test]
    fn trunk_opens_with_the_authoritative_source() {
        let rules = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::develop_xml(),
            None,
        );
        assert_eq!(
            rules.first().map(|rule| rule.title.as_str()),
            Some("Authoritative Source")
        );
        let names = titles(&rules);
        assert!(names.contains(&"Read-Only Scope"));
        assert!(names.contains(&"Editable Scope"));
        assert!(names.contains(&"No Arbitrary Shell"));
        assert!(names.contains(&"Reasoning in English"));
        assert!(names.contains(&"Action Authorization"));
    }

    #[test]
    fn discussion_trunk_is_read_only() {
        let rules = trunk(
            TurnIntent::ReadonlyDiscussion,
            &CapabilitySet::discussion(),
            None,
        );
        let names = titles(&rules);
        assert!(names.contains(&"Discussion Mode Scope"));
        assert!(names.contains(&"Read-Only Context Exploration"));
        assert!(!names.contains(&"Read-Only Scope"));
        assert!(!names.contains(&"Editable Scope"));
        assert!(!names.contains(&"Action Authorization"));
    }

    #[test]
    fn isolated_step_trunk_delegates_verification_and_scopes_the_step() {
        let rules = trunk(
            TurnIntent::IsolatedStep,
            &CapabilitySet::isolated_step_xml(),
            None,
        );
        let names = titles(&rules);
        assert!(names.contains(&"Code Verification"));
        assert!(names.contains(&"Step Execution Scope"));
        assert!(!names.contains(&"Action Authorization"));
        assert_eq!(names.last(), Some(&"Step Execution Scope"));
    }

    #[test]
    fn action_authorization_requires_the_file_edit_capability() {
        let with_edits = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::develop_xml(),
            None,
        );
        assert!(titles(&with_edits).contains(&"Action Authorization"));

        let without_edits = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::from_slice(&[LlmCapability::TextMessage]),
            None,
        );
        assert!(!titles(&without_edits).contains(&"Action Authorization"));
    }

    #[test]
    fn editable_scope_applies_the_turn_marker() {
        let rules = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::develop_xml(),
            Some("M12345"),
        );
        let scope = body(&rules, "Editable Scope");
        assert!(scope.contains("<create_M12345 path=\"...\">"));
        assert!(scope.contains("<move_M12345 from=\"...\" to=\"...\" />"));
        assert!(!scope.contains("<create path"));
    }
}
