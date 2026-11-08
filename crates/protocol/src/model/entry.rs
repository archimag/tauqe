use serde::{Deserialize, Serialize};

use super::catalog::ModelCatalogEntry;
use super::effort::{parse_effort_opt, ModelRolesEffort, ReasoningEffort};
use super::reference::ModelRef;

/// Generation-attempt budget of a coder entry that declares none.
///
/// A cheap model usually needs more repair rounds than an expensive one, so the
/// budget belongs to the model entry itself. An entry that omits the field
/// keeps this default, unless it opted out of ladder escalation, where the
/// excluded default applies instead.
const DEFAULT_CODER_ATTEMPTS: u32 = 3;

/// Generation-attempt budget of an Architect tier that declares none.
const DEFAULT_ARCHITECT_ATTEMPTS: u32 = 2;

/// Generation-attempt budget of a role entry that opted out of escalation.
///
/// A model the developer reserved for a manual run (`level_up = false`) is
/// never levelled up automatically, so it runs a single attempt. Granting it
/// the role default would let a repair round silently escalate the very model
/// the developer excluded.
const DEFAULT_EXCLUDED_ATTEMPTS: u32 = 1;

/// Resolves the generation-attempt budget a role entry runs within.
///
/// An explicit budget always wins, so a configured `attempts` keeps priority
/// over every default. An entry that opted out of ladder escalation runs a
/// single attempt; every other entry keeps the default of its own role, which
/// differs between the coder and the architect contours.
fn resolve_attempts(attempts: Option<u32>, level_up: Option<bool>, role_default: u32) -> u32 {
    attempts.unwrap_or(match level_up {
        Some(false) => DEFAULT_EXCLUDED_ATTEMPTS,
        _ => role_default,
    })
}

/// A model entry configured for a semantic role.
///
/// Pairs the model a role slot runs on with the runtime parameters of that
/// slot: an optional reasoning depth, an optional generation-attempt budget,
/// and an optional level-up marker. A
/// display alias is not a role parameter: it describes the model itself and
/// is declared once in the provider catalog (`ModelCatalogEntry`), so the
/// same model is never labelled in two places and every surface resolves a
/// display name from a single source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub model: ModelRef,
    pub effort: Option<ReasoningEffort>,
    /// Explicit generation-attempt budget, absent by default.
    ///
    /// An absent budget is resolved adaptively from the level-up marker: an
    /// entry that participates in escalation falls back to the coder default of
    /// three attempts, while an entry that opted out runs a single attempt. A
    /// role entry is the coder ladder rung whose budget the step executors
    /// read, so the fallback belongs to the entry and every consumer reads one
    /// authoritative value.
    pub attempts: Option<u32>,
    /// Explicit level-up marker, absent by default.
    ///
    /// An absent marker means the entry participates in ladder escalation.
    pub level_up: Option<bool>,
}

impl ModelEntry {
    /// Creates a role entry from a model and an optional reasoning depth.
    ///
    /// The created entry carries neither an attempt budget nor a level-up
    /// marker, so it keeps the coder default budget and participates in ladder
    /// escalation.
    pub fn new(model: ModelRef, effort: Option<ReasoningEffort>) -> Self {
        Self {
            model,
            effort,
            attempts: None,
            level_up: None,
        }
    }

    /// Returns the explicit level-up marker the entry was configured with.
    ///
    /// An absent marker means the entry participates in ladder escalation:
    /// `level_up = true` reserves the entry for an explicit level-up, and
    /// `level_up = false` is the explicit form of that default. Consumers
    /// read the marker through this accessor so the participation rule has
    /// a single authoritative statement.
    pub fn level_up(&self) -> Option<bool> {
        self.level_up
    }

    /// Returns the generation-attempt budget the entry runs within.
    ///
    /// An explicit budget the entry declares wins; an absent one resolves from
    /// the level-up marker, so an entry excluded from escalation runs a single
    /// attempt instead of the coder default. Every consumer reads the value
    /// through this accessor, so the adaptive default has one authoritative
    /// statement instead of a repeated number.
    pub fn attempts(&self) -> u32 {
        resolve_attempts(self.attempts, self.level_up, DEFAULT_CODER_ATTEMPTS)
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
            attempts: None,
            level_up: None,
        }
    }
}

