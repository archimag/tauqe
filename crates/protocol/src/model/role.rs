use serde::{Deserialize, Serialize};

/// Six semantic model role specializations.
///
/// Deserialization routes through [`std::str::FromStr`] via
/// `#[serde(try_from = "String")]`, so the wire form and the
/// configuration/CLI form share one parser and accept the same spellings.
/// Serialization always emits the canonical `snake_case` name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum ModelRole {
    Scribe,
    Coder,
    Architect,
    Gatekeeper,
    #[serde(rename = "repl")]
    REPL,
    Reviewer,
}

impl std::fmt::Display for ModelRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelRole::Scribe => f.write_str("scribe"),
            ModelRole::Coder => f.write_str("coder"),
            ModelRole::Architect => f.write_str("architect"),
            ModelRole::Gatekeeper => f.write_str("gatekeeper"),
            ModelRole::REPL => f.write_str("repl"),
            ModelRole::Reviewer => f.write_str("reviewer"),
        }
    }
}

impl std::str::FromStr for ModelRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "scribe" => Ok(ModelRole::Scribe),
            "coder" => Ok(ModelRole::Coder),
            "architect" => Ok(ModelRole::Architect),
            "gatekeeper" => Ok(ModelRole::Gatekeeper),
            "repl" => Ok(ModelRole::REPL),
            "reviewer" => Ok(ModelRole::Reviewer),
            other => Err(format!(
                "Unknown model role '{}'. Expected scribe, coder, architect, gatekeeper, repl, or reviewer",
                other
            )),
        }
    }
}

impl std::convert::TryFrom<String> for ModelRole {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}
