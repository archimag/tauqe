//! Documentation embedded into the binary. The same chapters serve human readers
//! and the model, which requests them on demand via the `<doc_request>` tag.

const INTRO: &str = include_str!("../../../docs/src/intro.md");
const PHILOSOPHY: &str = include_str!("../../../docs/src/ch01-philosophy/semantic-harness.md");
const CONTEXT: &str = include_str!("../../../docs/src/ch02-context/three-tier-model.md");
const LIFECYCLE: &str = include_str!("../../../docs/src/ch03-bounded-turn/lifecycle.md");
const GIT_SAFETY: &str = include_str!("../../../docs/src/ch04-git-safety/checkpoints-and-undo.md");
const INTERFACE: &str = include_str!("../../../docs/src/ch05-interface/tui-ux.md");
const REVIEW: &str = include_str!("../../../docs/src/ch06-review/review.md");

/// Topics the model can request. The last one aggregates every chapter.
pub(crate) const TOPICS: &[&str] = &[
    "overview",
    "context",
    "workflow",
    "git",
    "interface",
    "review",
    "all",
];

/// Maps a requested topic to a known one; unknown topics resolve to `all`.
pub(crate) fn resolve_topic(topic: &str) -> &'static str {
    TOPICS
        .iter()
        .copied()
        .find(|known| *known == topic)
        .unwrap_or("all")
}

pub(crate) fn get_documentation(topic: &str) -> String {
    let sections: &[&str] = match resolve_topic(topic) {
        "overview" => &[INTRO, PHILOSOPHY],
        "context" => &[CONTEXT],
        "workflow" => &[LIFECYCLE],
        "git" => &[GIT_SAFETY],
        "interface" => &[INTERFACE],
        "review" => &[REVIEW],
        _ => &[
            INTRO, PHILOSOPHY, CONTEXT, LIFECYCLE, GIT_SAFETY, INTERFACE, REVIEW,
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
    fn test_resolve_topic_falls_back_to_all() {
        assert_eq!(resolve_topic("interface"), "interface");
        assert_eq!(resolve_topic("unknown"), "all");
    }

    #[test]
    fn test_get_documentation_topics() {
        assert!(get_documentation("interface").contains("Ctrl+1"));
        let all = get_documentation("all");
        assert!(all.contains("Semantic Harness"));
        assert!(all.contains("Ctrl+1"));
        assert_eq!(get_documentation("bogus"), all);
    }
}
