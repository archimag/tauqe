use serde::{Deserialize, Serialize};

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
}

/// Presentation form only (`provider:name`); never parse it back.
impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.provider, self.name)
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

        let m2: ModelRef = "openrouter:meta-llama/llama-3.3-70b-instruct:free".parse().unwrap();
        assert_eq!(m2, ModelRef::openrouter("meta-llama/llama-3.3-70b-instruct:free"));

        let m3: ModelRef = "meta-llama/llama-3.3-70b-instruct:free".parse().unwrap();
        assert_eq!(m3, ModelRef::openrouter("meta-llama/llama-3.3-70b-instruct:free"));

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
}
