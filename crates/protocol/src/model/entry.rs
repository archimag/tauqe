use serde::{Deserialize, Serialize};

use super::catalog::ModelCatalogEntry;
use super::effort::{parse_effort_opt, ModelRolesEffort, ReasoningEffort};
use super::reference::ModelRef;

/// A model entry configured for a semantic role.
///
/// Pairs the model a role slot runs on with the runtime parameters of that
/// slot. A display alias is not a role parameter: it describes the model itself
/// and is declared once in the provider catalog (`ModelCatalogEntry`), so the
/// same model is never labelled in two places and every surface resolves a
/// display name from a single source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub model: ModelRef,
    pub effort: Option<ReasoningEffort>,
}

impl ModelEntry {
    /// Creates a role entry from a model and an optional reasoning depth.
    pub fn new(model: ModelRef, effort: Option<ReasoningEffort>) -> Self {
        Self { model, effort }
    }

    /// Renders the entry as the canonical model specification, with the
    /// configured reasoning depth appended when one is set.
    pub fn display_label(&self) -> String {
        match self.effort {
            Some(effort) => format!("{} [{}]", self.model.to_spec(), effort),
            None => self.model.to_spec(),
        }
    }
}

impl From<ModelRef> for ModelEntry {
    fn from(model: ModelRef) -> Self {
        Self {
            model,
            effort: None,
        }
    }
}

/// Shared polymorphic specification for a configured model role entry.
///
/// A configured entry is either a bare model string or a table with a mandatory
/// `model` field and an optional `effort` field. `ModelEntry`, `CoderTier`, and
/// `ArchitectTier` all deserialize through this single type, so the three kinds
/// cannot drift apart.
///
/// The value is dispatched on its runtime shape with a visitor rather than an
/// untagged enum. An untagged enum reports every malformed field as the opaque
/// "data did not match any variant", which discards the precise diagnostics
/// produced while parsing the model string or the reasoning effort; the visitor
/// forwards those messages verbatim.
struct ModelEntrySpec {
    model: ModelRef,
    effort: Option<ReasoningEffort>,
}

impl ModelEntrySpec {
    /// Decodes a role entry, rejecting unknown keys.
    ///
    /// Every unrecognized key is reported as an `unknown field` error rather
    /// than discarded, so a typo such as `efort` surfaces immediately instead
    /// of leaving the entry without the intended reasoning depth. A display
    /// alias belongs to the provider catalog entry of a model, so a role slot
    /// that carries one is rejected as well instead of quietly reinstating a
    /// second source of truth for the same label. A repeated key is reported as
    /// a `duplicate field` error rather than letting the last value silently
    /// win.
    fn deserialize<'de, D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct SpecVisitor;

        impl<'de> serde::de::Visitor<'de> for SpecVisitor {
            type Value = ModelEntrySpec;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a model string or a { model, effort } table")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value.parse::<ModelRef>() {
                    Ok(model) => Ok(ModelEntrySpec {
                        model,
                        effort: None,
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
                        other => {
                            return Err(serde::de::Error::unknown_field(
                                other,
                                &["model", "effort"],
                            ))
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

                Ok(ModelEntrySpec { model, effort })
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
        Ok(ModelEntry::new(spec.model, spec.effort))
    }
}

impl Serialize for ModelEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.effort {
            Some(effort) => {
                use serde::ser::SerializeStruct;
                let mut state = serializer.serialize_struct("ModelEntry", 2)?;
                state.serialize_field("model", &self.model.to_spec())?;
                state.serialize_field("effort", &effort)?;
                state.end()
            }
            None => serializer.serialize_str(&self.model.to_spec()),
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

/// Serializes a `{ model, effort }` rung.
///
/// A rung without a reasoning depth is written as the bare model string, so
/// every rung kind shares a single wire form and the kinds cannot drift apart.
fn serialize_rung<S>(
    model: &ModelRef,
    effort: Option<ReasoningEffort>,
    name: &'static str,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match effort {
        Some(effort) => {
            use serde::ser::SerializeStruct;
            let mut state = serializer.serialize_struct(name, 2)?;
            state.serialize_field("model", &model.to_spec())?;
            state.serialize_field("effort", &effort)?;
            state.end()
        }
        None => serializer.serialize_str(&model.to_spec()),
    }
}

/// The Architect configuration that produced a decomposition.
///
/// A plan and a review finding both record which Architect model, at which
/// reasoning depth, produced their decomposition. Recording the pair makes a
/// later refinement reproducible with the same Architect configuration instead
/// of re-deriving one, and keeps the *What* role's provenance visible in the
/// document itself.
///
/// Deserializes from either a bare model string or a `{ model, effort }` table
/// through the shared entry spec, exactly like [`CoderTier`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitectTier {
    pub model: ModelRef,
    pub effort: Option<ReasoningEffort>,
}

impl ArchitectTier {
    /// Creates an Architect configuration from a model and an optional
    /// reasoning depth.
    pub fn new(model: ModelRef, effort: Option<ReasoningEffort>) -> Self {
        Self { model, effort }
    }
}

impl From<ModelRef> for ArchitectTier {
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
        serialize_rung(&self.model, self.effort, "CoderTier", serializer)
    }
}

impl<'de> Deserialize<'de> for ArchitectTier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let spec = ModelEntrySpec::deserialize(deserializer)?;
        Ok(ArchitectTier::new(spec.model, spec.effort))
    }
}

