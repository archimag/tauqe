use serde::{Deserialize, Serialize};

use super::effort::{parse_effort_opt, ModelRolesEffort, ReasoningEffort};
use super::reference::ModelRef;

/// A model entry configured with optional reasoning depth and human-readable alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub model: ModelRef,
    pub effort: Option<ReasoningEffort>,
    pub alias: Option<String>,
}

impl ModelEntry {
    pub fn new(model: ModelRef, effort: Option<ReasoningEffort>, alias: Option<String>) -> Self {
        Self { model, effort, alias }
    }

    pub fn display_label(&self) -> String {
        if let Some(alias) = &self.alias {
            alias.clone()
        } else if let Some(effort) = self.effort {
            format!("{}:{} [{}]", self.model.provider, self.model.name, effort)
        } else {
            self.model.to_string()
        }
    }
}

impl From<ModelRef> for ModelEntry {
    fn from(model: ModelRef) -> Self {
        Self {
            model,
            effort: None,
            alias: None,
        }
    }
}

/// Shared polymorphic specification for a configured model entry.
///
/// A configured entry is either a bare model string or a table with a mandatory
/// `model` field and optional `effort` and `alias` fields. Both `ModelEntry` and
/// `CoderTier` deserialize through this single type.
///
/// The value is dispatched on its runtime shape with a visitor rather than an
/// untagged enum. An untagged enum reports every malformed field as the opaque
/// "data did not match any variant", which discards the precise diagnostics
/// produced while parsing the model string or the reasoning effort; the visitor
/// forwards those messages verbatim.
struct ModelEntrySpec {
    model: ModelRef,
    effort: Option<ReasoningEffort>,
    alias: Option<String>,
}

impl<'de> Deserialize<'de> for ModelEntrySpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct SpecVisitor;

        impl<'de> serde::de::Visitor<'de> for SpecVisitor {
            type Value = ModelEntrySpec;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a model string or a { model, effort, alias } table")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value.parse::<ModelRef>() {
                    Ok(model) => Ok(ModelEntrySpec {
                        model,
                        effort: None,
                        alias: None,
                    }),
                    Err(err) => Err(E::custom(err)),
                }
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut model: Option<String> = None;
                let mut effort: Option<ReasoningEffort> = None;
                let mut alias: Option<String> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "model" => model = Some(map.next_value()?),
                        "effort" => {
                            effort = match parse_effort_opt(map.next_value::<Option<String>>()?) {
                                Ok(parsed) => parsed,
                                Err(err) => return Err(serde::de::Error::custom(err)),
                            };
                        }
                        "alias" => alias = Some(map.next_value()?),
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }

                let raw_model = match model {
                    Some(model) => model,
                    None => return Err(serde::de::Error::missing_field("model")),
                };
                let model = match raw_model.parse::<ModelRef>() {
                    Ok(model) => model,
                    Err(err) => return Err(serde::de::Error::custom(err)),
                };
                let alias = alias.and_then(|a| {
                    let trimmed = a.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                });

                Ok(ModelEntrySpec {
                    model,
                    effort,
                    alias,
                })
            }
        }

        deserializer.deserialize_any(SpecVisitor)
    }
}

impl<'de> Deserialize<'de> for ModelEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let spec = ModelEntrySpec::deserialize(deserializer)?;
        Ok(ModelEntry::new(spec.model, spec.effort, spec.alias))
    }
}

impl Serialize for ModelEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if self.effort.is_none() && self.alias.is_none() {
            serializer.serialize_str(&self.model.to_string())
        } else {
            use serde::ser::SerializeStruct;
            let mut count = 1;
            if self.effort.is_some() {
                count += 1;
            }
            if self.alias.is_some() {
                count += 1;
            }
            let mut state = serializer.serialize_struct("ModelEntry", count)?;
            state.serialize_field("model", &self.model.to_string())?;
            if let Some(effort) = &self.effort {
                state.serialize_field("effort", effort)?;
            }
            if let Some(alias) = &self.alias {
                state.serialize_field("alias", alias)?;
            }
            state.end()
        }
    }
}

/// A single rung of the coder escalation ladder.
///
/// Pairs a concrete model with an optional reasoning depth. Deserializes from
/// either a bare model string or a `{ model, effort }` table, so a ladder may
/// mix terse and detailed rungs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoderTier {
    pub model: ModelRef,
    pub effort: Option<ReasoningEffort>,
}

