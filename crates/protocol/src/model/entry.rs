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
        Self {
            model,
            effort,
            alias: normalize_alias(alias),
        }
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

/// Normalizes an optional alias by trimming surrounding whitespace and
/// collapsing an empty result to `None`.
///
/// Both deserialization and [`ModelEntry::new`] route aliases through this
/// helper, so an entry built by hand carries exactly the same alias as one
/// decoded from configuration and a blank alias is never stored as `Some("")`.
fn normalize_alias(alias: Option<String>) -> Option<String> {
    alias.and_then(|a| {
        let trimmed = a.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

/// Builds the `unknown field` diagnostic for a rejected table key.
///
/// The expected-field list reflects whether `alias` is valid for the entry kind
/// being decoded, so the message names only the fields that kind actually
/// accepts.
fn unknown_field_error<E: serde::de::Error>(field: &str, allow_alias: bool) -> E {
    let expected = if allow_alias {
        "`model`, `effort` or `alias`"
    } else {
        "`model` or `effort`"
    };
    E::custom(format_args!(
        "unknown field `{}`, expected {}",
        field, expected
    ))
}

/// Shared polymorphic specification for a configured model entry.
///
/// A configured entry is either a bare model string or a table with a mandatory
/// `model` field and an optional `effort` field; `alias` is accepted only for
/// entry kinds that carry one. Both `ModelEntry` and `CoderTier` deserialize
/// through this single type.
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

impl ModelEntrySpec {
    /// Decodes a model entry, rejecting unknown keys.
    ///
    /// `allow_alias` gates the optional `alias` field: `ModelEntry` accepts it,
    /// while `CoderTier` does not carry one and therefore rejects it instead of
    /// silently dropping the value. Every other unrecognized key is reported as
    /// an `unknown field` error rather than discarded, so a typo such as `efort`
    /// surfaces immediately instead of leaving the entry without the intended
    /// reasoning depth. A repeated key is reported as a `duplicate field` error
    /// rather than letting the last value silently win.
    fn deserialize_with<'de, D>(deserializer: D, allow_alias: bool) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct SpecVisitor {
            allow_alias: bool,
        }

        impl<'de> serde::de::Visitor<'de> for SpecVisitor {
            type Value = ModelEntrySpec;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                if self.allow_alias {
                    f.write_str("a model string or a { model, effort, alias } table")
                } else {
                    f.write_str("a model string or a { model, effort } table")
                }
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
                let mut effort_seen = false;
                let mut alias: Option<String> = None;
                let mut alias_seen = false;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "model" => {
                            if model.is_some() {
                                return Err(serde::de::Error::duplicate_field("model"));
                            }
                            model = Some(map.next_value()?);
                        }
                        "effort" => {
                            if effort_seen {
                                return Err(serde::de::Error::duplicate_field("effort"));
                            }
                            effort_seen = true;
                            effort = match parse_effort_opt(map.next_value::<Option<String>>()?) {
                                Ok(parsed) => parsed,
                                Err(err) => return Err(serde::de::Error::custom(err)),
                            };
                        }
                        "alias" if self.allow_alias => {
                            if alias_seen {
                                return Err(serde::de::Error::duplicate_field("alias"));
                            }
                            alias_seen = true;
                            // Read as `Option<String>` so an explicit `null`
                            // means "no alias" instead of failing the entry.
                            alias = map.next_value()?;
                        }
                        other => return Err(unknown_field_error(other, self.allow_alias)),
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

                Ok(ModelEntrySpec {
                    model,
                    effort,
                    alias: normalize_alias(alias),
                })
            }
        }

        deserializer.deserialize_any(SpecVisitor { allow_alias })
    }
}

impl<'de> Deserialize<'de> for ModelEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let spec = ModelEntrySpec::deserialize_with(deserializer, true)?;
        Ok(ModelEntry::new(spec.model, spec.effort, spec.alias))
    }
}

impl Serialize for ModelEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if self.effort.is_none() && self.alias.is_none() {
            serializer.serialize_str(&self.model.to_spec())
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
            state.serialize_field("model", &self.model.to_spec())?;
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
        let spec = ModelEntrySpec::deserialize_with(deserializer, false)?;
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
                state.serialize_field("model", &self.model.to_spec())?;
                state.serialize_field("effort", &effort)?;
                state.end()
            }
            None => serializer.serialize_str(&self.model.to_spec()),
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

    #[test]
    fn test_model_entry_rejects_unknown_field() {
        // A misspelled key is reported instead of being silently discarded, so
        // the entry is never left without the intended reasoning depth.
        let err = serde_json::from_str::<ModelEntry>(
            r#"{"model":"openrouter:anthropic/claude-sonnet","efort":"high"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown field"), "got: {err}");
        assert!(err.contains("efort"), "got: {err}");
    }

    #[test]
    fn test_coder_tier_rejects_alias() {
        // `CoderTier` carries no alias; a supplied one is rejected instead of
        // being accepted and silently dropped.
        let err = serde_json::from_str::<CoderTier>(
            r#"{"model":"openrouter:strong/model","alias":"Strong"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown field"), "got: {err}");
        assert!(err.contains("alias"), "got: {err}");
    }

    #[test]
    fn test_model_entry_tolerates_null_alias() {
        // An explicit `null` alias means "no alias", mirroring an absent field,
        // so a null-tolerant producer never makes the entry unreadable.
        let entry: ModelEntry = serde_json::from_str(
            r#"{"model":"openrouter:anthropic/claude-sonnet","alias":null}"#,
        )
        .unwrap();
        assert_eq!(entry.model, ModelRef::openrouter("anthropic/claude-sonnet"));
        assert_eq!(entry.alias, None);
    }

    #[test]
    fn test_model_entry_and_coder_tier_reject_duplicate_keys() {
        // A repeated key is reported instead of letting the last value win.
        let err = serde_json::from_str::<ModelEntry>(
            r#"{"model":"openrouter:a/b","model":"openrouter:c/d"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate field"), "got: {err}");
        assert!(err.contains("model"), "got: {err}");

        let err = serde_json::from_str::<ModelEntry>(
            r#"{"model":"openrouter:a/b","effort":"low","effort":"high"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate field"), "got: {err}");

        let err = serde_json::from_str::<ModelEntry>(
            r#"{"model":"openrouter:a/b","alias":"x","alias":"y"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate field"), "got: {err}");

        // `CoderTier` shares the same spec, so it rejects duplicates too.
        let err = serde_json::from_str::<CoderTier>(
            r#"{"model":"openrouter:a/b","model":"openrouter:c/d"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate field"), "got: {err}");
    }

    #[test]
    fn test_model_entry_new_normalizes_blank_alias() {
        let model = || ModelRef::openrouter("base/model");

        // An empty or whitespace-only alias collapses to `None`, matching the
        // behavior of deserialization.
        assert_eq!(
            ModelEntry::new(model(), None, Some(String::new())).alias,
            None
        );
        assert_eq!(
            ModelEntry::new(model(), None, Some("   ".to_string())).alias,
            None
        );

        // A meaningful alias is trimmed but preserved.
        assert_eq!(
            ModelEntry::new(model(), None, Some("  Sonnet  ".to_string()))
                .alias
                .as_deref(),
            Some("Sonnet")
        );
    }
}