impl Serialize for ArchitectTier {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_rung(&self.model, self.effort, "ArchitectTier", serializer)
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
    /// Provider catalog backing client-side display names.
    ///
    /// The catalog is the single source of truth for a model's display alias, so
    /// a client renders a name from that declaration instead of deriving one
    /// from the model string. The catalog travels with the role summary because
    /// the summary already reaches every client surface that prints a model
    /// name, so no second request is issued for the same declaration. An absent
    /// or empty catalog is omitted from the wire rather than sent as an empty
    /// list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<Vec<ModelCatalogEntry>>,
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
        assert_eq!(entry_str.display_label(), "openrouter:openai/gpt-4o");

        let entry_table: ModelEntry = serde_json::from_str(
            r#"{"model":"openrouter:anthropic/claude-sonnet","effort":"high"}"#,
        )
        .unwrap();
        assert_eq!(
            entry_table.model,
            ModelRef::openrouter("anthropic/claude-sonnet")
        );
        assert_eq!(entry_table.effort, Some(ReasoningEffort::High));
        assert_eq!(
            entry_table.display_label(),
            "openrouter:anthropic/claude-sonnet [high]"
        );

        let serialized_str = serde_json::to_string(&entry_str).unwrap();
        assert_eq!(serialized_str, "\"openrouter:openai/gpt-4o\"");

        let serialized_table = serde_json::to_string(&entry_table).unwrap();
        assert_eq!(
            serialized_table,
            "{\"model\":\"openrouter:anthropic/claude-sonnet\",\"effort\":\"high\"}"
        );
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
    fn test_architect_tier_shares_entry_spec_parsing() {
        let tier_str: ArchitectTier = serde_json::from_str("\"openrouter:planner/model\"").unwrap();
        assert_eq!(
            tier_str,
            ArchitectTier::new(ModelRef::openrouter("planner/model"), None)
        );

        let tier_table: ArchitectTier =
            serde_json::from_str(r#"{"model":"openrouter:planner/model","effort":"xhigh"}"#)
                .unwrap();
        assert_eq!(
            tier_table,
            ArchitectTier::new(
                ModelRef::openrouter("planner/model"),
                Some(ReasoningEffort::XHigh)
            )
        );

        // A rung without a reasoning depth is written as the bare model string,
        // so the terse and detailed forms round-trip exactly.
        assert_eq!(
            serde_json::to_string(&tier_str).unwrap(),
            "\"openrouter:planner/model\""
        );
        let table_json = serde_json::to_string(&tier_table).unwrap();
        assert_eq!(
            table_json,
            "{\"model\":\"openrouter:planner/model\",\"effort\":\"xhigh\"}"
        );
        let restored: ArchitectTier = serde_json::from_str(&table_json).unwrap();
        assert_eq!(restored, tier_table);

        // A bare model reference promotes into a rung without a depth.
        assert_eq!(
            ArchitectTier::from(ModelRef::openrouter("planner/model")),
            tier_str
        );

        // A bad effort surfaces the precise diagnostic through the shared spec.
        let err = serde_json::from_str::<ArchitectTier>(
            r#"{"model":"openrouter:planner/model","effort":"extreme"}"#,
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
    fn test_role_entries_reject_a_display_alias() {
        // A display alias belongs to the provider catalog entry of a model, so a
        // role slot that carries one is rejected instead of reinstating a second
        // source of truth for the same label.
        for err in [
            serde_json::from_str::<ModelEntry>(r#"{"model":"openrouter:a/b","alias":"Fast"}"#)
                .unwrap_err()
                .to_string(),
            serde_json::from_str::<CoderTier>(r#"{"model":"openrouter:a/b","alias":"Fast"}"#)
                .unwrap_err()
                .to_string(),
            serde_json::from_str::<ArchitectTier>(
                r#"{"model":"openrouter:a/b","alias":"Planner"}"#,
            )
            .unwrap_err()
            .to_string(),
        ] {
            assert!(err.contains("unknown field"), "got: {err}");
            assert!(err.contains("alias"), "got: {err}");
        }
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

        // `CoderTier` shares the same spec, so it rejects duplicates too.
        let err = serde_json::from_str::<CoderTier>(
            r#"{"model":"openrouter:a/b","model":"openrouter:c/d"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate field"), "got: {err}");
    }

    #[test]
    fn test_model_entry_new_keeps_the_model_and_effort_only() {
        let model = ModelRef::openrouter("base/model");

        let entry = ModelEntry::new(model.clone(), Some(ReasoningEffort::Low));
        assert_eq!(entry.model, model);
        assert_eq!(entry.effort, Some(ReasoningEffort::Low));
        assert_eq!(entry.display_label(), "openrouter:base/model [low]");
    }

    #[test]
    fn test_roles_entries_omit_an_absent_catalog() {
        // A summary without a declared catalog carries no extra field on the
        // wire, so a client never distinguishes "absent" from "empty".
        let empty = ModelRolesEntries::default();
        let json = serde_json::to_string(&empty).unwrap();
        assert!(!json.contains("catalog"), "got: {json}");

        let with_catalog = ModelRolesEntries {
            catalog: Some(vec![ModelCatalogEntry {
                model: ModelRef::openrouter("deepseek/deepseek-chat"),
                alias: Some("Deep".to_string()),
            }]),
            ..Default::default()
        };
        let json = serde_json::to_string(&with_catalog).unwrap();
        assert!(json.contains("\"catalog\""), "got: {json}");

        // The declaration round-trips unchanged, so the alias reaches the client
        // exactly as configuration stated it.
        let restored: ModelRolesEntries = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, with_catalog);
    }
}