impl CoderTier {
    /// Creates a coder rung from a model and an optional reasoning depth.
    pub fn new(model: ModelRef, effort: Option<ReasoningEffort>) -> Self {
        Self { model, effort }
    }
}

impl From<ModelRef> for CoderTier {
    fn from(model: ModelRef) -> Self {
        Self {
            model,
            effort: None,
        }
    }
}

impl<'de> Deserialize<'de> for CoderTier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let spec = ModelEntrySpec::deserialize(deserializer)?;
        Ok(CoderTier::new(spec.model, spec.effort))
    }
}

impl Serialize for CoderTier {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.effort {
            Some(effort) => {
                use serde::ser::SerializeStruct;
                let mut state = serializer.serialize_struct("CoderTier", 2)?;
                state.serialize_field("model", &self.model.to_string())?;
                state.serialize_field("effort", &effort)?;
                state.end()
            }
            None => serializer.serialize_str(&self.model.to_string()),
        }
    }
}

/// Optional per-role lists of configured model entries.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelRolesEntries {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scribe: Vec<ModelEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coder: Vec<ModelEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub architect: Vec<ModelEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub primary: Vec<ModelEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewer: Vec<ModelEntry>,
}

/// Summary of effective concrete models assigned to each semantic role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRolesSummary {
    pub scribe: ModelRef,
    pub coder: ModelRef,
    pub architect: ModelRef,
    pub primary: ModelRef,
    pub reviewer: ModelRef,
    /// Per-role reasoning-effort overrides, carried so clients can render each
    /// role's reasoning depth without re-reading configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efforts: Option<ModelRolesEffort>,
    /// Per-role configured entry lists backing client-side role selectors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<ModelRolesEntries>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_entry_serde_and_display() {
        let entry_str: ModelEntry = serde_json::from_str("\"openrouter:openai/gpt-4o\"").unwrap();
        assert_eq!(entry_str.model, ModelRef::openrouter("openai/gpt-4o"));
        assert_eq!(entry_str.effort, None);
        assert_eq!(entry_str.alias, None);
        assert_eq!(entry_str.display_label(), "openrouter:openai/gpt-4o");

        let entry_table: ModelEntry = serde_json::from_str(
            r#"{"model":"openrouter:anthropic/claude-sonnet","effort":"high","alias":"Sonnet High"}"#,
        )
        .unwrap();
        assert_eq!(entry_table.model, ModelRef::openrouter("anthropic/claude-sonnet"));
        assert_eq!(entry_table.effort, Some(ReasoningEffort::High));
        assert_eq!(entry_table.alias, Some("Sonnet High".to_string()));
        assert_eq!(entry_table.display_label(), "Sonnet High");

        let serialized_str = serde_json::to_string(&entry_str).unwrap();
        assert_eq!(serialized_str, "\"openrouter:openai/gpt-4o\"");

        let serialized_table = serde_json::to_string(&entry_table).unwrap();
        assert!(serialized_table.contains("\"alias\":\"Sonnet High\""));
    }

    #[test]
    fn test_model_entry_reports_precise_field_errors() {
        // An unrecognized effort surfaces the reasoning-effort diagnostic instead
        // of the opaque "data did not match any variant" of an untagged enum.
        let err = serde_json::from_str::<ModelEntry>(
            r#"{"model":"openrouter:anthropic/claude-sonnet","effort":"extreme"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Unknown reasoning effort"), "got: {err}");

        // A misspelled provider prefix in a bare model string surfaces the
        // provider diagnostic rather than being absorbed into the model name.
        let err = serde_json::from_str::<ModelEntry>(r#"{"model":"anthropc:claude"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unknown model provider"), "got: {err}");
    }

    #[test]
    fn test_coder_tier_shares_entry_spec_parsing() {
        let tier_str: CoderTier = serde_json::from_str("\"openrouter:base/model\"").unwrap();
        assert_eq!(
            tier_str,
            CoderTier::new(ModelRef::openrouter("base/model"), None)
        );

        let tier_table: CoderTier =
            serde_json::from_str(r#"{"model":"openrouter:strong/model","effort":"high"}"#).unwrap();
        assert_eq!(
            tier_table,
            CoderTier::new(
                ModelRef::openrouter("strong/model"),
                Some(ReasoningEffort::High)
            )
        );

        // A bad effort surfaces the precise diagnostic through the shared spec.
        let err = serde_json::from_str::<CoderTier>(
            r#"{"model":"openrouter:strong/model","effort":"nope"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Unknown reasoning effort"), "got: {err}");
    }
}
