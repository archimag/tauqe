//! Typed composition of the turn-regime trunk of the `Core System Contract`.
//!
//! Every trunk rule is selected by typed inputs: the declared turn intent and
//! the enabled capability surface. No rule is chosen by inspecting rendered
//! text, and no rule carries a hand-written number — each list is numbered
//! once, by position, when it is rendered.
//!
//! The trunk is assembled as two typed lists. The shared list carries the
//! invariants that every turn states identically, so it renders into the
//! byte-stable head of the prompt. The intent list carries the rules that only
//! one regime states, so it renders into the tail delta of that regime.
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

use tauqe_protocol::{CapabilitySet, EditProtocolKind, LlmCapability};

use super::registry::{ContractContext, ContractRule};

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
/// discussion, and the review discussion. The review dialogue runs as an
/// interactive turn, so it shares the interactive trunk by construction.
///
/// A rule that names the wire representation renders the variant of the
/// declared protocol. The two-contour mode never reaches the trunk, because
/// prompt assembly hands each contour its concrete protocol, so every
/// declaration other than `Xml` renders the structured spelling.
pub fn shared_trunk_rules(
    ctx: &ContractContext<'_>,
    capabilities: &CapabilitySet,
) -> Vec<ContractRule> {
    let structured = ctx.protocol != EditProtocolKind::Xml;

    let mut rules = vec![
        ContractRule::new(
            "Authoritative Source",
            "The files provided in <context> represent the authoritative current state of the project. Do not invent missing code.",
        ),
        ContractRule::new(
            "No Arbitrary Shell",
            "You do not have shell execution capabilities. Work strictly through the context and actions provided.",
        ),
    ];

    // A failure rooted in the host system cannot be repaired by editing code:
    // the only real fix is a terminal command the developer runs. The contract
    // therefore forbids every surrogate the model could reach for instead.
    if capabilities.contains(LlmCapability::Abort) {
        let abort_command = if structured {
            "report it immediately through the `abort` field with `is_aborted` set to true, its `reason` naming the concrete missing capability and its `recommendation` carrying the exact command or instruction".to_string()
        } else {
            format!(
                "emit <{abort} reason=\"...\" recommendation=\"...\" /> immediately",
                abort = ctx.tag_name("abort"),
            )
        };
        let blocker = if capabilities.contains(LlmCapability::Blocked) {
            if structured {
                " Reporting the same failure through the `blocked` field is not a substitute either: a blocker states a failed precondition the harness replans around, and no amount of replanning installs a package, approves a build script, or starts a service.".to_string()
            } else {
                format!(
                    " Reporting the same failure as a <{blocked}> outcome is not a substitute either: a blocker states a failed precondition the harness replans around, and no amount of replanning installs a package, approves a build script, or starts a service.",
                    blocked = ctx.tag_name("blocked"),
                )
            }
        } else {
            String::new()
        };
        let handling = if structured {
            "Put the exact copy-pasteable command, or the exact step-by-step instruction, in `recommendation`, so the developer can complete it without further investigation."
        } else {
            "Name the concrete missing capability in `reason` and put the exact copy-pasteable command, or the exact step-by-step instruction, in `recommendation`, so the developer can complete it without further investigation."
        };
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

    rules
}

/// Returns the intent-specific delta of the `Core System Contract`.
///
/// The list carries the rules that only one regime states: the scope and
/// authorship of an editing turn, the action authorization and the plan-first
/// proposal of a dialogue, and the decision immutability, verification, and
/// scope rules of an isolated step. The registry renders it after the shared
/// trunk, and the tail delta of a turn carries it once the head is emitted on
/// its own.
///
/// Selection reads [`ContractContext::intent`] and the capability surface, never
/// the text of a rule, so a rule cannot migrate between the two lists by
/// accident. A rule that names the wire representation renders the variant of
/// the declared protocol, exactly as in the shared trunk.
pub fn intent_delta_rules(
    ctx: &ContractContext<'_>,
    capabilities: &CapabilitySet,
) -> Vec<ContractRule> {
    let structured = ctx.protocol != EditProtocolKind::Xml;
    let mut rules: Vec<ContractRule> = Vec::new();

    rules.push(ContractRule::new(
        "Read-Only Scope",
        "files inside <context> carry a declarative `access` attribute. Files marked `access=\"read_only\"` are provided for reference, context, and structural comprehension; prefer modifying files marked `access=\"editable\"`. If completing the task requires editing a read-only file, you may propose edits to it directly without an extra discovery round — the environment will automatically promote it to editable upon applying the changes.",
    ));
    let editable_scope = if structured {
        "files inside <context> marked `access=\"editable\"` are permitted for modification. You may also create new files by appending an entry to the `changes` array whose `op` is 'create', or move and rename files with an entry whose `op` is 'move', when required by the task.".to_string()
    } else {
        format!(
            "files inside <context> marked `access=\"editable\"` are permitted for modification. You may also create new files using <{create} path=\"...\"> or move/rename files using <{moved} from=\"...\" to=\"...\" /> when required by the task.",
            create = ctx.tag_name("create"),
            moved = ctx.tag_name("move"),
        )
    };
    rules.push(ContractRule::new("Editable Scope", editable_scope));

    // A file-editing turn is guarded by the authorization rule wherever it can
    // actually write: an interactive dialogue, a review dialogue, and a plan
    // discussion. An isolated step is itself the explicit instruction, so the
    // rule is deliberately absent there.
    if !ctx.intent.is_isolated_step() && capabilities.contains(LlmCapability::FileEdits) {
        rules.push(ContractRule::new(
            "Action Authorization",
            "Default to discussion: conversational dialogue and technical analysis is the default mode of interaction. Whenever there is any ambiguity or doubt regarding the user's intent, or when the user asks an open, exploratory, analytical, or architectural question, engage strictly in conversational discussion without editing files or creating plans. Proceed directly with minimal coherent edits only when the required change is clear, specific, and unambiguous (e.g. direct instructions, straightforward bug fixes, or obvious adjustments) and that change is compact and indivisible. Propose, analyze, and seek confirmation first when there is architectural ambiguity, competing trade-offs, multiple viable designs, or when the work spans several independent subsystems. An analytical or exploratory request is never closed by silence: once the files, searches, or documentation you requested are available, the same turn must carry the written analysis the developer asked for, and an empty reply is never a valid conclusion. An answer is the deliverable of such a turn, so gathering material never substitutes for it.",
        ));

        // The proposal offers a fork between patching now and planning first. It
        // belongs to every dialogue contour — the free REPL and the plan
        // discussion — and never to an isolated step, which implements an
        // approved design instead of proposing one.
        if (ctx.intent.is_interactive_repl() || ctx.intent.is_plan_discussion())
            && capabilities.contains(LlmCapability::PlanCreation)
        {
            let proposal = if structured {
                "A request is broad when it touches several independent subsystems or contracts, when its implementation has dependent stages that must land in order, or when those stages need separate verification. Purely mechanical or localized work — a cross-cutting rename, a signature update, one focused fix — is never broad, and you execute it immediately. When a broad request arrives, propose planning instead of patching: explain briefly in plain text why decomposition is warranted, then offer the developer the choice between drafting a plan and performing the whole change at once. A proposal is not a change: a non-empty `changes` array or a `plan_update` payload in the same answer is strictly prohibited, and no plan document may be created until the developer explicitly confirms or requests one. If the developer declines and asks for the whole task, execute it completely and never repeat the proposal. A proposal-only turn must still end with the `outcome` field carrying status 'success' and a concise summary of the offer.".to_string()
            } else {
                format!(
                    "A request is broad when it touches several independent subsystems or contracts, when its implementation has dependent stages that must land in order, or when those stages need separate verification. Purely mechanical or localized work — a cross-cutting rename, a signature update, one focused fix — is never broad, and you execute it immediately. When a broad request arrives, propose planning instead of patching: explain briefly in plain text why decomposition is warranted, then offer the developer the choice between drafting a plan and performing the whole change at once. A proposal is not a change: emitting an <{edits} ...> tag or a <{plan} ...> tag in the same answer is strictly prohibited, and no plan document may be created until the developer explicitly confirms or requests one. If the developer declines and asks for the whole task, execute it completely and never repeat the proposal. A proposal-only turn must still end with an <{success} ...> tag carrying a concise summary of the offer.",
                    edits = ctx.tag_name("edits"),
                    plan = ctx.tag_name("plan"),
                    success = ctx.tag_name("success"),
                )
            };
            rules.push(ContractRule::new("Plan-First Proposal", proposal));
        }
    }

    if ctx.intent.is_isolated_step() {
        let immutability = if structured {
            "The architectural decision of this step is fixed by the Architect role and is immutable. You own the implementation only, never the requirement: revising the chosen architecture, substituting the components, libraries, or interfaces the decision selected, bypassing the contracts it designed, or relaxing its requirements is strictly prohibited. When the step cannot be implemented within the approved design, report the conflict through the `blocked` field with `is_blocked` set to true, its `reason` naming the violated decision and its `details` describing the exact conflict, so the harness starts a controlled replanning cycle instead of an improvised alternative design.".to_string()
        } else {
            format!(
                "The architectural decision of this step is fixed by the Architect role and is immutable. You own the implementation only, never the requirement: revising the chosen architecture, substituting the components, libraries, or interfaces the decision selected, bypassing the contracts it designed, or relaxing its requirements is strictly prohibited. When the step cannot be implemented within the approved design, end the turn with a <{blocked} reason=\"...\" details=\"...\" /> outcome describing the exact conflict, so the harness starts a controlled replanning cycle instead of an improvised alternative design.",
                blocked = ctx.tag_name("blocked"),
            )
        };
        rules.push(ContractRule::new(
            "Architectural Decision Immutability",
            immutability,
        ));
        rules.push(ContractRule::new(
            "Code Verification",
            if structured {
                "Verification on code modifications is handled automatically and deterministically by the harness at the end of each step. Leave the `verify` field unfilled and keep its `target` at 'none' (the harness runs toolchain verification and gating automatically)."
            } else {
                "Verification on code modifications is handled automatically and deterministically by the harness at the end of each step. Do NOT emit verification tags (the harness runs toolchain verification and gating automatically)."
            },
        ));
        rules.push(ContractRule::new(
            "Step Execution Scope",
            "You are executing a single isolated plan step. Plan creation, modification, and status updates are strictly prohibited (step lifecycle and plan tracking are handled deterministically by the harness).",
        ));
    }

    rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::registry::render_contract_rules;
    use crate::capabilities::PromptRenderContext;
    use tauqe_protocol::TurnIntent;

    /// Builds the shared trunk for one typed regime and wire contour.
    fn shared(
        intent: TurnIntent,
        set: &CapabilitySet,
        protocol: EditProtocolKind,
        marker: Option<&str>,
    ) -> Vec<ContractRule> {
        let render = PromptRenderContext::new(&[], marker);
        let ctx = ContractContext::new(intent, protocol, render);
        shared_trunk_rules(&ctx, set)
    }

    /// Builds the intent-specific delta for one typed regime and wire contour.
    fn delta(
        intent: TurnIntent,
        set: &CapabilitySet,
        protocol: EditProtocolKind,
        marker: Option<&str>,
    ) -> Vec<ContractRule> {
        let render = PromptRenderContext::new(&[], marker);
        let ctx = ContractContext::new(intent, protocol, render);
        intent_delta_rules(&ctx, set)
    }

    /// Builds the trunk rules for one typed regime and wire contour.
    ///
    /// The two lists are concatenated so a test can inspect the rules a regime
    /// states as a whole; the split itself is asserted by the dedicated tests.
    fn trunk(
        intent: TurnIntent,
        set: &CapabilitySet,
        protocol: EditProtocolKind,
        marker: Option<&str>,
    ) -> Vec<ContractRule> {
        let mut rules = shared(intent, set, protocol, marker);
        rules.extend(delta(intent, set, protocol, marker));
        rules
    }

    /// Collects the rule numbers rendered for one list of rules.
    fn rule_numbers(rules: &[ContractRule]) -> Vec<usize> {
        render_contract_rules(rules)
            .lines()
            .filter_map(|line| {
                let (prefix, _) = line.split_once(". ")?;
                prefix.parse::<usize>().ok()
            })
            .collect()
    }

    /// Returns the three regimes the trunk renders for.
    fn regimes() -> Vec<(TurnIntent, CapabilitySet)> {
        vec![
            (
                TurnIntent::InteractiveRepl,
                CapabilitySet::interactive_repl(),
            ),
            (TurnIntent::PlanDiscussion, CapabilitySet::discussion()),
            (TurnIntent::IsolatedStep, CapabilitySet::isolated_step()),
        ]
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
        "<doc_request",
        "<custom_tool",
    ];

    /// Top-level properties of the composed structured response schema.
    const SCHEMA_FIELDS: &[&str] = &[
        "changes",
        "plan_update",
        "message",
        "context_requests",
        "repo_searches",
        "repo_dirs",
        "repo_maps",
        "dir_lists",
        "doc_requests",
        "verify",
        "scratchpad",
        "outcome",
        "blocked",
        "abort",
        "user_language",
        "custom_tools",
    ];

    #[test]
    fn host_environment_failures_forbid_stubs_and_blockers() {
        let rules = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            None,
        );
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

        // The rule sits beside the shell rule it complements, ahead of the
        // authorization rule that governs edits.
        let names = titles(&rules);
        let shell = names
            .iter()
            .position(|title| *title == "No Arbitrary Shell")
            .expect("shell rule");
        let host = names
            .iter()
            .position(|title| *title == "Host Environment Failures")
            .expect("host rule");
        let authorization = names
            .iter()
            .position(|title| *title == "Action Authorization")
            .expect("authorization rule");
        assert_eq!(host, shell + 1);
        assert!(host < authorization);
    }

    #[test]
    fn host_environment_failures_require_the_abort_capability() {
        // A surface without the abort capability never states the rule.
        let narrowed_discussion = CapabilitySet::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::PlanCreation,
            LlmCapability::PlanUpdate,
            LlmCapability::Discovery,
            LlmCapability::UserLanguage,
        ]);
        let discussion = trunk(
            TurnIntent::PlanDiscussion,
            &narrowed_discussion,
            EditProtocolKind::Xml,
            None,
        );
        assert!(!titles(&discussion).contains(&"Host Environment Failures"));

        // A narrowed surface without the abort capability keeps the dense
        // numbering of its own rules and never advertises the rule.
        let narrowed = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::FileEdits]),
            EditProtocolKind::Xml,
            None,
        );
        assert!(!titles(&narrowed).contains(&"Host Environment Failures"));

        // An unattended step is exactly where an unverified stub would ship
        // unseen, so the rule is rendered there as well.
        let step = trunk(
            TurnIntent::IsolatedStep,
            &CapabilitySet::isolated_step(),
            EditProtocolKind::Xml,
            None,
        );
        assert!(titles(&step).contains(&"Host Environment Failures"));
    }

    #[test]
    fn host_environment_failures_render_both_tags_through_the_marker() {
        let rules = trunk(
            TurnIntent::IsolatedStep,
            &CapabilitySet::isolated_step(),
            EditProtocolKind::Xml,
            Some("M12345"),
        );
        let rule = body(&rules, "Host Environment Failures");

        assert!(rule.contains("abort_M12345"));
        assert!(rule.contains("blocked_M12345"));
        assert!(!rule.contains("abort reason"));
    }

    #[test]
    fn trunk_opens_with_the_authoritative_source() {
        let rules = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
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
    fn discussion_trunk_carries_the_dialogue_surface() {
        let rules = trunk(
            TurnIntent::PlanDiscussion,
            &CapabilitySet::discussion(),
            EditProtocolKind::Xml,
            None,
        );
        let names = titles(&rules);
        // The rule that restricted a plan discussion to prose is gone: the
        // regime renders the interactive trunk instead of a discussion-only one.
        assert!(!names.contains(&"Discussion Mode Scope"));
        // A plan discussion carries the full dialogue surface, so it states the
        // same scope and authorization rules as any other editing dialogue.
        assert!(names.contains(&"Read-Only Scope"));
        assert!(names.contains(&"Editable Scope"));
        assert!(names.contains(&"Action Authorization"));
        assert!(!names.contains(&"Read-Only Context Exploration"));
        // A discussion may also author a new plan, so it offers the same fork
        // between patching now and planning first.
        assert!(names.contains(&"Plan-First Proposal"));
    }

    #[test]
    fn discussion_trunk_renders_the_interactive_dialogue_surface() {
        let rules = trunk(
            TurnIntent::PlanDiscussion,
            &CapabilitySet::discussion(),
            EditProtocolKind::Xml,
            None,
        );

        // No discussion-only scope rule survives: an edit travels the ordinary
        // transactional path once the developer states the request directly.
        assert!(!titles(&rules).contains(&"Discussion Mode Scope"));
        assert!(body(&rules, "Action Authorization").contains("Default to discussion"));

        // The shared scope pair travels with the discussion, so the XML contour
        // renders its own edit spellings there instead of the JSON fields.
        let editable = body(&rules, "Editable Scope");
        assert!(editable.contains("access=\"editable\""));
        assert!(editable.contains("<create"));
        assert!(!editable.contains("`changes`"));

        // The very same trunk renders for the free REPL, which is what keeps the
        // provider prefix cache warm when a dialogue changes its focus.
        let repl = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            None,
        );
        assert_eq!(render_contract_rules(&rules), render_contract_rules(&repl));
    }

    #[test]
    fn isolated_step_trunk_delegates_verification_and_scopes_the_step() {
        let rules = trunk(
            TurnIntent::IsolatedStep,
            &CapabilitySet::isolated_step(),
            EditProtocolKind::Xml,
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
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            None,
        );
        assert!(titles(&with_edits).contains(&"Action Authorization"));

        let without_edits = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::from_slice(&[LlmCapability::TextMessage]),
            EditProtocolKind::Xml,
            None,
        );
        assert!(!titles(&without_edits).contains(&"Action Authorization"));
    }

    #[test]
    fn editable_scope_applies_the_turn_marker() {
        let rules = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            Some("M12345"),
        );
        let scope = body(&rules, "Editable Scope");
        assert!(scope.contains("<create_M12345 path=\"...\">"));
        assert!(scope.contains("<move_M12345 from=\"...\" to=\"...\" />"));
        assert!(!scope.contains("<create path"));
    }

    #[test]
    fn action_authorization_is_bounded_by_change_scale() {
        let rules = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            None,
        );
        let authorization = body(&rules, "Action Authorization");
        assert!(!authorization.contains("ONLY when"));
        assert!(authorization.contains("Default to discussion"));
        assert!(authorization.contains("doubt"));
        assert!(authorization.contains("compact and indivisible"));
        assert!(authorization.contains("several independent subsystems"));
    }

    #[test]
    fn action_authorization_demands_the_written_analysis_after_discovery() {
        let rules = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            None,
        );
        let authorization = body(&rules, "Action Authorization");

        // The investigation a discovery round performs never concludes the
        // turn on its own: the same turn owes the developer the analysis.
        assert!(authorization.contains("never closed by silence"));
        assert!(authorization.contains("same turn must carry the written analysis"));
        assert!(authorization.contains("an empty reply is never a valid conclusion"));

        // The obligation is a general invariant, so the rule names no concrete
        // control tag that a narrowed capability set could disable.
        assert!(!authorization.contains("<success"));
        assert!(!authorization.contains("<error"));
        assert!(!authorization.contains("<blocked"));
    }

    #[test]
    fn plan_first_proposal_is_gated_by_intent_and_capabilities() {
        let develop = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            None,
        );
        let names = titles(&develop);
        let authorization = names
            .iter()
            .position(|title| *title == "Action Authorization")
            .expect("action authorization");
        let proposal = names
            .iter()
            .position(|title| *title == "Plan-First Proposal")
            .expect("plan-first proposal");
        assert_eq!(proposal, authorization + 1);

        // A surface that cannot author plans never proposes one.
        let without_plans = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::isolated_step(),
            EditProtocolKind::Xml,
            None,
        );
        assert!(!titles(&without_plans).contains(&"Plan-First Proposal"));

        // A plan discussion carries the whole dialogue surface, so it offers the
        // same fork between patching now and planning first.
        let discussion = trunk(
            TurnIntent::PlanDiscussion,
            &CapabilitySet::discussion(),
            EditProtocolKind::Xml,
            None,
        );
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
        // An isolated step cannot author plans either.
        let step = trunk(
            TurnIntent::IsolatedStep,
            &CapabilitySet::isolated_step(),
            EditProtocolKind::Xml,
            None,
        );
        assert!(!titles(&step).contains(&"Plan-First Proposal"));
    }

    #[test]
    fn plan_first_proposal_forbids_edits_and_requires_explicit_confirmation() {
        let rules = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            Some("M12345"),
        );
        let proposal = body(&rules, "Plan-First Proposal");

        // The categorical prohibition names every control tag through the
        // active turn marker, so the rule stays a command rather than prose.
        assert!(proposal.contains("<edits_M12345 ...>"));
        assert!(proposal.contains("<plan_M12345 ...>"));
        assert!(proposal.contains("<success_M12345 ...>"));
        assert!(!proposal.contains("<edits ...>"));

        // The rule carries the scale criteria, the explicit-confirmation
        // boundary, and the ban on repeating a declined offer.
        assert!(proposal.contains("independent subsystems"));
        assert!(proposal.contains("explicitly confirms"));
        assert!(proposal.contains("never repeat the proposal"));
        assert!(proposal.contains("cross-cutting rename"));
    }

    #[test]
    fn develop_trunk_carries_action_authorization_and_plan_first_proposal() {
        let rules = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            None,
        );

        let authorization = body(&rules, "Action Authorization");
        assert!(authorization.contains("compact and indivisible"));
        assert!(authorization.contains("several independent subsystems"));

        let proposal = body(&rules, "Plan-First Proposal");
        assert!(proposal.contains("planning instead of patching"));
        assert!(proposal.contains("dependent stages"));
        assert!(proposal.contains("explicitly confirms"));
    }

    #[test]
    fn plan_first_proposal_requires_the_plan_creation_capability() {
        // A surface that edits files but cannot author plans has no fork
        // between patching now and planning first, whichever transport carries
        // it.
        let planless = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl().without(LlmCapability::PlanCreation),
            EditProtocolKind::Xml,
            None,
        );
        assert!(titles(&planless).contains(&"Action Authorization"));
        assert!(!titles(&planless).contains(&"Plan-First Proposal"));

        // A bare file-edit surface is the same case: without plan creation
        // there is no proposal channel to offer.
        let edits_only = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::FileEdits]),
            EditProtocolKind::Xml,
            None,
        );
        assert!(titles(&edits_only).contains(&"Action Authorization"));
        assert!(!titles(&edits_only).contains(&"Plan-First Proposal"));
    }

    #[test]
    fn plan_first_proposal_renders_every_tag_through_the_turn_marker() {
        let marked = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            Some("M12345"),
        );
        let proposal = body(&marked, "Plan-First Proposal");
        assert!(proposal.contains("<edits_M12345 ...>"));
        assert!(proposal.contains("<plan_M12345 ...>"));
        assert!(proposal.contains("<success_M12345 ...>"));
        assert!(!proposal.contains("<edits ...>"));
        assert!(!proposal.contains("<plan ...>"));
        assert!(!proposal.contains("<success ...>"));

        // Without a marker the same rule renders bare tag names, so the rule
        // never hardcodes one spelling of the control tags.
        let bare = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            None,
        );
        let bare_proposal = body(&bare, "Plan-First Proposal");
        assert!(bare_proposal.contains("<edits ...>"));
        assert!(bare_proposal.contains("<plan ...>"));
        assert!(bare_proposal.contains("<success ...>"));
        assert!(!bare_proposal.contains("_M12345"));
    }

    #[test]
    fn only_the_isolated_step_never_renders_the_proposal_rule() {
        let discussion = trunk(
            TurnIntent::PlanDiscussion,
            &CapabilitySet::discussion(),
            EditProtocolKind::Xml,
            Some("M12345"),
        );
        assert!(titles(&discussion).contains(&"Plan-First Proposal"));

        let step = trunk(
            TurnIntent::IsolatedStep,
            &CapabilitySet::isolated_step(),
            EditProtocolKind::Xml,
            Some("M12345"),
        );
        assert!(!titles(&step).contains(&"Plan-First Proposal"));
        assert!(!body(&step, "Action Authorization").contains("planning instead of patching"));
    }

    #[test]
    fn isolated_step_trunk_pins_the_architectural_decision() {
        let rules = trunk(
            TurnIntent::IsolatedStep,
            &CapabilitySet::isolated_step(),
            EditProtocolKind::Xml,
            None,
        );
        let immutability = body(&rules, "Architectural Decision Immutability");
        assert!(immutability.contains("immutable"));
        assert!(immutability.contains("never the requirement"));
        assert!(immutability.contains("<blocked reason=\"...\" details=\"...\" />"));

        // The rule belongs to the isolated-step regime: an interactive turn may
        // still revise a decision with the developer, and a read-only
        // discussion never implements anything in the first place.
        for (intent, set) in [
            (
                TurnIntent::InteractiveRepl,
                CapabilitySet::interactive_repl(),
            ),
            (TurnIntent::PlanDiscussion, CapabilitySet::discussion()),
        ] {
            let rules = trunk(intent, &set, EditProtocolKind::Xml, None);
            assert!(!titles(&rules).contains(&"Architectural Decision Immutability"));
        }
    }

    #[test]
    fn architectural_decision_immutability_renders_the_blocked_tag_through_the_marker() {
        let rules = trunk(
            TurnIntent::IsolatedStep,
            &CapabilitySet::isolated_step(),
            EditProtocolKind::Xml,
            Some("M12345"),
        );
        let immutability = body(&rules, "Architectural Decision Immutability");
        assert!(immutability.contains("<blocked_M12345 reason=\"...\" details=\"...\" />"));
        assert!(!immutability.contains("<blocked reason"));
    }

    #[test]
    fn both_contours_render_the_same_rule_sequence() {
        // The wire format changes how a rule spells its command, never which
        // invariants the regime states or in which order.
        for (intent, set) in regimes() {
            let xml = trunk(intent, &set, EditProtocolKind::Xml, None);
            let structured = trunk(intent, &set, EditProtocolKind::Structured, None);
            assert_eq!(
                titles(&xml),
                titles(&structured),
                "the two contours render identical rule titles"
            );
        }
    }

    #[test]
    fn structured_trunk_never_renders_an_operational_tag() {
        for (intent, set) in regimes() {
            let rules = trunk(intent, &set, EditProtocolKind::Structured, Some("M12345"));
            for rule in &rules {
                for prefix in XML_TAG_PREFIXES {
                    assert!(
                        !rule.title.contains(prefix),
                        "rule title '{}' advertises {prefix} in the structured contour",
                        rule.title
                    );
                    assert!(
                        !rule.body.contains(prefix),
                        "rule '{}' advertises {prefix} in the structured contour",
                        rule.title
                    );
                }
            }
        }
    }

    #[test]
    fn xml_trunk_never_names_a_structured_schema_field() {
        for (intent, set) in regimes() {
            let rules = trunk(intent, &set, EditProtocolKind::Xml, Some("M12345"));
            for rule in &rules {
                for field in SCHEMA_FIELDS {
                    let quoted = format!("`{field}`");
                    assert!(
                        !rule.body.contains(&quoted),
                        "rule '{}' names the schema field {quoted} in the XML contour",
                        rule.title
                    );
                }
            }
        }
    }

    #[test]
    fn structured_twins_name_the_fields_they_govern() {
        let develop = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Structured,
            None,
        );

        // Creating and moving files addresses the change set, never a tag.
        let scope = body(&develop, "Editable Scope");
        assert!(scope.contains("`changes`"));
        assert!(scope.contains("'create'"));
        assert!(scope.contains("'move'"));

        // A host failure is reported through the abort field, and the blocker
        // clause is ruled out through the blocked field.
        let host = body(&develop, "Host Environment Failures");
        assert!(host.contains("`abort`"));
        assert!(host.contains("`is_aborted`"));
        assert!(host.contains("`reason`"));
        assert!(host.contains("`recommendation`"));
        assert!(host.contains("`blocked`"));
        assert!(host.contains("not a substitute"));
        assert!(host.contains("stub"));

        // A proposal keeps both the change set and the plan payload empty and
        // terminates through the outcome field.
        let proposal = body(&develop, "Plan-First Proposal");
        assert!(proposal.contains("`changes`"));
        assert!(proposal.contains("`plan_update`"));
        assert!(proposal.contains("`outcome`"));
        assert!(proposal.contains("'success'"));
        assert!(proposal.contains("planning instead of patching"));
        assert!(proposal.contains("explicitly confirms"));

        let step = trunk(
            TurnIntent::IsolatedStep,
            &CapabilitySet::isolated_step(),
            EditProtocolKind::Structured,
            None,
        );

        // The immutability rule reports the conflict through the blocked field.
        let immutability = body(&step, "Architectural Decision Immutability");
        assert!(immutability.contains("immutable"));
        assert!(immutability.contains("never the requirement"));
        assert!(immutability.contains("`blocked`"));
        assert!(immutability.contains("`is_blocked`"));
        assert!(immutability.contains("`details`"));

        // Verification is delegated by leaving the verify field idle.
        let verification = body(&step, "Code Verification");
        assert!(verification.contains("`verify`"));
        assert!(verification.contains("'none'"));
        assert!(!verification.contains("verification tags"));
    }

    #[test]
    fn structured_twins_keep_the_shared_invariant_text() {
        let xml = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            None,
        );
        let structured = trunk(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Structured,
            None,
        );

        // Both variants keep the shared invariant wording; only the spelling
        // of the command differs. The authorization rule bounds edits by the
        // scale of the change, and the proposal rule states that criterion
        // before adding the confirmation boundary.
        for authorization in [
            body(&xml, "Action Authorization"),
            body(&structured, "Action Authorization"),
        ] {
            assert!(authorization.contains("compact and indivisible"));
            assert!(authorization.contains("several independent subsystems"));
        }

        for proposal in [
            body(&xml, "Plan-First Proposal"),
            body(&structured, "Plan-First Proposal"),
        ] {
            assert!(proposal.contains("independent subsystems"));
            assert!(proposal.contains("explicitly confirms"));
            assert!(proposal.contains("never repeat the proposal"));
            assert!(proposal.contains("cross-cutting rename"));
        }
    }

    #[test]
    fn step_execution_scope_is_wire_neutral() {
        for protocol in [EditProtocolKind::Xml, EditProtocolKind::Structured] {
            let rules = trunk(
                TurnIntent::IsolatedStep,
                &CapabilitySet::isolated_step(),
                protocol,
                None,
            );
            let scope = body(&rules, "Step Execution Scope");
            assert!(scope.contains("single isolated plan step"));
            assert!(scope.contains("Plan creation, modification, and status updates"));
            assert!(!scope.contains("plan tags"));
            assert!(!scope.contains("plan_tags"));
        }
    }

    /// Regime-specific rule titles that the shared trunk must never carry.
    const REGIME_SPECIFIC_TITLES: &[&str] = &[
        "Read-Only Scope",
        "Editable Scope",
        "Action Authorization",
        "Plan-First Proposal",
        "Architectural Decision Immutability",
        "Code Verification",
        "Step Execution Scope",
    ];

    #[test]
    fn shared_trunk_states_the_universal_invariants_only() {
        let rules = shared(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            None,
        );
        let names = titles(&rules);
        assert_eq!(names.first(), Some(&"Authoritative Source"));
        assert!(names.contains(&"No Arbitrary Shell"));
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
        // The head is byte-stable only when the shared trunk renders the same
        // bytes for every dialogue. The review discussion runs as an
        // interactive turn, so its share of the trunk is the interactive one by
        // construction; comparing the remaining intents states that the shared
        // list never reads the declared intent either.
        let surface = CapabilitySet::interactive_repl();
        let reference = render_contract_rules(&shared(
            TurnIntent::InteractiveRepl,
            &surface,
            EditProtocolKind::Xml,
            Some("M12345"),
        ));
        assert!(!reference.is_empty());

        for intent in [TurnIntent::PlanDiscussion, TurnIntent::IsolatedStep] {
            let other = render_contract_rules(&shared(
                intent,
                &surface,
                EditProtocolKind::Xml,
                Some("M12345"),
            ));
            assert_eq!(other, reference, "the shared trunk diverged for {intent:?}");
        }
    }

    #[test]
    fn intent_delta_carries_only_the_specifics_of_its_regime() {
        let repl = delta(
            TurnIntent::InteractiveRepl,
            &CapabilitySet::interactive_repl(),
            EditProtocolKind::Xml,
            None,
        );
        let names = titles(&repl);
        assert!(names.contains(&"Read-Only Scope"));
        assert!(names.contains(&"Editable Scope"));
        assert!(names.contains(&"Action Authorization"));
        assert!(names.contains(&"Plan-First Proposal"));
        assert!(!names.contains(&"Discussion Mode Scope"));
        assert!(!names.contains(&"Architectural Decision Immutability"));
        assert!(!names.contains(&"Step Execution Scope"));

        let discussion = delta(
            TurnIntent::PlanDiscussion,
            &CapabilitySet::discussion(),
            EditProtocolKind::Xml,
            None,
        );
        let names = titles(&discussion);
        assert!(!names.contains(&"Discussion Mode Scope"));
        assert!(!names.contains(&"Read-Only Context Exploration"));
        // A plan discussion states the same dialogue pair as the REPL, because
        // its surface is the dialogue head plus the plan mutation.
        assert!(names.contains(&"Read-Only Scope"));
        assert!(names.contains(&"Editable Scope"));
        assert!(names.contains(&"Action Authorization"));
        assert!(names.contains(&"Plan-First Proposal"));
        assert!(!names.contains(&"Step Execution Scope"));

        let step = delta(
            TurnIntent::IsolatedStep,
            &CapabilitySet::isolated_step(),
            EditProtocolKind::Xml,
            None,
        );
        let names = titles(&step);
        assert!(names.contains(&"Architectural Decision Immutability"));
        assert!(names.contains(&"Code Verification"));
        assert!(names.contains(&"Step Execution Scope"));
        assert!(!names.contains(&"Action Authorization"));
        assert!(!names.contains(&"Plan-First Proposal"));
        assert!(!names.contains(&"Discussion Mode Scope"));
        assert_eq!(names.last(), Some(&"Step Execution Scope"));
    }

    #[test]
    fn every_regime_states_a_non_empty_delta_beside_the_shared_trunk() {
        for (intent, set) in regimes() {
            for protocol in [EditProtocolKind::Xml, EditProtocolKind::Structured] {
                assert!(!shared(intent, &set, protocol, None).is_empty());
                assert!(
                    !delta(intent, &set, protocol, None).is_empty(),
                    "the {intent:?} regime states no intent-specific rule"
                );
            }
        }
    }

    #[test]
    fn both_lists_are_numbered_densely_and_independently() {
        for (intent, set) in regimes() {
            for protocol in [EditProtocolKind::Xml, EditProtocolKind::Structured] {
                for rules in [
                    shared(intent, &set, protocol, None),
                    delta(intent, &set, protocol, None),
                ] {
                    let numbers = rule_numbers(&rules);
                    assert!(!numbers.is_empty());
                    assert_eq!(
                        numbers,
                        (1..=numbers.len()).collect::<Vec<_>>(),
                        "the {intent:?} regime renders non-dense numbering"
                    );
                }
            }
        }
    }
}
