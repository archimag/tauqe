use serde::{Deserialize, Serialize};

/// Reasoning depth (cognitive effort) requested for a semantic role.
///
/// Kept strictly orthogonal to the model identity: the same model may be
/// driven at different reasoning depths depending on the role it plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
}

impl ReasoningEffort {
    /// Canonical lowercase wire representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::XHigh => "xhigh",
        }
    }
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "low" => Ok(ReasoningEffort::Low),
            "medium" => Ok(ReasoningEffort::Medium),
            "high" => Ok(ReasoningEffort::High),
            "xhigh" => Ok(ReasoningEffort::XHigh),
            other => Err(format!(
                "Unknown reasoning effort '{}'. Expected low, medium, high, or xhigh",
                other
            )),
        }
    }
}

impl Serialize for ReasoningEffort {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReasoningEffort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// Deserializes an optional reasoning effort, treating blank strings as absence.
pub(super) fn deserialize_effort_opt<'de, D>(
    deserializer: D,
) -> Result<Option<ReasoningEffort>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    parse_effort_opt(raw).map_err(serde::de::Error::custom)
}

/// Parses an optional reasoning effort from an already-decoded string value.
///
/// Absent and blank strings map to `None`. A non-empty unrecognized value
/// yields the precise diagnostic from the effort parser, so callers can forward
/// it verbatim instead of collapsing it into an opaque enum-variant error.
pub(super) fn parse_effort_opt(raw: Option<String>) -> Result<Option<ReasoningEffort>, String> {
    match raw {
        None => Ok(None),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed.parse().map(Some)
            }
        }
    }
}

/// Optional per-role reasoning effort overrides.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelRolesEffort {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_effort_opt"
    )]
    pub scribe: Option<ReasoningEffort>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_effort_opt"
    )]
    pub coder: Option<ReasoningEffort>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_effort_opt"
    )]
    pub architect: Option<ReasoningEffort>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_effort_opt"
    )]
    pub primary: Option<ReasoningEffort>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_effort_opt"
    )]
    pub reviewer: Option<ReasoningEffort>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reasoning_effort_parse_display_and_serde() {
        assert_eq!("low".parse::<ReasoningEffort>().unwrap(), ReasoningEffort::Low);
        assert_eq!(
            "  HIGH ".parse::<ReasoningEffort>().unwrap(),
            ReasoningEffort::High
        );
        assert_eq!(ReasoningEffort::XHigh.to_string(), "xhigh");
        assert!("extreme".parse::<ReasoningEffort>().is_err());

        let json = serde_json::to_string(&ReasoningEffort::XHigh).unwrap();
        assert_eq!(json, "\"xhigh\"");
        let de: ReasoningEffort = serde_json::from_str("\"Medium\"").unwrap();
        assert_eq!(de, ReasoningEffort::Medium);

        let efforts = ModelRolesEffort {
            architect: Some(ReasoningEffort::XHigh),
            ..Default::default()
        };
        let efforts_json = serde_json::to_string(&efforts).unwrap();
        assert_eq!(efforts_json, "{\"architect\":\"xhigh\"}");
        let de_efforts: ModelRolesEffort = serde_json::from_str(&efforts_json).unwrap();
        assert_eq!(de_efforts, efforts);

        let blank: ModelRolesEffort = serde_json::from_str("{\"coder\":\"\"}").unwrap();
        assert_eq!(blank.coder, None);
    }
}
