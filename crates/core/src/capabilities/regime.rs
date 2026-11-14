//! Typed composition of the turn-regime trunk of the `Core System Contract`.
//!
//! Every trunk rule is selected by typed inputs: the declared turn intent and
//! the enabled capability surface. No rule is chosen by inspecting rendered
//! text, and no rule carries a hand-written number — each list is numbered
//! once, by position, when it is rendered.
//!
//! The trunk is assembled as two typed lists. The shared list carries the
//! invariants that every turn states identically, so it renders into the
//! byte-stable head of the prompt. The intent list carries the rules that the
//! shared list cannot state for every regime — the scope and authorization
//! rules of a dialogue and the decision, verification, and scope rules of an
//! isolated step — so it renders into the tail delta of the turn.
//!
//! A rule that names the wire representation is dualized by the declared
//! protocol. The XML contour spells a command as a marker-bound tag through
//! [`ContractContext::tag_name`], while the structured contour names the field
//! of the strict response schema and never mentions markup. Both variants state
//! the same invariant, so the capability surface of a turn follows from its
//! intent and never from the transport that carries it.
//!
//! The trunk lives here rather than inside prompt assembly so the regime has a
//! single source of truth: prompt rendering, the structured response schema,
//! and command decoding all observe the same typed surface.

use tauqe_protocol::{CapabilityExpr, LlmCapability};

use crate::prompt::ContourKind;

use super::rules::{ContractContext, ContractRule, CODE_VERIFICATION_GUARANTEE};
/// Returns the shared, intent-independent trunk of the `Core System Contract`.
///
/// The list carries only the invariants that every turn states in exactly the
/// same words, so it renders into the byte-stable head of the prompt: the
/// authoritative-source rule, the shell prohibition, the host-failure handling
/// selected by the `abort` capability, and the reasoning-language rule.
///
/// The list never reads [`ContractContext::intent`]: two turns that share a
/// capability surface and the render inputs produce identical rules, which is
/// what keeps the head identical for the interactive dialogue, the plan
/// discussion, and the review discussion. The review dialogue declares a
/// regime of its own, and the trunk never reads it.
///
/// A rule that names the wire representation renders the variant of the
/// declared protocol. The two-contour mode never reaches the trunk, because
/// prompt assembly hands each contour its concrete protocol, so every
/// declaration other than `Xml` renders the structured spelling.
pub fn shared_trunk_rules(
    _ctx: &ContractContext<'_>,
    capabilities: &CapabilityExpr,
) -> Vec<ContractRule> {
    let mut rules = vec![
        ContractRule::new(
            "Authoritative Source",
            "The files provided in <context> represent the authoritative current state of the project. Do not invent missing code.",
        ),
    ];

    // A failure rooted in the host system cannot be repaired by editing code:
    // the only real fix is a terminal command the developer runs. The contract
    // therefore forbids every surrogate the model could reach for instead.
    if capabilities.contains(LlmCapability::Abort) {
        let abort_command = "report it immediately through the `abort` field with `is_aborted` set to true, its `reason` naming the concrete missing capability and its `recommendation` carrying the exact command or instruction".to_string();
        let blocker = if capabilities.contains(LlmCapability::Blocked) {
            " Reporting the same failure through the `blocked` field is not a substitute either: a blocker states a failed precondition the harness replans around, and no amount of replanning installs a package, approves a build script, or starts a service.".to_string()
        } else {
            String::new()
        };
        let handling = "Put the exact copy-pasteable command, or the exact step-by-step instruction, in `recommendation`, so the developer can complete it without further investigation.";
        rules.push(ContractRule::new(
            "Host Environment Failures",
            format!(
                "A failure whose cause lies in the host system is repaired by the developer, never by the code. When compilation, a test run, or a post-hook fails because an external system utility or package is missing, a dependency manager still has to install or approve packages before its build scripts may run, a database, daemon, or service has not been started, or an external registry demands authentication, {abort_command}. Writing a stub, a surrogate, a placeholder, or a fallback implementation in place of the real code is strictly prohibited, because it ships behavior nobody verified while hiding the missing capability.{blocker} {handling}"
            ),
        ));
    }

    rules.push(ContractRule::new(
        "Reasoning in English",
        "Always conduct internal reasoning, planning, and thinking strictly in English to preserve token budget and maximize reasoning quality.",
    ));

    rules.push(ContractRule::new(
        "Response Language",
        "Always formulate all conversational explanations, answers, and user-facing messages in the same language as the user's request. Do not switch to English for user-facing responses unless requested.",
    ));
    if capabilities.contains(LlmCapability::OuterDocRequest) {
        let request =
            "request it by that prefix through the `outer_doc_requests` field".to_string();
        let contrast = if !capabilities.contains(LlmCapability::DocRequest) {
            String::new()
        } else {
            " The `tauqe_doc_requests` field requests the internal documentation of the harness itself; neither channel substitutes for the other.".to_string()
        };
        rules.push(ContractRule::new(
            "External Project Documentation",
            format!(
                "A project may declare external documentation in `[[outer_docs]]`, keyed by an identifier that names one root document, for material it reasons against but does not version: an upstream API note, a protocol summary, or a published book. When the task must be checked against such a document, {request}, and never invent an identifier the project does not declare: a refusal names its cause and is not retried inside the same turn.{contrast}"
            ),
        ));
    }

    rules
}

