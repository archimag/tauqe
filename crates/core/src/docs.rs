//! Documentation embedded into the binary. The same chapters serve human readers
//! and the model, which requests them on demand via the `<doc_request>` tag.
//!
//! Every chapter is a sovereign concept file under `docs/src/`, so a topic maps
//! to a small, cohesive set of chapters instead of one umbrella narrative.

use tauqe_protocol::DocTopic;

const INTRO: &str = include_str!("../../../docs/src/intro.md");
const SEMANTIC_HARNESS: &str =
    include_str!("../../../docs/src/ch01-foundations/semantic-harness.md");
const BOUNDED_TURN: &str = include_str!("../../../docs/src/ch01-foundations/bounded-turn-fsm.md");
const MODEL_ROLES: &str = include_str!("../../../docs/src/ch01-foundations/model-roles.md");
const SCRATCHPAD: &str = include_str!("../../../docs/src/ch01-foundations/turn-scratchpad.md");
const CONTEXT_MODEL: &str = include_str!("../../../docs/src/ch02-perception/context-model.md");
const DISCUSSION_FOCUS: &str =
    include_str!("../../../docs/src/ch02-perception/discussion-focus.md");
const REPO_MAP: &str = include_str!("../../../docs/src/ch02-perception/repo-map.md");
const DISCOVERY_ENGINE: &str =
    include_str!("../../../docs/src/ch02-perception/discovery-engine.md");
const PROJECT_PROTOCOL: &str =
    include_str!("../../../docs/src/ch03-project/tauqe-project-protocol.md");
const MODIFICATION_LIFECYCLE: &str =
    include_str!("../../../docs/src/ch03-project/three-phase-lifecycle.md");
const VERIFICATION_HEALING: &str =
    include_str!("../../../docs/src/ch03-project/verification-healing.md");
const GIT_SAFETY: &str = include_str!("../../../docs/src/ch03-project/git-transactions.md");
const PLANS: &str = include_str!("../../../docs/src/ch04-workflows/plans.md");
const REVIEW: &str = include_str!("../../../docs/src/ch04-workflows/review.md");
const CONFIGURATION: &str = include_str!("../../../docs/src/ch05-system/configuration.md");
const TUI: &str = include_str!("../../../docs/src/ch05-system/tui-ux.md");
const I18N: &str = include_str!("../../../docs/src/ch05-system/i18n-subsystem.md");

/// Topics advertised to the model, in the order the system prompt lists them.
/// The exhaustive match in [`get_documentation`] is the compile-time guard that
/// keeps this list aligned with [`DocTopic`].
const TOPICS: &[DocTopic] = &[
    DocTopic::Overview,
    DocTopic::Context,
    DocTopic::Workflow,
    DocTopic::Git,
    DocTopic::Interface,
    DocTopic::Review,
    DocTopic::Plan,
    DocTopic::Config,
    DocTopic::All,
];

/// Renders the authoritative documentation topics advertised in the system
/// prompt.
pub(crate) fn topic_hint() -> String {
    TOPICS
        .iter()
        .map(DocTopic::as_str)
        .collect::<Vec<_>>()
        .join("|")
}

/// Returns the concatenated documentation chapters for the requested topic.
///
/// A topic names one cohesive slice of the book's five parts, so a request loads
/// focused invariants without the context noise of neighboring subsystems.
pub(crate) fn get_documentation(topic: DocTopic) -> String {
    let sections: &[&str] = match topic {
        // Part I: the paradigm and the roles that own "what" and "how".
        DocTopic::Overview => &[INTRO, SEMANTIC_HARNESS, MODEL_ROLES],
        // Part II: the perceived working set and how discovery widens it.
        DocTopic::Context => &[CONTEXT_MODEL, DISCUSSION_FOCUS, REPO_MAP, DISCOVERY_ENGINE],
        // Part I and Part III: the turn machine plus the project work it
        // delegates to the driver.
        DocTopic::Workflow => &[
            BOUNDED_TURN,
            SCRATCHPAD,
            PROJECT_PROTOCOL,
            MODIFICATION_LIFECYCLE,
            VERIFICATION_HEALING,
        ],
        DocTopic::Git => &[GIT_SAFETY],
        // Part V: the terminal surfaces, including their localization.
        DocTopic::Interface => &[TUI, I18N],
        DocTopic::Review => &[REVIEW],
        DocTopic::Plan => &[PLANS],
        DocTopic::Config => &[CONFIGURATION],
        DocTopic::All => &[
            INTRO,
            SEMANTIC_HARNESS,
            BOUNDED_TURN,
            MODEL_ROLES,
            SCRATCHPAD,
            CONTEXT_MODEL,
            DISCUSSION_FOCUS,
            REPO_MAP,
            DISCOVERY_ENGINE,
            PROJECT_PROTOCOL,
            MODIFICATION_LIFECYCLE,
            VERIFICATION_HEALING,
            GIT_SAFETY,
            PLANS,
            REVIEW,
            CONFIGURATION,
            TUI,
            I18N,
        ],
    };
    sections
        .iter()
        .map(|section| section.trim())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_hint_lists_every_advertised_topic() {
        let hint = topic_hint();
        for topic in TOPICS {
            assert!(hint.contains(topic.as_str()));
        }
    }

    #[test]
    fn every_topic_resolves_to_its_chapters() {
        assert!(get_documentation(DocTopic::Overview).contains("The Semantic Harness"));
        assert!(get_documentation(DocTopic::Overview).contains("Model Roles and the Coder Ladder"));
        assert!(get_documentation(DocTopic::Context).contains("The Context Model"));
        assert!(get_documentation(DocTopic::Context).contains("Discovery Engine"));
        assert!(get_documentation(DocTopic::Workflow).contains("The TauqeProjectProtocol"));
        assert!(get_documentation(DocTopic::Workflow).contains("Turn Scratchpad"));
        assert!(get_documentation(DocTopic::Git).contains("Git Safety & Transactions"));
        assert!(get_documentation(DocTopic::Interface).contains("Keyboard-First Interface"));
        assert!(
            get_documentation(DocTopic::Interface).contains("Zero-Hardcode Interface Localization")
        );
        assert!(get_documentation(DocTopic::Review).contains("Structured Code Review"));
        assert!(get_documentation(DocTopic::Plan).contains("Local Plans"));
        assert!(
            get_documentation(DocTopic::Config).contains("Configuration and Project Preparation")
        );
    }

    #[test]
    fn every_chapter_is_reachable_by_a_topic() {
        let all = get_documentation(DocTopic::All);
        for marker in [
            "Semantic Harness",
            "Autonomous Bounded Turn FSM",
            "Model Roles and the Coder Ladder",
            "Turn Scratchpad",
            "The Context Model",
            "Discussion Focus",
            "Repository Map and Blinkers",
            "Discovery Engine",
            "The TauqeProjectProtocol",
            "Three-Phase Modification Lifecycle",
            "Structured Diagnostics and Verification Healing",
            "Git Safety & Transactions",
            "Local Plans",
            "Structured Code Review",
            "Configuration and Project Preparation",
            "Keyboard-First Interface",
            "Zero-Hardcode Interface Localization",
        ] {
            assert!(all.contains(marker), "missing chapter marker: {marker}");
        }
    }
}
