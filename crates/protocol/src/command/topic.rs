//! Topic vocabulary of the harness documentation requests.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Topic of a system documentation request.
///
/// Replaces the former free-form topic string so a request topic is a closed,
/// exhaustively matched set. `All` is the default and the tolerant fallback,
/// honoring Postel's law at the model-output ingest gateway: an unknown, empty,
/// or missing topic resolves to the full documentation set instead of failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", from = "String")]
pub enum DocTopic {
    /// Project introduction and philosophy.
    Overview,
    /// Three-tier context model.
    Context,
    /// Autonomous bounded-turn lifecycle.
    Workflow,
    /// Git safety and transactions.
    Git,
    /// Keyboard-first interface.
    Interface,
    /// Code review workflow.
    Review,
    /// Local plans.
    Plan,
    /// Project and user configuration reference.
    Config,
    /// Every documentation chapter aggregated (default).
    #[default]
    All,
}

impl std::convert::From<String> for DocTopic {
    fn from(s: String) -> Self {
        Self::parse_lossy(&s)
    }
}

impl std::convert::From<&str> for DocTopic {
    fn from(s: &str) -> Self {
        Self::parse_lossy(s)
    }
}

impl DocTopic {
    /// Every documentation topic in canonical prompt order.
    pub const ALL: &'static [DocTopic] = &[
        Self::Overview,
        Self::Context,
        Self::Workflow,
        Self::Git,
        Self::Interface,
        Self::Review,
        Self::Plan,
        Self::Config,
        Self::All,
    ];

    /// Canonical lower-case identifier used on the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Context => "context",
            Self::Workflow => "workflow",
            Self::Git => "git",
            Self::Interface => "interface",
            Self::Review => "review",
            Self::Plan => "plan",
            Self::Config => "config",
            Self::All => "all",
        }
    }

    /// Tolerant constructor: trims, matches aliases case-insensitively, and
    /// resolves any unknown or empty topic to [`DocTopic::All`].
    pub fn parse_lossy(topic: &str) -> Self {
        topic.trim().parse().unwrap_or(Self::All)
    }
}

impl fmt::Display for DocTopic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DocTopic {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "overview" => Ok(Self::Overview),
            "context" => Ok(Self::Context),
            "workflow" => Ok(Self::Workflow),
            "git" => Ok(Self::Git),
            "interface" => Ok(Self::Interface),
            "review" => Ok(Self::Review),
            // `plans` is a legacy plural alias kept only for tolerant model
            // output parsing; scheduled for removal in 0.2.0 and tracked by the
            // wire-contract test in `lib.rs`.
            "plan" | "plans" => Ok(Self::Plan),
            "config" => Ok(Self::Config),
            "all" => Ok(Self::All),
            other => Err(format!(
                "Unknown documentation topic '{}'. Expected one of: {}",
                other,
                Self::ALL
                    .iter()
                    .map(DocTopic::as_str)
                    .collect::<Vec<_>>()
                    .join("|")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::LlmCommand;

    #[test]
    fn test_doc_topic_serde_aliases_default_and_parsing() {
        assert_eq!(serde_json::to_string(&DocTopic::Plan).unwrap(), "\"plan\"");
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"plans\"").unwrap(),
            DocTopic::Plan
        );
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"  PLANS  \"").unwrap(),
            DocTopic::Plan
        );
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"all\"").unwrap(),
            DocTopic::All
        );
        // Serde deserialization is tolerant on the wire according to Postel's law.
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"bogus\"").unwrap(),
            DocTopic::All
        );
        assert_eq!(DocTopic::default(), DocTopic::All);

        assert_eq!(DocTopic::parse_lossy("bogus"), DocTopic::All);
        assert_eq!(DocTopic::parse_lossy(""), DocTopic::All);
        assert_eq!(DocTopic::parse_lossy("  Interface "), DocTopic::Interface);
        assert_eq!(DocTopic::Interface.to_string(), "interface");
        assert_eq!("GIT".parse::<DocTopic>().unwrap(), DocTopic::Git);

        assert_eq!(
            serde_json::to_string(&DocTopic::Config).unwrap(),
            "\"config\""
        );
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"config\"").unwrap(),
            DocTopic::Config
        );
        assert_eq!(DocTopic::Config.to_string(), "config");
        assert_eq!("config".parse::<DocTopic>().unwrap(), DocTopic::Config);
        assert!("bogus".parse::<DocTopic>().is_err());

        // Unknown doc topic inside an LlmCommand does not fail deserialization.
        let raw = r#"{"type":"doc_request","topic":"nonexistent_topic"}"#;
        let command: LlmCommand = serde_json::from_str(raw).unwrap();
        assert_eq!(
            command,
            LlmCommand::DocRequest {
                topic: DocTopic::All
            }
        );
    }

    #[test]
    fn test_doc_topic_all_roundtrips_through_wire_names() {
        for topic in DocTopic::ALL {
            assert_eq!(topic.as_str().parse::<DocTopic>().unwrap(), *topic);
        }
    }
}
