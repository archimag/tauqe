use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    #[serde(rename = "openrouter")]
    OpenRouter,
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderKind::OpenRouter => f.write_str("openrouter"),
        }
    }
}

/// A model identified by its provider and the provider-native model name.
///
/// The default is an empty OpenRouter reference, serving as the neutral
/// starting value for UI state that has not yet observed a running model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct ModelRef {
    pub provider: ProviderKind,
    pub name: String,
}

impl ModelRef {
    pub fn openrouter(name: impl Into<String>) -> Self {
        Self {
            provider: ProviderKind::OpenRouter,
            name: name.into(),
        }
    }

    /// Leniently parses a model reference from a previously persisted string.
    ///
    /// Unlike the strict [`FromStr`] implementation — which deliberately
    /// rejects a mistyped provider prefix so a configuration typo surfaces
    /// immediately — this parser reads values that were already stored (review
    /// sessions, saved plans). A persisted record must never become unreadable
    /// because an old value predates a stricter rule, so this parser never
    /// fails:
    ///
    /// - an empty or whitespace-only string yields the neutral
    ///   [`ModelRef::default`];
    /// - a well-formed `provider:name` or bare provider-native name parses
    ///   exactly as the strict parser does;
    /// - any other string keeps its raw text as an OpenRouter model name, so
    ///   the original value survives a read/write round-trip instead of being
    ///   silently lost.
    pub fn parse_lenient(s: &str) -> Self {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return ModelRef::default();
        }
        match trimmed.parse::<ModelRef>() {
            Ok(model) => model,
            Err(_) => ModelRef::openrouter(trimmed),
        }
    }

    /// Returns the canonical `provider:name` specification for persistence and
    /// wire transport.
    ///
    /// This is the authoritative text form of a [`ModelRef`]: it is exactly the
    /// string serialization writes and the strict [`FromStr`] implementation
    /// reads back, so `ModelRef::from_str(&model.to_spec()) == Ok(model)` holds
    /// for every value the strict parser can produce. [`std::fmt::Display`]
    /// renders the same text for humans; this method names the round-trip
    /// contract explicitly so persisted and wire values never depend on a
    /// presentation-oriented formatter.
    pub fn to_spec(&self) -> String {
        format!("{}:{}", self.provider, self.name)
    }
}

/// Renders the canonical `provider:name` specification for display.
///
/// The rendered text is identical to [`ModelRef::to_spec`], which is the
/// authoritative form for persistence and wire transport. Use `to_spec` when
/// building persisted or wire values; use `Display` when producing human-facing
/// labels.
impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_spec())
    }
}

/// Parses a model reference from its configuration string form.
///
/// Accepts an explicit `openrouter:` prefix or a bare provider-native name,
/// which defaults to OpenRouter. Surrounding whitespace is ignored.
///
/// A colon in a bare name is ambiguous: OpenRouter appends routing variants
/// such as `:free` to a `vendor/model` identifier, but a colon can also be a
/// mistyped provider separator (`anthropc:claude`). To keep typos from being
/// silently absorbed into the model name, a bare name may carry a colon only
/// when the text before it is a vendor path (contains a `/`) or the text after
/// it is a recognized OpenRouter routing suffix.
impl std::str::FromStr for ModelRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("Model specification cannot be empty".to_string());
        }

        if let Some((prefix, rest)) = s.split_once(':') {
            let provider = prefix.trim().to_lowercase();
            let model_name = rest.trim();

            if provider == "openrouter" {
                if model_name.is_empty() {
                    return Err("Model name cannot be empty".to_string());
                }
                return Ok(ModelRef::openrouter(model_name));
            }

            if !prefix.contains('/') && !is_openrouter_variant_suffix(model_name) {
                return Err(format!(
                    "Unknown model provider prefix '{}' in '{}'. Prefix OpenRouter models with 'openrouter:' or write a bare 'vendor/model' name",
                    provider, s
                ));
            }
        }

        Ok(ModelRef::openrouter(s))
    }
}

/// Returns `true` for OpenRouter routing suffixes appended after a `vendor/model`
/// identifier, such as `:free` or `:nitro`.
///
/// These are the only colon-terminated forms accepted on a bare (provider-less)
/// model specification; any other colon is treated as a mistyped provider
/// separator and rejected.
fn is_openrouter_variant_suffix(suffix: &str) -> bool {
    matches!(
        suffix.trim().to_ascii_lowercase().as_str(),
        "free" | "extended" | "nitro" | "floor" | "online" | "thinking"
    )
}