/// Shared polymorphic specification for a configured model role entry.
///
/// A configured entry is either a bare model string or a table with a
/// mandatory `model` field and optional `effort`, `attempts`, and `level_up`
/// fields.
/// `ModelEntry`, `CoderTier`, and `ArchitectTier` all deserialize through
/// this single type, so the three kinds cannot drift apart.
///
/// The value is dispatched on its runtime shape with a visitor rather than an
/// untagged enum. An untagged enum reports every malformed field as the opaque
/// "data did not match any variant", which discards the precise diagnostics
/// produced while parsing the model string or the reasoning effort; the visitor
/// forwards those messages verbatim.
struct ModelEntrySpec {
    model: ModelRef,
    effort: Option<ReasoningEffort>,
    attempts: Option<u32>,
    level_up: Option<bool>,
}

/// Decodes the optional level-up marker from an already-read value.
///
/// Only a genuine boolean literal carries intent. Every other value shape
/// (`null`, a string, a number, an array, a table) degrades to the absent
/// marker instead of failing the entry, so a malformed annotation never
/// renders a configured role unreadable.
fn decode_level_up_marker(raw: serde_json::Value) -> Option<bool> {
    match raw {
        serde_json::Value::Bool(marker) => Some(marker),
        _ => None,
    }
}

/// Decodes the optional generation-attempt budget of a role entry.
///
/// Only a positive integer states a usable budget. Zero is refused instead of
/// accepted, because a model allowed zero attempts could never regenerate a
/// rejected patch, which would turn a configurable budget into a silent
/// disable of the capability.
fn decode_attempts<E>(raw: Option<u32>) -> Result<Option<u32>, E>
where
    E: serde::de::Error,
{
    match raw {
        Some(0) => Err(E::custom(
            "Invalid model attempts: the generation budget must be at least 1",
        )),
        budget => Ok(budget),
    }
}

impl ModelEntrySpec {
    /// Decodes a role entry, rejecting unknown keys.
    ///
    /// Every unrecognized key is reported as an `unknown field` error rather
    /// than discarded, so a typo such as `efort` surfaces immediately instead
    /// of leaving the entry without the intended reasoning depth. A display
    /// alias belongs to the provider catalog entry of a model, so a role slot
    /// that carries one is rejected as well instead of quietly reinstating a
    /// second source of truth for the same label. A repeated key is reported
    /// as a `duplicate field` error rather than letting the last value
    /// silently win. The level-up marker alone decodes leniently: a value that
    /// is not a boolean literal degrades to the absent marker.
    fn deserialize<'de, D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct SpecVisitor;