pub fn intent_delta_rules(
    ctx: &ContractContext<'_>,
    capabilities: &CapabilityExpr,
) -> Vec<ContractRule> {
    let mut rules = Vec::new();
    // The contour is declared by the loop that owns the turn, so the rules of a
    // step follow that declaration instead of a composition of leaves.
    let is_step_contour = ctx.contour == ContourKind::Step;

    if has_any_edit_capability(capabilities) {
        rules.push(ContractRule::new(
            "File Access",
            "files inside <context> carry a declarative `access` attribute. Files marked `access=\"read_only\"` are reference material provided for comprehension and are never modified; files marked `access=\"editable\"` may be modified. When the task requires changing a read-only file, propose the edit immediately without an extra discovery round: the environment promotes the file to editable when it applies the change.",
        ));
    }

    if !is_step_contour && has_any_edit_capability(capabilities) {
        rules.push(ContractRule::new(
            "Action Authorization",
            "Proactive engineering stance: when the developer asks to change code, fix a bug, or inspect a subsystem, that request is executed immediately with concrete action instead of passive commentary or manual instructions. If the relevant files are not yet in context, select the `research` branch (`repo_searches`, `context_requests`) to locate and load them in the same turn; if they are already in context, propose the minimal coherent change in the `edit` branch. Discussion is the mode for open, exploratory, analytical, and architectural questions, and for genuine ambiguity about the developer's intent: there you analyze, weigh competing trade-offs, and seek confirmation before editing files or creating a plan. An analytical or exploratory request is never closed by silence: once the files, searches, or documentation you requested are available, the same turn must carry the written analysis the developer asked for, and an empty reply is never a valid conclusion. An answer is the deliverable of such a turn, so gathering material never substitutes for it.",
        ));

        if capabilities.contains(LlmCapability::PlanCreation) {
            let proposal = "A broad request — one that touches several independent subsystems or contracts, or whose dependent stages must land in order — is answered by proposing planning instead of patching: explain briefly in plain text why decomposition is warranted and offer the developer the choice between drafting a plan and performing the whole change at once. Purely mechanical or localized work, such as a cross-cutting rename or one focused fix, is never broad and is executed immediately. A proposal is not a change: a non-empty `changes` array or a plan payload (`plan_create` or `plan_update`) in the same answer is strictly prohibited, and no plan document may be created until the developer explicitly confirms or requests one. Once the developer confirms, execute the task completely and never repeat the proposal; a proposal-only turn closes in the answer branch and states the offer in its `message`.".to_string();
            rules.push(ContractRule::new("Plan-First Proposal", proposal));
        }
    }

    if is_step_contour {
        let immutability = "The architectural decision of this step is fixed by the Architect role and is immutable. You own the implementation only, never the requirement: revising the chosen architecture, substituting the components, libraries, or interfaces the decision selected, bypassing the contracts it designed, or relaxing its requirements is strictly prohibited. When the step cannot be implemented within the approved design, report the conflict through the `blocked` field with `is_blocked` set to true, its `reason` naming the violated decision and its `details` describing the exact conflict, so the harness starts a controlled replanning cycle instead of an improvised alternative design.".to_string();
        rules.push(ContractRule::new(
            "Architectural Decision Immutability",
            immutability,
        ));
        let mandatory_edits = "You are executing an implementation step. Every execution turn must produce concrete code changes in the `changes` array. Purely conversational responses, analysis, or explanations without code changes are strictly prohibited and will be rejected. If you need more information before editing, emit a discovery request through the research branch: `context_requests`, `repo_searches`, `repo_dirs`, `repo_maps`, or `dir_lists`. If the step cannot be implemented as specified, report a blocker via `blocked`.".to_string();
        rules.push(ContractRule::new(
            "Mandatory Code Implementation",
            mandatory_edits,
        ));
        rules.push(ContractRule::new(
            "Code Verification",
            CODE_VERIFICATION_GUARANTEE,
        ));
        rules.push(ContractRule::new(
            "Step Execution Scope",
            "You are executing a single isolated plan step. Plan creation, modification, and status updates are strictly prohibited (step lifecycle and plan tracking are handled deterministically by the harness).",
        ));
    }

    rules
}

