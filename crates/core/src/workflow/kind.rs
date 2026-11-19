//! Configuration identities owned by the execution engine.
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

/// Discrete workflow engine that drives a development turn.
///
/// Replaces the former free-form workflow string. `Git` is the default
/// transactional workflow: the working tree is checkpointed and every applied
/// turn is committed. `Naive` applies edits directly to the working tree
/// without commit safety and is also known as the single-turn or direct mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum WorkflowKind {
    /// Transactional Git workflow (default).
    #[default]
    Git,
    /// Direct working-tree edits without commit checkpoints.
    Naive,
}

impl std::convert::TryFrom<String> for WorkflowKind {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl WorkflowKind {
    /// Canonical lower-case identifier used in configuration.
    fn as_str(&self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Naive => "naive",
        }
    }
}

impl fmt::Display for WorkflowKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WorkflowKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "git" => Ok(Self::Git),
            "naive" => Ok(Self::Naive),
            other => Err(format!(
                "Unknown workflow '{}'. Expected 'git' or 'naive'",
                other
            )),
        }
    }
}

/// Discrete edit protocol used to decode model output for a turn.
///
/// All turns interact with models exclusively via strict JSON-schema
/// Structured Output (`response_format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum EditProtocolKind {
    /// JSON-schema structured output protocol (default and only protocol).
    #[default]
    Structured,
}

impl std::convert::TryFrom<String> for EditProtocolKind {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl EditProtocolKind {
    /// Canonical lower-case identifier used in configuration.
    fn as_str(&self) -> &'static str {
        match self {
            Self::Structured => "structured",
        }
    }
}

impl fmt::Display for EditProtocolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EditProtocolKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "structured" => Ok(Self::Structured),
            // Backward compatibility aliases for reading older configurations
            "xml" | "mixed" | "auto" => Ok(Self::Structured),
            other => Err(format!(
                "Unknown edit protocol '{}'. Expected 'structured'",
                other
            )),
        }
    }
}

/// Strategy that decides how a change set addresses the lines it replaces.
///
/// The strategy is orthogonal to the transport. Both the XML and the structured
/// contour serialize the same edit vocabulary, and the strategy only decides
/// which fields address a line range: `SearchReplace` echoes the replaced text
/// and matches it exactly, while `LineAnchor` names a range through the compact
/// opaque anchors rendered beside every context line, so the model never has to
/// reproduce the lines it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum PatchStrategy {
    /// Search/replace blocks that echo the replaced lines.
    SearchReplace,
    /// Opaque line anchors naming a range of lines without echoing them.
    LineAnchor,
    /// Codex patch with file sections and context hunks.
    #[default]
    ApplyPatch,
}

impl std::convert::TryFrom<String> for PatchStrategy {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl PatchStrategy {
    /// Canonical lower-case identifier used on the wire.
    fn as_str(&self) -> &'static str {
        match self {
            Self::SearchReplace => "search_replace",
            Self::LineAnchor => "line_anchor",
            Self::ApplyPatch => "apply_patch",
        }
    }
}

impl fmt::Display for PatchStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PatchStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "search_replace" => Ok(Self::SearchReplace),
            "line_anchor" => Ok(Self::LineAnchor),
            "apply_patch" => Ok(Self::ApplyPatch),
            other => Err(format!(
                "Unknown patch strategy '{}'. Expected 'apply_patch', 'search_replace', or 'line_anchor'",
                other
            )),
        }
    }
}

#[cfg(test)]
mod patch_strategy_tests {
    use super::*;
    #[test]
    fn test_patch_strategy_serde_default_and_parsing() {
        assert_eq!(PatchStrategy::default(), PatchStrategy::ApplyPatch);
        assert_eq!("apply_patch".parse(), Ok(PatchStrategy::ApplyPatch));
        assert_eq!(
            serde_json::to_string(&PatchStrategy::SearchReplace).unwrap(),
            "\"search_replace\""
        );
        assert_eq!(
            serde_json::to_string(&PatchStrategy::LineAnchor).unwrap(),
            "\"line_anchor\""
        );
        assert_eq!(
            serde_json::from_str::<PatchStrategy>("\"line_anchor\"").unwrap(),
            PatchStrategy::LineAnchor
        );
        assert_eq!(
            serde_json::from_str::<PatchStrategy>("\"  LINE_ANCHOR \"").unwrap(),
            PatchStrategy::LineAnchor
        );
        assert!(serde_json::from_str::<PatchStrategy>("\"anchors\"").is_err());

        assert_eq!(PatchStrategy::SearchReplace.as_str(), "search_replace");
        assert_eq!(PatchStrategy::LineAnchor.to_string(), "line_anchor");
        assert_eq!(
            "line_anchor".parse::<PatchStrategy>().unwrap(),
            PatchStrategy::LineAnchor
        );
        assert!("".parse::<PatchStrategy>().is_err());
        assert!("bogus".parse::<PatchStrategy>().is_err());
    }
}