        impl<'de> serde::de::Visitor<'de> for SpecVisitor {
            type Value = ModelEntrySpec;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a model string or a { model, effort, attempts, level_up } table")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value.parse::<ModelRef>() {
                    Ok(model) => Ok(ModelEntrySpec {
                        model,
                        effort: None,
                        attempts: None,
                        level_up: None,
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
                let mut attempts: Option<u32> = None;
                let mut attempts_seen = false;
                let mut level_up: Option<bool> = None;
                let mut level_up_seen = false;

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
                        "attempts" => {
                            if attempts_seen {
                                return Err(serde::de::Error::duplicate_field("attempts"));
                            }
                            attempts_seen = true;
                            attempts = decode_attempts(map.next_value::<Option<u32>>()?)?;
                        }
                        "level_up" => {
                            if level_up_seen {
                                return Err(serde::de::Error::duplicate_field("level_up"));
                            }
                            level_up_seen = true;
                            level_up = decode_level_up_marker(map.next_value()?);
                        }
                        other => {
                            return Err(serde::de::Error::unknown_field(
                                other,
                                &["model", "effort", "attempts", "level_up"],
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

                Ok(ModelEntrySpec {
                    model,
                    effort,
                    attempts,
                    level_up,
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
        Ok(ModelEntry {
            model: spec.model,
            effort: spec.effort,
            attempts: spec.attempts,
            level_up: spec.level_up,
        })
    }
}

impl Serialize for ModelEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_rung(
            &self.model,
            self.effort,
            self.attempts,
            self.level_up,
            "ModelEntry",
            serializer,
        )
    }
}

/// A single rung of the coder escalation ladder.
///
/// Pairs a concrete model with an optional reasoning depth, an optional
/// generation-attempt budget, and an optional level-up marker. Deserializes
/// from either a bare model string or a `{ model, effort, attempts, level_up }`
/// table, so a ladder may mix terse and detailed rungs. A rung without the
/// marker participates in escalation, and a rung without the budget keeps the
/// coder default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoderTier {
    pub model: ModelRef,
    pub effort: Option<ReasoningEffort>,
    /// Explicit generation-attempt budget, absent by default.
    ///
    /// An absent budget resolves adaptively from the level-up marker: a rung
    /// that participates in escalation keeps the coder default, while a rung
    /// that opted out runs a single attempt.
    pub attempts: Option<u32>,
    /// Explicit level-up marker, absent by default.
    ///
    /// An absent marker means the rung participates in ladder escalation.
    pub level_up: Option<bool>,
}

impl CoderTier {
    /// Creates a coder rung from a model and an optional reasoning depth.
    ///
    /// The created rung carries neither an attempt budget nor a level-up
    /// marker, so it keeps the coder default budget and participates in ladder
    /// escalation.
    pub fn new(model: ModelRef, effort: Option<ReasoningEffort>) -> Self {
        Self {
            model,
            effort,
            attempts: None,
            level_up: None,
        }
    }

    /// Returns the explicit level-up marker the rung was configured with.
    ///
    /// An absent marker means the rung participates in ladder escalation:
    /// `level_up = true` reserves the rung for an explicit level-up, and
    /// `level_up = false` is the explicit form of that default.
    pub fn level_up(&self) -> Option<bool> {
        self.level_up
    }

    /// Returns the generation-attempt budget the rung runs within.
    ///
    /// A budget the rung declares wins; an absent one resolves from the
    /// level-up marker, so escalation always knows how many attempts the next
    /// tier grants without consulting configuration again.
    pub fn attempts(&self) -> u32 {
        resolve_attempts(self.attempts, self.level_up, DEFAULT_CODER_ATTEMPTS)
    }
}

impl From<ModelRef> for CoderTier {
    fn from(model: ModelRef) -> Self {
        Self {
            model,
            effort: None,
            attempts: None,
            level_up: None,
        }
    }
}

/// Serializes a `{ model, effort, attempts, level_up }` rung.
///
/// A rung with no runtime parameter at all is written as the bare model
/// string, so every rung kind shares a single wire form and the kinds cannot
/// drift apart.
fn serialize_rung<S>(
    model: &ModelRef,
    effort: Option<ReasoningEffort>,
    attempts: Option<u32>,
    level_up: Option<bool>,
    name: &'static str,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match (effort, attempts, level_up) {
        (None, None, None) => serializer.serialize_str(&model.to_spec()),
        _ => {
            use serde::ser::SerializeStruct;
            let mut state = serializer.serialize_struct(name, 4)?;
            state.serialize_field("model", &model.to_spec())?;
            if let Some(effort) = effort {
                state.serialize_field("effort", &effort)?;
            }
            if let Some(attempts) = attempts {
                state.serialize_field("attempts", &attempts)?;
            }
            if let Some(level_up) = level_up {
                state.serialize_field("level_up", &level_up)?;
            }
            state.end()
        }
    }
}

/// The Architect configuration that produced a decomposition.
///
/// A plan and a review finding both record which Architect model, at which
/// reasoning depth, produced their decomposition. Recording the pair makes a
/// later refinement reproducible with the same Architect configuration
/// instead of re-deriving one, and keeps the *What* role's provenance visible
/// in the document itself.
///
/// Deserializes from either a bare model string or a `{ model, effort,
/// attempts, level_up }` table through the shared entry spec, exactly like
/// [`CoderTier`]. A tier without the marker participates in escalation, and a
/// tier without the budget keeps the Architect default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitectTier {
    pub model: ModelRef,
    pub effort: Option<ReasoningEffort>,
    /// Explicit generation-attempt budget, absent by default.
    ///
    /// An absent budget resolves adaptively from the level-up marker: a tier
    /// that participates in escalation keeps the Architect default, which is
    /// lower than the coder default because a plan synthesis is expected to
    /// converge in fewer rounds, while a tier that opted out runs a single
    /// attempt.
    pub attempts: Option<u32>,
    /// Explicit level-up marker, absent by default.
    ///
    /// An absent marker means the tier participates in ladder escalation.
    pub level_up: Option<bool>,
}

impl ArchitectTier {
    /// Creates an Architect configuration from a model and an optional
    /// reasoning depth.
    ///
    /// The created tier carries neither an attempt budget nor a level-up
    /// marker, so it keeps the Architect default budget and participates in
    /// ladder escalation.
    pub fn new(model: ModelRef, effort: Option<ReasoningEffort>) -> Self {
        Self {
            model,
            effort,
            attempts: None,
            level_up: None,
        }
    }

