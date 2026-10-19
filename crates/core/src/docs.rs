//! Documentation embedded into the binary. The same chapters serve human readers
//! and the model, which requests them on demand via the `<doc_request>` tag.

use tauqe_protocol::DocTopic;

const INTRO: &str = include_str!("../../../docs/src/intro.md");
const PHILOSOPHY: &str = include_str!("../../../docs/src/ch01-philosophy/semantic-harness.md");
const CONTEXT: &str = include_str!("../../../docs/src/ch02-context/three-tier-model.md");
const LIFECYCLE: &str = include_str!("../../../docs/src/ch03-bounded-turn/lifecycle.md");
const GIT_SAFETY: &str = include_str!("../../../docs/src/ch04-git-safety/checkpoints-and-undo.md");
const INTERFACE: &str = include_str!("../../../docs/src/ch05-interface/tui-ux.md");
const REVIEW: &str = include_str!("../../../docs/src/ch06-review/review.md");
const PLANS: &str = include_str!("../../../docs/src/ch07-plans/plans.md");
const CONFIG: &str = include_str!("../../../docs/src/ch08-config/configuration.md");

/// Renders the authoritative documentation topics advertised in the system
/// prompt, derived from [`DocTopic::ALL`] so the hint keeps a single source of
/// truth.
pub(crate) fn topic_hint() -> String {
    DocTopic::ALL
        .iter()
        .map(DocTopic::as_str)
        .collect::<Vec<_>>()
        .join("|")
}

/// Returns the concatenated documentation chapters for the requested topic.
pub(crate) fn get_documentation(topic: DocTopic) -> String {
    let sections: &[&str] = match topic {
        DocTopic::Overview => &[INTRO, PHILOSOPHY],
        DocTopic::Context => &[CONTEXT],
        DocTopic::Workflow => &[LIFECYCLE],
        DocTopic::Git => &[GIT_SAFETY],
        DocTopic::Interface => &[INTERFACE],
        DocTopic::Review => &[REVIEW],
        DocTopic::Plan => &[PLANS],
        DocTopic::Config => &[CONFIG],
        DocTopic::All => &[
            INTRO, PHILOSOPHY, CONTEXT, LIFECYCLE, GIT_SAFETY, INTERFACE, REVIEW, PLANS, CONFIG,
        ],
    };
    sections
        .iter()
        .map(|s| s.trim())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_documentation_topics() {
        assert!(get_documentation(DocTopic::Interface).contains("Ctrl+1"));
        assert!(get_documentation(DocTopic::Plan).contains("Local Plans"));
        assert!(
            get_documentation(DocTopic::Config).contains("Configuration and Project Preparation")
        );
        let all = get_documentation(DocTopic::All);
        assert!(all.contains("Semantic Harness"));
        assert!(all.contains("Ctrl+1"));
        assert!(all.contains("Local Plans"));
        assert!(all.contains("Configuration and Project Preparation"));
    }

    #[test]
    fn test_topic_hint_lists_every_topic() {
        let hint = topic_hint();
        for topic in DocTopic::ALL {
            assert!(hint.contains(topic.as_str()));
        }
    }
}