fn has_any_edit_capability(capabilities: &CapabilityExpr) -> bool {
    capabilities.contains(LlmCapability::EditReplace)
        || capabilities.contains(LlmCapability::EditCreate)
        || capabilities.contains(LlmCapability::EditDelete)
        || capabilities.contains(LlmCapability::EditMove)
        || capabilities.contains(LlmCapability::EditMoveDir)
        || capabilities.contains(LlmCapability::EditOverwrite)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{compose_contract_rules, render_contract_rules, PromptRenderContext};
    use tauqe_protocol::DiscussionTarget;

    /// Builds the capability surface of a free interactive dialogue.
    fn repl_surface() -> CapabilityExpr {
        crate::control::repl::repl_capability_expr(None)
    }

    /// Builds the capability surface of a plan-anchored dialogue.
    fn plan_discussion_surface() -> CapabilityExpr {
        crate::control::repl::repl_capability_expr(Some(&DiscussionTarget::Plan {
            id: "p-1".to_string(),
        }))
    }

    /// Builds the capability surface of a review-anchored dialogue.
    fn review_discussion_surface() -> CapabilityExpr {
        crate::control::repl::repl_capability_expr(Some(&DiscussionTarget::Review {
            id: "r-1".to_string(),
        }))
    }

    /// Builds the capability surface of one isolated step.
    fn step_surface() -> CapabilityExpr {
        crate::control::turn::turn_capability_expr()
    }

    /// Builds the capability surface of an Architect horizon synthesis.
    fn architect_surface() -> CapabilityExpr {
        crate::control::refine::architect_synthesis_capability_expr()
    }

    /// Builds the shared trunk for one capability surface.
    fn shared(expr: &CapabilityExpr, marker: Option<&str>) -> Vec<ContractRule> {
        let render = PromptRenderContext::new(&[], marker);
        let ctx = ContractContext::new(render);
        shared_trunk_rules(&ctx, expr)
    }

    /// Builds the surface-specific delta of one contour and capability surface.
    fn delta(
        expr: &CapabilityExpr,
        contour: ContourKind,
        marker: Option<&str>,
    ) -> Vec<ContractRule> {
        let render = PromptRenderContext::new(&[], marker);
        let ctx = ContractContext::new(render).with_contour(contour);
        intent_delta_rules(&ctx, expr)
    }

    /// Builds the trunk rules of one contour and capability surface.
    fn trunk(
        expr: &CapabilityExpr,
        contour: ContourKind,
        marker: Option<&str>,
    ) -> Vec<ContractRule> {
        let mut rules = shared(expr, marker);
        rules.extend(delta(expr, contour, marker));
        rules
    }

    /// Collects the rule numbers rendered for one contract composition.
    fn rule_numbers(rendered: &str) -> Vec<usize> {
        rendered
            .lines()
            .filter_map(|line| {
                let (prefix, _) = line.split_once(". ")?;
                prefix.parse::<usize>().ok()
            })
            .collect()
    }

    /// Returns the capability surfaces the trunk renders for.
    fn regimes() -> Vec<(&'static str, CapabilityExpr, ContourKind)> {
        [
            ("interactive_repl", repl_surface(), ContourKind::Dialogue),
            (
                "plan_discussion",
                plan_discussion_surface(),
                ContourKind::Dialogue,
            ),
            (
                "review_discussion",
                review_discussion_surface(),
                ContourKind::Dialogue,
            ),
            ("isolated_step", step_surface(), ContourKind::Step),
            (
                "architect_synthesis",
                architect_surface(),
                ContourKind::Architect,
            ),
        ]
        .into_iter()
        .collect()
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

    const REGIME_SPECIFIC_TITLES: &[&str] = &[
        "File Access",
        "Action Authorization",
        "Plan-First Proposal",
        "Environment Ontology",
        "Architectural Decision Immutability",
        "Mandatory Code Implementation",
        "Code Verification",
        "Step Execution Scope",
    ];
    /// Operational tag prefixes that belong to the XML contour only.
    const XML_TAG_PREFIXES: &[&str] = &[
        "<abort",
        "<blocked",
        "<create",
        "<move",
        "<edits",
        "<plan",
        "<success",
        "<error",
        "<verify",
        "<skip",
        "<scratchpad",
        "<user_language",
        "<context_request",
        "<repo_search",
        "<repo_dirs",
        "<repo_map",
        "<dir_list",
        "<tauqe_doc_request",
        "<doc_request",
        "<custom_tool",
    ];

    #[test]
    fn host_environment_failures_forbid_stubs_and_blockers() {
        let rules = trunk(&repl_surface(), ContourKind::Dialogue, None);
        let rule = body(&rules, "Host Environment Failures");

        // The rule states the repair the developer performs and forbids every
        // surrogate the model could substitute for the missing capability.
        assert!(rule.contains("abort"));
        assert!(rule.contains("`reason`"));
        assert!(rule.contains("`recommendation`"));
        assert!(rule.contains("stub"));
        assert!(rule.contains("surrogate"));
        assert!(rule.contains("placeholder"));
        assert!(rule.contains("strictly prohibited"));
        assert!(rule.contains("copy-pasteable command"));

        // A blocker is ruled out explicitly: replanning cannot install a
        // package, approve a build script, or start a service.
        assert!(rule.contains("blocked"));
        assert!(rule.contains("not a substitute"));

        // The rule follows the authoritative-source rule it complements, ahead
        // of the authorization rule that governs edits.
        let names = titles(&rules);
        let host = names
            .iter()
            .position(|title| *title == "Host Environment Failures")
            .expect("host rule");
        let authorization = names
            .iter()
            .position(|title| *title == "Action Authorization")
            .expect("authorization rule");
        assert_eq!(host, 1);
        assert!(host < authorization);
    }

    #[test]
    fn host_environment_failures_require_the_abort_capability() {
        // A surface without the abort capability never states the rule.
        let narrowed_discussion = CapabilityExpr::from_leaves([
            LlmCapability::TextMessage,
            LlmCapability::PlanCreation,
            LlmCapability::PlanUpdate,
            LlmCapability::ContextRequest,
        ]);
        let discussion = trunk(&narrowed_discussion, ContourKind::Dialogue, None);
        assert!(!titles(&discussion).contains(&"Host Environment Failures"));

        // A narrowed surface without the abort capability keeps the dense
        // numbering of its own rules and never advertises the rule.
        let narrowed_surface =
            CapabilityExpr::from_leaves([LlmCapability::TextMessage, LlmCapability::EditReplace]);
        let narrowed = trunk(&narrowed_surface, ContourKind::Dialogue, None);
        assert!(!titles(&narrowed).contains(&"Host Environment Failures"));

        // An unattended step is exactly where an unverified stub would ship
        // unseen, so the rule is rendered there as well.
        let step = trunk(&step_surface(), ContourKind::Step, None);
        assert!(titles(&step).contains(&"Host Environment Failures"));
    }

    #[test]
    fn host_environment_failures_render_both_tags_through_the_marker() {
        let rules = trunk(&step_surface(), ContourKind::Step, None);
        let rule = body(&rules, "Host Environment Failures");

        assert!(rule.contains("`abort`"));
        assert!(rule.contains("`blocked`"));
    }

    #[test]
    fn trunk_opens_with_the_authoritative_source() {
        let rules = trunk(&repl_surface(), ContourKind::Dialogue, None);
        assert_eq!(
            rules.first().map(|rule| rule.title.as_str()),
            Some("Authoritative Source")
        );
        let names = titles(&rules);
        assert!(names.contains(&"File Access"));
        assert!(names.contains(&"Reasoning in English"));
        assert!(names.contains(&"Action Authorization"));
    }

    #[test]
    fn discussion_trunk_carries_the_dialogue_surface() {
        let rules = trunk(&plan_discussion_surface(), ContourKind::Dialogue, None);
        let names = titles(&rules);
        assert!(!names.contains(&"Discussion Mode Scope"));
        assert!(names.contains(&"File Access"));
        assert!(names.contains(&"Action Authorization"));
        assert!(!names.contains(&"Read-Only Context Exploration"));
        assert!(names.contains(&"Plan-First Proposal"));
    }

    #[test]
    fn discussion_trunk_renders_the_interactive_dialogue_surface() {
        let rules = trunk(&plan_discussion_surface(), ContourKind::Dialogue, None);

        assert!(!titles(&rules).contains(&"Discussion Mode Scope"));
        assert!(body(&rules, "Action Authorization").contains("executed immediately"));

        let access = body(&rules, "File Access");
        assert!(access.contains("access=\"read_only\""));
        assert!(access.contains("access=\"editable\""));

        let repl = trunk(&repl_surface(), ContourKind::Dialogue, None);
        assert_eq!(render_contract_rules(&rules), render_contract_rules(&repl));
    }

    #[test]
    fn isolated_step_trunk_delegates_verification_and_scopes_the_step() {
        let rules = trunk(&step_surface(), ContourKind::Step, None);
        let names = titles(&rules);
        assert!(names.contains(&"Mandatory Code Implementation"));
        assert!(names.contains(&"Code Verification"));
        assert!(names.contains(&"Step Execution Scope"));
        assert!(!names.contains(&"Action Authorization"));
        assert_eq!(names.last(), Some(&"Step Execution Scope"));
    }

    #[test]
    fn action_authorization_requires_the_file_edit_capability() {
        let with_edits = trunk(&repl_surface(), ContourKind::Dialogue, None);
        assert!(titles(&with_edits).contains(&"Action Authorization"));

        let without_edits_surface = CapabilityExpr::from_leaves([LlmCapability::TextMessage]);
        let without_edits = trunk(&without_edits_surface, ContourKind::Dialogue, None);
        assert!(!titles(&without_edits).contains(&"Action Authorization"));
    }

    #[test]
    fn file_access_rule_states_the_read_only_promotion() {
        let rules = trunk(&repl_surface(), ContourKind::Dialogue, None);
        let access = body(&rules, "File Access");
        assert!(access.contains("read_only"));
        assert!(access.contains("promotes the file to editable"));
    }

    #[test]
    fn action_authorization_states_a_proactive_stance() {
        let rules = trunk(&repl_surface(), ContourKind::Dialogue, None);
        let authorization = body(&rules, "Action Authorization");
        assert!(!authorization.contains("ONLY when"));
        assert!(!authorization.contains("Default to discussion"));
        assert!(authorization.contains("executed immediately"));
        assert!(authorization.contains("Discussion is the mode"));
        assert!(authorization.contains("genuine ambiguity"));
    }

    #[test]
    fn action_authorization_demands_the_written_analysis_after_discovery() {
        let rules = trunk(&repl_surface(), ContourKind::Dialogue, None);
        let authorization = body(&rules, "Action Authorization");

        assert!(authorization.contains("never closed by silence"));
        assert!(authorization.contains("same turn must carry the written analysis"));
        assert!(authorization.contains("an empty reply is never a valid conclusion"));
    }

    #[test]
    fn plan_first_proposal_is_gated_by_intent_and_capabilities() {
        let repl = trunk(&repl_surface(), ContourKind::Dialogue, None);
        let names = titles(&repl);
        let authorization = names
            .iter()
            .position(|title| *title == "Action Authorization")
            .expect("action authorization");
        let proposal = names
            .iter()
            .position(|title| *title == "Plan-First Proposal")
            .expect("plan-first proposal");
        assert_eq!(proposal, authorization + 1);

        let discussion = trunk(&plan_discussion_surface(), ContourKind::Dialogue, None);
        let names = titles(&discussion);
        let authorization = names
            .iter()
            .position(|title| *title == "Action Authorization")
            .expect("action authorization");
        let proposal = names
            .iter()
            .position(|title| *title == "Plan-First Proposal")
            .expect("plan-first proposal");
        assert_eq!(proposal, authorization + 1);

        let step = trunk(&step_surface(), ContourKind::Step, None);
        assert!(!titles(&step).contains(&"Plan-First Proposal"));
    }

    #[test]
    fn plan_first_proposal_forbids_edits_and_requires_explicit_confirmation() {
        let rules = trunk(&repl_surface(), ContourKind::Dialogue, None);
        let proposal = body(&rules, "Plan-First Proposal");

        assert!(proposal.contains("`changes`"));
        assert!(proposal.contains("`plan_create`"));
        assert!(proposal.contains("`plan_update`"));
        assert!(proposal.contains("`message`"));
        assert!(proposal.contains("independent subsystems"));
        assert!(proposal.contains("explicitly confirms"));
        assert!(proposal.contains("never repeat the proposal"));
        assert!(proposal.contains("cross-cutting rename"));
    }

    #[test]
    fn repl_trunk_carries_action_authorization_and_plan_first_proposal() {
        let rules = trunk(&repl_surface(), ContourKind::Dialogue, None);

        let authorization = body(&rules, "Action Authorization");
        assert!(authorization.contains("executed immediately"));
        assert!(authorization.contains("genuine ambiguity"));

        let proposal = body(&rules, "Plan-First Proposal");
        assert!(proposal.contains("planning instead of patching"));
        assert!(proposal.contains("dependent stages"));
        assert!(proposal.contains("explicitly confirms"));
    }

    #[test]
    fn plan_first_proposal_requires_the_plan_creation_capability() {
        let planless_surface = repl_surface()
            .filter(&|capability| capability != LlmCapability::PlanCreation)
            .expect("the shared head survives the narrowing");
        let planless = trunk(&planless_surface, ContourKind::Dialogue, None);
        assert!(titles(&planless).contains(&"Action Authorization"));
        assert!(!titles(&planless).contains(&"Plan-First Proposal"));
    }

    #[test]
    fn only_the_isolated_step_never_renders_the_proposal_rule() {
        let discussion = trunk(&plan_discussion_surface(), ContourKind::Dialogue, None);
        assert!(titles(&discussion).contains(&"Plan-First Proposal"));

        let step = trunk(&step_surface(), ContourKind::Step, None);
        assert!(!titles(&step).contains(&"Plan-First Proposal"));
        assert!(!body(&step, "Action Authorization").contains("planning instead of patching"));
    }

    #[test]
    fn isolated_step_trunk_pins_the_architectural_decision() {
        let rules = trunk(&step_surface(), ContourKind::Step, None);
        let immutability = body(&rules, "Architectural Decision Immutability");
        assert!(immutability.contains("immutable"));
        assert!(immutability.contains("never the requirement"));
        assert!(immutability.contains("`blocked`"));
    }

    #[test]
    fn architectural_decision_immutability_mentions_blocked_field() {
        let rules = trunk(&step_surface(), ContourKind::Step, None);
        let immutability = body(&rules, "Architectural Decision Immutability");
        assert!(immutability.contains("`blocked`"));
    }

    #[test]
    fn structured_trunk_never_renders_an_operational_tag() {
        for (label, surface, contour) in regimes() {
            let rules = trunk(&surface, contour, None);
            for rule in &rules {
                for prefix in XML_TAG_PREFIXES {
                    assert!(
                        !rule.title.contains(prefix),
                        "rule title '{}' advertises {prefix} in the structured contour ({label})",
                        rule.title
                    );
                    assert!(
                        !rule.body.contains(prefix),
                        "rule '{}' advertises {prefix} in the structured contour ({label})",
                        rule.title
                    );
                }
            }
        }
    }

    #[test]
    fn structured_twins_name_the_fields_they_govern() {
        let develop = trunk(&repl_surface(), ContourKind::Dialogue, None);

        let scope = body(&develop, "File Access");
        assert!(scope.contains("access=\"read_only\""));
        assert!(scope.contains("access=\"editable\""));
        assert!(scope.contains("promotes the file to editable"));

        let host = body(&develop, "Host Environment Failures");
        assert!(host.contains("`abort`"));
        assert!(host.contains("`is_aborted`"));
        assert!(host.contains("`reason`"));
        assert!(host.contains("`recommendation`"));
        assert!(host.contains("`blocked`"));
        assert!(host.contains("not a substitute"));
        assert!(host.contains("stub"));

        let proposal = body(&develop, "Plan-First Proposal");
        assert!(proposal.contains("`changes`"));
        assert!(proposal.contains("`plan_create`"));
        assert!(proposal.contains("`plan_update`"));
        assert!(proposal.contains("`message`"));
        assert!(proposal.contains("planning instead of patching"));
        assert!(proposal.contains("explicitly confirms"));

        let step = trunk(&step_surface(), ContourKind::Step, None);

        let immutability = body(&step, "Architectural Decision Immutability");
        assert!(immutability.contains("immutable"));
        assert!(immutability.contains("never the requirement"));
        assert!(immutability.contains("`blocked`"));
        assert!(immutability.contains("`is_blocked`"));
        assert!(immutability.contains("`details`"));

        let verification = body(&step, "Code Verification");
        assert!(verification.contains("automatically and deterministically"));
        assert!(verification.contains("checks the project declares"));
        assert!(verification.contains("rejected"));
        assert!(verification.contains("never reaches the repository"));
        assert!(!verification.contains("`verify`"));
    }

    #[test]
    fn step_execution_scope_is_wire_neutral() {
        let rules = trunk(&step_surface(), ContourKind::Step, None);
        let scope = body(&rules, "Step Execution Scope");
        assert!(scope.contains("single isolated plan step"));
        assert!(scope.contains("Plan creation, modification, and status updates"));
    }

    #[test]
    fn shared_trunk_states_the_universal_invariants_only() {
        let rules = shared(&repl_surface(), None);
        let names = titles(&rules);
        assert_eq!(names.first(), Some(&"Authoritative Source"));
        assert!(names.contains(&"Host Environment Failures"));
        assert!(names.contains(&"Reasoning in English"));
        for specific in REGIME_SPECIFIC_TITLES {
            assert!(
                !names.contains(specific),
                "the shared trunk must not carry the regime-specific rule {specific}"
            );
        }
    }

    #[test]
    fn shared_trunk_is_identical_for_every_dialog_regime() {
        let reference = render_contract_rules(&shared(&repl_surface(), None));
        assert!(!reference.is_empty());

        for surface in [plan_discussion_surface(), review_discussion_surface()] {
            let other = render_contract_rules(&shared(&surface, None));
            assert_eq!(
                other, reference,
                "the shared trunk diverged for another dialogue"
            );
        }
    }

    #[test]
    fn intent_delta_carries_only_the_specifics_of_its_regime() {
        let repl = delta(&repl_surface(), ContourKind::Dialogue, None);
        let names = titles(&repl);
        assert!(names.contains(&"File Access"));
        assert!(names.contains(&"Action Authorization"));
        assert!(names.contains(&"Plan-First Proposal"));
        assert!(!names.contains(&"Discussion Mode Scope"));
        assert!(!names.contains(&"Architectural Decision Immutability"));
        assert!(!names.contains(&"Step Execution Scope"));

        let discussion = delta(&plan_discussion_surface(), ContourKind::Dialogue, None);
        let names = titles(&discussion);
        assert!(!names.contains(&"Discussion Mode Scope"));
        assert!(!names.contains(&"Read-Only Context Exploration"));
        assert!(names.contains(&"File Access"));
        assert!(names.contains(&"Action Authorization"));
        assert!(names.contains(&"Plan-First Proposal"));
        assert!(!names.contains(&"Step Execution Scope"));

        let review = delta(&review_discussion_surface(), ContourKind::Dialogue, None);
        let names = titles(&review);
        assert!(names.contains(&"File Access"));
        assert!(names.contains(&"Action Authorization"));
        assert!(names.contains(&"Plan-First Proposal"));
        assert!(!names.contains(&"Architectural Decision Immutability"));
        assert!(!names.contains(&"Step Execution Scope"));
        assert_eq!(
            render_contract_rules(&review),
            render_contract_rules(&repl),
            "the review delta diverged from the REPL delta"
        );

        let step = delta(&step_surface(), ContourKind::Step, None);
        let names = titles(&step);
        assert!(names.contains(&"Architectural Decision Immutability"));
        assert!(names.contains(&"Mandatory Code Implementation"));
        assert!(names.contains(&"Code Verification"));
        assert!(names.contains(&"Step Execution Scope"));
        assert!(!names.contains(&"Action Authorization"));
        assert!(!names.contains(&"Plan-First Proposal"));
        assert!(!names.contains(&"Discussion Mode Scope"));
        assert_eq!(names.last(), Some(&"Step Execution Scope"));
    }

    /// The contour decides the regime: the same leaves declared as a step render the
    /// step rules, and the step surface declared as a dialogue renders the dialogue
    /// rules, so no composition of leaves selects a regime.
    #[test]
    fn the_step_rules_follow_the_declared_contour_not_the_leaf_composition() {
        let as_step = trunk(&repl_surface(), ContourKind::Step, None);
        let names = titles(&as_step);
        assert!(names.contains(&"Architectural Decision Immutability"));
        assert!(names.contains(&"Mandatory Code Implementation"));
        assert!(names.contains(&"Step Execution Scope"));
        assert!(!names.contains(&"Action Authorization"));
        assert!(!names.contains(&"Plan-First Proposal"));

        let as_dialogue = trunk(&step_surface(), ContourKind::Dialogue, None);
        let names = titles(&as_dialogue);
        assert!(names.contains(&"Action Authorization"));
        assert!(!names.contains(&"Step Execution Scope"));
        assert!(!names.contains(&"Architectural Decision Immutability"));
    }
    #[test]
    fn the_intent_delta_carries_no_documentation_ontology() {
        // A question about the harness is answered by the self-knowledge rule
        // and the authoritative documentation, so the intent delta states no
        // second lecture about the environment.
        let rules = delta(&repl_surface(), ContourKind::Dialogue, None);
        assert!(!titles(&rules).contains(&"Environment Ontology"));

        // Nothing of the delta depends on the documentation capability, so a
        // dialogue that cannot request a document states the very same rules.
        let narrowed = delta(
            &repl_surface()
                .filter(&|capability| capability != LlmCapability::DocRequest)
                .expect("the shared head survives the narrowing"),
            ContourKind::Dialogue,
            None,
        );
        assert_eq!(
            render_contract_rules(&narrowed),
            render_contract_rules(&rules),
            "narrowing the documentation capability changed the intent delta"
        );
    }

    #[test]
    fn the_architect_synthesis_delta_states_no_edit_scope() {
        // The synthesis contour owns no edit scope, no plan authorship, and no
        // step scope, so its intent delta is legitimately empty.
        let rules = delta(&architect_surface(), ContourKind::Architect, None);
        assert!(rules.is_empty());
    }

    #[test]
    fn architect_synthesis_trunk_keeps_the_abort_escape_hatch() {
        let rules = shared(&architect_surface(), None);
        assert!(titles(&rules).contains(&"Host Environment Failures"));
        let host = body(&rules, "Host Environment Failures");
        assert!(host.contains("`abort`"));
        assert!(host.contains("`is_aborted`"));
        assert!(host.contains("`recommendation`"));
    }

    #[test]
    fn every_regime_states_the_shared_trunk_and_only_authorized_delta_rules() {
        for (label, surface, _) in regimes() {
            assert!(
                !shared(&surface, None).is_empty(),
                "the {label} regime states no shared trunk"
            );
        }

        // Every contour that owns an edit scope, a plan authorship, or a step
        // scope states the rules of that scope in its delta.
        for (label, surface, contour) in [
            ("interactive_repl", repl_surface(), ContourKind::Dialogue),
            (
                "plan_discussion",
                plan_discussion_surface(),
                ContourKind::Dialogue,
            ),
            (
                "review_discussion",
                review_discussion_surface(),
                ContourKind::Dialogue,
            ),
            ("isolated_step", step_surface(), ContourKind::Step),
        ] {
            assert!(
                !delta(&surface, contour, None).is_empty(),
                "the {label} regime states no surface-specific rule"
            );
        }

        // The Architect synthesis contour authorizes none of them, so its delta
        // is legitimately empty and the shared trunk carries it alone.
        assert!(delta(&architect_surface(), ContourKind::Architect, None).is_empty());
    }
    #[test]
    fn the_unified_composition_is_numbered_densely() {
        for (label, surface, contour) in regimes() {
            let ctx =
                ContractContext::new(PromptRenderContext::new(&[], None)).with_contour(contour);
            let rendered = compose_contract_rules(&surface, &ctx);
            let numbers = rule_numbers(&rendered);
            assert!(!numbers.is_empty());
            assert_eq!(
                numbers,
                (1..=numbers.len()).collect::<Vec<_>>(),
                "the {label} regime renders non-dense numbering"
            );

            // The unified composition states every rule of the shared trunk
            // beside every rule the declared regime alone authorizes, so no
            // rule of the two former lists may disappear from the sequence.
            for rule in trunk(&surface, contour, None) {
                assert!(
                    rendered.contains(&rule.title),
                    "the unified composition dropped '{}'",
                    rule.title
                );
            }
        }
    }

    #[test]
    fn the_external_documentation_rule_states_the_channel_in_both_contours() {
        let repl = |contour: ContourKind| trunk(&repl_surface(), contour, None);

        let structured_rules = repl(ContourKind::Dialogue);
        let structured = body(&structured_rules, "External Project Documentation");
        assert!(structured.contains("`outer_doc_requests`"));
        assert!(structured.contains("`tauqe_doc_requests`"));
        assert!(structured.contains("never invent"));
        // The prefix and jail semantics live in the catalog heading, so the
        // trunk paragraph states only the essence of the declaration.
        assert!(!structured.contains("`id/relative.md`"));
        assert!(!structured.contains("climbs above the root"));
        assert!(structured.contains("`[[outer_docs]]`"));
    }

    #[test]
    fn the_external_documentation_rule_follows_the_capability_surface() {
        for (label, surface, contour) in regimes() {
            let rules = trunk(&surface, contour, None);
            assert!(
                titles(&rules).contains(&"External Project Documentation"),
                "the {label} regime cannot request the documents it declares"
            );
        }
    }

    #[test]
    fn the_external_documentation_rule_folds_the_turn_marker() {
        let rules = trunk(&repl_surface(), ContourKind::Dialogue, None);
        let rule = body(&rules, "External Project Documentation");
        assert!(rule.contains("`outer_doc_requests`"));
        assert!(!rule.contains("<outer_doc_request id"));
    }
}