/// Leniently deserializes a [`ModelRef`] from either a structured JSON object
/// (`{"provider":"openrouter","name":"..."}`) or a legacy string representation
/// (`"openrouter:..."` or a bare provider-native name).
///
/// This runs while reading values that were already persisted, so it obeys
/// Postel's law. A legacy string the strict parser now rejects (for example a
/// mistyped provider prefix) never fails the whole record: it is preserved
/// through [`ModelRef::parse_lenient`]. An explicit `null` degrades to the
/// neutral default, matching an absent field. A dispatched visitor keeps the
/// precise structured-field diagnostic, such as an unknown provider, instead of
/// the opaque "data did not match any variant" that an untagged enum produces.
pub fn deserialize_model_ref_lenient<'de, D>(deserializer: D) -> Result<ModelRef, D::Error>
where
    D: Deserializer<'de>,
{
    struct ModelRefVisitor;

    impl<'de> serde::de::Visitor<'de> for ModelRefVisitor {
        type Value = ModelRef;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a { provider, name } table or a legacy model string")
        }

        fn visit_str<E>(self, value: &str) -> Result<ModelRef, E>
        where
            E: serde::de::Error,
        {
            Ok(ModelRef::parse_lenient(value))
        }

        fn visit_unit<E>(self) -> Result<ModelRef, E>
        where
            E: serde::de::Error,
        {
            Ok(ModelRef::default())
        }

        fn visit_none<E>(self) -> Result<ModelRef, E>
        where
            E: serde::de::Error,
        {
            Ok(ModelRef::default())
        }

        fn visit_map<A>(self, map: A) -> Result<ModelRef, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            // Delegating the structured form to `ModelRef`'s own `Deserialize`
            // keeps its provider validation identical to the strict path while
            // preserving the precise field diagnostic.
            ModelRef::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_any(ModelRefVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_ref_display_and_serde() {
        let model = ModelRef::openrouter("anthropic/claude-3.7-sonnet");
        assert_eq!(model.to_string(), "openrouter:anthropic/claude-3.7-sonnet");

        let json = serde_json::to_string(&model).unwrap();
        let de: ModelRef = serde_json::from_str(&json).unwrap();
        assert_eq!(de, model);
    }

    #[test]
    fn test_model_ref_default() {
        let default = ModelRef::default();
        assert_eq!(default.provider, ProviderKind::OpenRouter);
        assert!(default.name.is_empty());
    }

    #[test]
    fn test_model_ref_from_str() {
        let m1: ModelRef = "openrouter:anthropic/claude-3.7-sonnet".parse().unwrap();
        assert_eq!(m1, ModelRef::openrouter("anthropic/claude-3.7-sonnet"));

        let m2: ModelRef = "openrouter:meta-llama/llama-3.3-70b-instruct:free"
            .parse()
            .unwrap();
        assert_eq!(
            m2,
            ModelRef::openrouter("meta-llama/llama-3.3-70b-instruct:free")
        );

        let m3: ModelRef = "meta-llama/llama-3.3-70b-instruct:free".parse().unwrap();
        assert_eq!(
            m3,
            ModelRef::openrouter("meta-llama/llama-3.3-70b-instruct:free")
        );

        let m4: ModelRef = "anthropic/claude-3.7-sonnet".parse().unwrap();
        assert_eq!(m4, ModelRef::openrouter("anthropic/claude-3.7-sonnet"));

        assert!("".parse::<ModelRef>().is_err());
        assert!("   ".parse::<ModelRef>().is_err());
        assert!("openrouter:".parse::<ModelRef>().is_err());
    }

    #[test]
    fn test_model_ref_rejects_unknown_provider_prefix() {
        // A bare name whose colon segment is neither a vendor path nor a known
        // routing suffix is a mistyped provider prefix, not part of a model name.
        assert!("anthropc:claude".parse::<ModelRef>().is_err());
        assert!("openai:gpt-4".parse::<ModelRef>().is_err());

        // A recognized routing suffix on a vendor path stays valid, with and
        // without the explicit provider prefix.
        assert_eq!(
            "meta-llama/llama-3.3-70b-instruct:nitro"
                .parse::<ModelRef>()
                .unwrap(),
            ModelRef::openrouter("meta-llama/llama-3.3-70b-instruct:nitro")
        );
        assert_eq!(
            "openrouter:meta-llama/llama-3.3-70b-instruct:nitro"
                .parse::<ModelRef>()
                .unwrap(),
            ModelRef::openrouter("meta-llama/llama-3.3-70b-instruct:nitro")
        );
    }

    #[test]
    fn test_model_ref_parse_lenient_never_fails() {
        // A well-formed specification parses exactly as the strict parser does.
        assert_eq!(
            ModelRef::parse_lenient("openrouter:anthropic/claude-3.7-sonnet"),
            ModelRef::openrouter("anthropic/claude-3.7-sonnet")
        );
        assert_eq!(
            ModelRef::parse_lenient("  meta-llama/llama-3.3-70b-instruct:free  "),
            ModelRef::openrouter("meta-llama/llama-3.3-70b-instruct:free")
        );

        // An empty or whitespace-only string yields the neutral default.
        assert_eq!(ModelRef::parse_lenient(""), ModelRef::default());
        assert_eq!(ModelRef::parse_lenient("   "), ModelRef::default());

        // A string the strict parser rejects keeps its raw text instead of
        // being lost or turning the whole persisted record unreadable.
        assert_eq!(
            ModelRef::parse_lenient("anthropc:claude"),
            ModelRef::openrouter("anthropc:claude")
        );
    }

    #[test]
    fn test_model_ref_to_spec_round_trips_through_from_str() {
        // The canonical spec string is exactly what the strict parser reads
        // back, so persistence never depends on a presentation formatter.
        for spec in [
            "openrouter:anthropic/claude-3.7-sonnet",
            "openrouter:meta-llama/llama-3.3-70b-instruct:free",
            "openrouter:meta-llama/llama-3.3-70b-instruct:nitro",
        ] {
            let model: ModelRef = spec.parse().unwrap();
            assert_eq!(model.to_spec(), spec);
            assert_eq!(model.to_spec().parse::<ModelRef>().unwrap(), model);
        }

        // `Display` renders the same canonical spec as `to_spec`.
        let model = ModelRef::openrouter("anthropic/claude-3.7-sonnet");
        assert_eq!(model.to_string(), model.to_spec());
    }
}
