//! Topic vocabulary of the harness documentation requests.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Topic of a system documentation request.
///
/// Replaces the former free-form topic string so a request topic is a closed,
/// exhaustively matched set. `All` is the default and the tolerant fallback,
/// honoring Postel's law at the model-output ingest gateway: an unknown, empty,
/// missing, or explicitly `null` topic resolves to the full documentation set
/// instead of failing, while a value of any other shape keeps reporting the
/// precise type error it produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
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
            "plan" => Ok(Self::Plan),
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

/// Null-tolerant wire decoding of a documentation topic.
///
/// The enum is decoded through a visitor instead of a derived conversion, so a
/// present-but-`null` topic resolves to the tolerant `DocTopic::All` fallback
/// and never takes the enclosing command or proposal with it. No `visit_*` arm
/// is declared for any other shape, so a value that is neither a string nor
/// `null` keeps reporting the precise type error serde produced for it.
impl<'de> Deserialize<'de> for DocTopic {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct TopicVisitor;

        impl<'de> serde::de::Visitor<'de> for TopicVisitor {
            type Value = DocTopic;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a documentation topic string or null")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(DocTopic::parse_lossy(value))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(DocTopic::parse_lossy(&value))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(DocTopic::All)
            }
        }

        deserializer.deserialize_any(TopicVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{DocRequestCommand, LlmCommand};

    #[test]
    fn test_doc_topic_serde_aliases_default_and_parsing() {
        assert_eq!(serde_json::to_string(&DocTopic::Plan).unwrap(), "\"plan\"");
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
        // `plans` named the plan topic before the vocabulary was fixed, so the
        // closed topic set no longer carries it.
        assert!("plans".parse::<DocTopic>().is_err());

        // Unknown doc topic inside an LlmCommand does not fail deserialization.
        let raw = r#"{"type":"doc_request","topic":"nonexistent_topic"}"#;
        let command: LlmCommand = serde_json::from_str(raw).unwrap();
        assert_eq!(
            command,
            LlmCommand::DocRequest(DocRequestCommand {
                topic: DocTopic::All
            })
        );
    }
    #[test]
    fn test_doc_request_without_topic_decodes_to_all() {
        // The documented missing-topic fallback holds at the command boundary.
        let command: LlmCommand = serde_json::from_str(r#"{"type":"doc_request"}"#).unwrap();
        assert_eq!(
            command,
            LlmCommand::DocRequest(DocRequestCommand {
                topic: DocTopic::All
            })
        );
    }

    /// A present-but-`null` topic is a partial emission rather than a defect, so
    /// it resolves to the tolerant fallback exactly like a missing or unknown
    /// topic and never takes the enclosing command with it.
    #[test]
    fn test_doc_topic_accepts_the_null_shape() {
        for spelling in ["tauqe_doc_request", "doc_request"] {
            let raw = format!(r#"{{"type":"{spelling}","topic":null}}"#);
            let command: LlmCommand = serde_json::from_str(&raw)
                .expect("a null topic must decode instead of rejecting the command");
            assert_eq!(
                command,
                LlmCommand::DocRequest(DocRequestCommand {
                    topic: DocTopic::All
                }),
                "{spelling}"
            );
        }

        // The topic type itself is total over the null shape.
        assert_eq!(
            serde_json::from_str::<DocTopic>("null").unwrap(),
            DocTopic::All
        );

        // A shape that is neither a string nor `null` keeps reporting its own
        // type error instead of being tolerated.
        let error = serde_json::from_str::<DocTopic>("7")
            .expect_err("an integer topic must be rejected")
            .to_string();
        assert!(error.contains("invalid type: integer"), "{error}");
    }

    #[test]
    fn test_doc_topic_all_roundtrips_through_wire_names() {
        for topic in DocTopic::ALL {
            assert_eq!(topic.as_str().parse::<DocTopic>().unwrap(), *topic);
        }
    }
}