    /// Returns the explicit level-up marker the tier was configured with.
    ///
    /// An absent marker means the tier participates in ladder escalation:
    /// `level_up = true` reserves the tier for an explicit level-up, and
    /// `level_up = false` is the explicit form of that default.
    pub fn level_up(&self) -> Option<bool> {
        self.level_up
    }

    /// Returns the generation-attempt budget the tier runs within.
    ///
    /// A budget the tier declares wins; an absent one resolves from the
    /// level-up marker, so every synthesis round starts from the budget of the
    /// tier that actually produced the decomposition.
    pub fn attempts(&self) -> u32 {
        resolve_attempts(self.attempts, self.level_up, DEFAULT_ARCHITECT_ATTEMPTS)
    }
}

impl From<ModelRef> for ArchitectTier {
    fn from(model: ModelRef) -> Self {
        Self {
            model,
            effort: None,
            attempts: None,
            level_up: None,
        }
    }
}

impl<'de> Deserialize<'de> for CoderTier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let spec = ModelEntrySpec::deserialize(deserializer)?;
        Ok(CoderTier {
            model: spec.model,
            effort: spec.effort,
            attempts: spec.attempts,
            level_up: spec.level_up,
        })
    }
}

impl Serialize for CoderTier {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_rung(
            &self.model,
            self.effort,
            self.attempts,
            self.level_up,
            "CoderTier",
            serializer,
        )
    }
}

impl<'de> Deserialize<'de> for ArchitectTier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let spec = ModelEntrySpec::deserialize(deserializer)?;
        Ok(ArchitectTier {
            model: spec.model,
            effort: spec.effort,
            attempts: spec.attempts,
            level_up: spec.level_up,
        })
    }
}

impl Serialize for ArchitectTier {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_rung(
            &self.model,
            self.effort,
            self.attempts,
            self.level_up,
            "ArchitectTier",
            serializer,
        )
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
    pub repl: Vec<ModelEntry>,
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
    pub repl: ModelRef,
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
        assert_eq!(entry.level_up(), None);
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

    #[test]
    fn test_level_up_marker_decodes_leniently() {
        // An entry without the marker participates in ladder escalation.
        let plain: ModelEntry =
            serde_json::from_str(r#"{"model":"openrouter:a/b","effort":"high"}"#).unwrap();
        assert_eq!(plain.level_up(), None);

        // A genuine boolean literal carries intent and round-trips.
        let marked: ModelEntry =
            serde_json::from_str(r#"{"model":"openrouter:a/b","level_up":true}"#).unwrap();
        assert_eq!(marked.level_up(), Some(true));
        let marked_json = serde_json::to_string(&marked).unwrap();
        assert_eq!(marked_json, r#"{"model":"openrouter:a/b","level_up":true}"#);
        assert_eq!(
            serde_json::from_str::<ModelEntry>(&marked_json).unwrap(),
            marked
        );

        // An explicit false is the explicit form of the default and survives
        // a round-trip.
        let explicit: ModelEntry =
            serde_json::from_str(r#"{"model":"openrouter:a/b","level_up":false}"#).unwrap();
        assert_eq!(explicit.level_up(), Some(false));
        assert_eq!(
            serde_json::to_string(&explicit).unwrap(),
            r#"{"model":"openrouter:a/b","level_up":false}"#
        );

        // Every other value shape degrades to the absent marker instead of
        // failing the entry.
        for raw in [
            r#"{"model":"openrouter:a/b","level_up":null}"#,
            r#"{"model":"openrouter:a/b","level_up":"true"}"#,
            r#"{"model":"openrouter:a/b","level_up":1}"#,
            r#"{"model":"openrouter:a/b","level_up":[true]}"#,
            r#"{"model":"openrouter:a/b","level_up":{"set":true}}"#,
        ] {
            let entry: ModelEntry = serde_json::from_str(raw).unwrap();
            assert_eq!(entry.level_up(), None, "got: {raw}");
        }
    }

    #[test]
    fn test_ladder_rungs_carry_the_level_up_marker() {
        // Coder and Architect rungs deserialize through the shared entry spec,
        // so the marker travels with both wire forms.
        let tier: CoderTier = serde_json::from_str(
            r#"{"model":"openrouter:strong/model","effort":"high","level_up":true}"#,
        )
        .unwrap();
        assert_eq!(tier.level_up(), Some(true));
        let tier_json = serde_json::to_string(&tier).unwrap();
        assert_eq!(
            tier_json,
            r#"{"model":"openrouter:strong/model","effort":"high","level_up":true}"#
        );
        assert_eq!(serde_json::from_str::<CoderTier>(&tier_json).unwrap(), tier);

        let architect: ArchitectTier =
            serde_json::from_str(r#"{"model":"openrouter:planner/model","level_up":true}"#)
                .unwrap();
        assert_eq!(architect.level_up(), Some(true));
        assert_eq!(
            serde_json::to_string(&architect).unwrap(),
            r#"{"model":"openrouter:planner/model","level_up":true}"#
        );

        // A rung without a marker keeps the terse bare-string wire form.
        let bare: CoderTier = serde_json::from_str("\"openrouter:base/model\"").unwrap();
        assert_eq!(bare.level_up(), None);
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            "\"openrouter:base/model\""
        );

        // Promotion from a bare model reference yields an unmarked rung.
        assert_eq!(
            ModelEntry::from(ModelRef::openrouter("primary/model")).level_up(),
            None
        );
        assert_eq!(
            CoderTier::from(ModelRef::openrouter("base/model")).level_up(),
            None
        );
        assert_eq!(
            ArchitectTier::from(ModelRef::openrouter("planner/model")).level_up(),
            None
        );
    }

    #[test]
    fn test_level_up_rejects_duplicate_keys() {
        // A repeated marker is reported instead of letting the last value win.
        let err = serde_json::from_str::<ModelEntry>(
            r#"{"model":"openrouter:a/b","level_up":true,"level_up":false}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate field"), "got: {err}");
        assert!(err.contains("level_up"), "got: {err}");
    }
    #[test]
    fn test_attempts_default_to_the_role_budget() {
        // A bare model string declares no budget, so each role default applies.
        let entry: ModelEntry = serde_json::from_str("\"openrouter:base/model\"").unwrap();
        assert_eq!(entry.attempts, None);
        assert_eq!(entry.attempts(), 3);
        assert_eq!(
            CoderTier::from(ModelRef::openrouter("base/model")).attempts(),
            3
        );
        assert_eq!(
            ArchitectTier::from(ModelRef::openrouter("planner/model")).attempts(),
            2
        );
    }

    #[test]
    fn test_attempts_round_trip_through_every_role_entry() {
        let entry: ModelEntry =
            serde_json::from_str(r#"{"model":"openrouter:cheap/model","attempts":5}"#).unwrap();
        assert_eq!(entry.attempts, Some(5));
        assert_eq!(entry.attempts(), 5);
        let entry_json = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            entry_json,
            r#"{"model":"openrouter:cheap/model","attempts":5}"#
        );
        assert_eq!(
            serde_json::from_str::<ModelEntry>(&entry_json).unwrap(),
            entry
        );

        let coder: CoderTier = serde_json::from_str(
            r#"{"model":"openrouter:cheap/model","effort":"low","attempts":5}"#,
        )
        .unwrap();
        assert_eq!(coder.attempts(), 5);
        assert_eq!(
            serde_json::to_string(&coder).unwrap(),
            r#"{"model":"openrouter:cheap/model","effort":"low","attempts":5}"#
        );

        let architect: ArchitectTier =
            serde_json::from_str(r#"{"model":"openrouter:planner/model","attempts":1}"#).unwrap();
        assert_eq!(architect.attempts(), 1);
        assert_eq!(
            serde_json::to_string(&architect).unwrap(),
            r#"{"model":"openrouter:planner/model","attempts":1}"#
        );

        // A rung that states only the marker keeps its previous wire form, so
        // the budget never widens an entry that does not declare one.
        let marked: CoderTier = serde_json::from_str(
            r#"{"model":"openrouter:strong/model","effort":"high","level_up":true}"#,
        )
        .unwrap();
        assert_eq!(marked.attempts(), 3);
        assert_eq!(
            serde_json::to_string(&marked).unwrap(),
            r#"{"model":"openrouter:strong/model","effort":"high","level_up":true}"#
        );
    }

    #[test]
    fn test_zero_attempts_is_rejected_for_every_role_entry() {
        // A model allowed zero attempts could never regenerate a rejected
        // patch, so the budget is refused instead of silently disabling it.
        for err in [
            serde_json::from_str::<ModelEntry>(r#"{"model":"openrouter:a/b","attempts":0}"#)
                .unwrap_err()
                .to_string(),
            serde_json::from_str::<CoderTier>(r#"{"model":"openrouter:a/b","attempts":0}"#)
                .unwrap_err()
                .to_string(),
            serde_json::from_str::<ArchitectTier>(r#"{"model":"openrouter:a/b","attempts":0}"#)
                .unwrap_err()
                .to_string(),
        ] {
            assert!(err.contains("attempts"), "got: {err}");
            assert!(err.contains("at least 1"), "got: {err}");
        }

        // A negative or non-numeric budget never decodes into a budget at all.
        assert!(
            serde_json::from_str::<ModelEntry>(r#"{"model":"openrouter:a/b","attempts":-1}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<ModelEntry>(r#"{"model":"openrouter:a/b","attempts":"3"}"#)
                .is_err()
        );
    }

    #[test]
    fn test_attempts_rejects_duplicate_keys() {
        // A repeated budget is reported instead of letting the last value win.
        let err = serde_json::from_str::<ModelEntry>(
            r#"{"model":"openrouter:a/b","attempts":1,"attempts":2}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate field"), "got: {err}");
        assert!(err.contains("attempts"), "got: {err}");
    }

    #[test]
    fn test_attempts_adapt_to_the_level_up_marker() {
        // An entry that participates in escalation keeps the default of its
        // role, so an unmarked declaration behaves exactly as before.
        let entry: ModelEntry = serde_json::from_str("\"openrouter:a/b\"").unwrap();
        assert_eq!(entry.level_up(), None);
        assert_eq!(entry.attempts(), 3);

        let coder: CoderTier = serde_json::from_str("\"openrouter:a/b\"").unwrap();
        assert_eq!(coder.attempts(), 3);
        let architect: ArchitectTier = serde_json::from_str("\"openrouter:a/b\"").unwrap();
        assert_eq!(architect.attempts(), 2);

        // An explicit `true` is the explicit form of the same default.
        let marked: CoderTier =
            serde_json::from_str(r#"{"model":"openrouter:a/b","level_up":true}"#).unwrap();
        assert_eq!(marked.attempts(), 3);
        let marked: ArchitectTier =
            serde_json::from_str(r#"{"model":"openrouter:a/b","level_up":true}"#).unwrap();
        assert_eq!(marked.attempts(), 2);
    }

    #[test]
    fn test_attempts_of_an_opted_out_entry_default_to_one() {
        // A model reserved for a manual run is never levelled up, so it runs a
        // single attempt instead of the budget of a ladder rung.
        let entry: ModelEntry =
            serde_json::from_str(r#"{"model":"openrouter:a/b","level_up":false}"#).unwrap();
        assert_eq!(entry.attempts(), DEFAULT_EXCLUDED_ATTEMPTS);
        assert_eq!(entry.attempts(), 1);

        let coder: CoderTier =
            serde_json::from_str(r#"{"model":"openrouter:a/b","level_up":false}"#).unwrap();
        assert_eq!(coder.attempts(), 1);

        let architect: ArchitectTier =
            serde_json::from_str(r#"{"model":"openrouter:a/b","level_up":false}"#).unwrap();
        assert_eq!(architect.attempts(), 1);

        // The constructors keep the marker absent, so a promoted reference is
        // unaffected by the adaptive default.
        assert_eq!(
            ArchitectTier::new(ModelRef::openrouter("a/b"), None).attempts(),
            2
        );
        assert_eq!(
            CoderTier::new(ModelRef::openrouter("a/b"), None).attempts(),
            3
        );
    }

    #[test]
    fn test_explicit_attempts_win_over_the_adaptive_default() {
        // An explicit budget keeps unconditional priority, so a model excluded
        // from escalation may still be granted the number of rounds the
        // developer chose for it.
        let entry: ModelEntry =
            serde_json::from_str(r#"{"model":"openrouter:a/b","level_up":false,"attempts":4}"#)
                .unwrap();
        assert_eq!(entry.attempts(), 4);

        let coder: CoderTier =
            serde_json::from_str(r#"{"model":"openrouter:a/b","level_up":false,"attempts":2}"#)
                .unwrap();
        assert_eq!(coder.attempts(), 2);

        let architect: ArchitectTier =
            serde_json::from_str(r#"{"model":"openrouter:a/b","level_up":false,"attempts":5}"#)
                .unwrap();
        assert_eq!(architect.attempts(), 5);
    }
}
