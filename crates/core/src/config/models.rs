use serde::{Deserialize, Serialize};
use tauqe_protocol::{ModelRef, ModelRole, ModelRolesSummary, ModelSelection};

pub const BUILTIN_DEFAULT_SCRIBE_MODEL: &str = "~anthropic/claude-haiku-latest";
pub const BUILTIN_DEFAULT_CODER_MODEL: &str = "~google/gemini-flash-latest";
pub const BUILTIN_DEFAULT_ARCHITECT_MODEL: &str = "~anthropic/claude-sonnet-latest";
pub const BUILTIN_DEFAULT_ADVISOR_MODEL: &str = "~google/gemini-flash-latest";
pub const BUILTIN_DEFAULT_REVIEWER_MODEL: &str = "~anthropic/claude-sonnet-latest";
pub const BUILTIN_DEFAULT_MODEL: &str = BUILTIN_DEFAULT_ADVISOR_MODEL;

pub fn parse_model_ref(s: &str) -> Result<ModelRef, String> {
    let s = s.trim();
    if let Some((provider_str, model_name)) = s.split_once(':') {
        let provider_str = provider_str.trim().to_lowercase();
        let model_name = model_name.trim();
        if provider_str == "openrouter" {
            if model_name.is_empty() {
                return Err("Model name cannot be empty".to_string());
            }
            Ok(ModelRef::openrouter(model_name))
        } else {
            Err(format!("Unsupported provider '{}'", provider_str))
        }
    } else if !s.is_empty() {
        Ok(ModelRef::openrouter(s))
    } else {
        Err("Model specification cannot be empty".to_string())
    }
}

pub fn deserialize_model_ref_opt<'de, D>(deserializer: D) -> Result<Option<ModelRef>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ModelRefRaw {
        Str(String),
        Struct(ModelRef),
    }

    match Option::<ModelRefRaw>::deserialize(deserializer)? {
        None => Ok(None),
        Some(ModelRefRaw::Str(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                parse_model_ref(trimmed).map(Some).map_err(serde::de::Error::custom)
            }
        }
        Some(ModelRefRaw::Struct(m)) => Ok(Some(m)),
    }
}

pub fn serialize_model_ref_opt<S>(opt: &Option<ModelRef>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match opt {
        Some(m) => serializer.serialize_str(&m.to_string()),
        None => serializer.serialize_none(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelsConfig {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub scribe: Option<ModelRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub coder: Option<ModelRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub architect: Option<ModelRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub advisor: Option<ModelRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub reviewer: Option<ModelRef>,
    #[serde(default)]
    pub auto_level_up: bool,
}

impl ModelsConfig {
    /// Validates that at least one semantic role model is configured.
    pub fn validate(&self) -> Result<(), String> {
        if self.scribe.is_none()
            && self.coder.is_none()
            && self.architect.is_none()
            && self.advisor.is_none()
            && self.reviewer.is_none()
        {
            return Err(
                "Invalid models configuration: at least one model role (scribe, coder, architect, advisor, reviewer) must be configured"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Resolves a ModelRole using the deterministic proximity fallback chain within the models configuration.
    pub fn resolve_role(&self, role: ModelRole) -> ModelRef {
        self.resolve_role_with_fallback(role, None)
    }

    /// Resolves a ModelRole with an optional external fallback model (e.g. from configured provider models).
    pub fn resolve_role_with_fallback(
        &self,
        role: ModelRole,
        fallback: Option<ModelRef>,
    ) -> ModelRef {
        match role {
            ModelRole::Scribe => self
                .scribe
                .clone()
                .or_else(|| self.coder.clone())
                .or_else(|| self.advisor.clone())
                .or_else(|| self.architect.clone())
                .or_else(|| self.reviewer.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_SCRIBE_MODEL)),
            ModelRole::Coder => self
                .coder
                .clone()
                .or_else(|| self.architect.clone())
                .or_else(|| self.advisor.clone())
                .or_else(|| self.scribe.clone())
                .or_else(|| self.reviewer.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_CODER_MODEL)),
            ModelRole::Architect => self
                .architect
                .clone()
                .or_else(|| self.advisor.clone())
                .or_else(|| self.reviewer.clone())
                .or_else(|| self.coder.clone())
                .or_else(|| self.scribe.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_ARCHITECT_MODEL)),
            ModelRole::Advisor => self
                .advisor
                .clone()
                .or_else(|| self.architect.clone())
                .or_else(|| self.coder.clone())
                .or_else(|| self.reviewer.clone())
                .or_else(|| self.scribe.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_ADVISOR_MODEL)),
            ModelRole::Reviewer => self
                .reviewer
                .clone()
                .or_else(|| self.architect.clone())
                .or_else(|| self.advisor.clone())
                .or_else(|| self.coder.clone())
                .or_else(|| self.scribe.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_REVIEWER_MODEL)),
        }
    }

    /// Resolves a ModelSelection to a concrete ModelRef.
    pub fn resolve_model(&self, selection: &ModelSelection) -> ModelRef {
        match selection {
            ModelSelection::Role(role) => self.resolve_role(*role),
            ModelSelection::Specific(model) => model.clone(),
        }
    }

    /// Summary of effective concrete models assigned to all semantic roles.
    pub fn roles_summary(&self) -> ModelRolesSummary {
        self.roles_summary_with_fallback(None)
    }

    /// Summary of effective concrete models assigned to all semantic roles with fallback.
    pub fn roles_summary_with_fallback(&self, fallback: Option<ModelRef>) -> ModelRolesSummary {
        ModelRolesSummary {
            scribe: self.resolve_role_with_fallback(ModelRole::Scribe, fallback.clone()),
            coder: self.resolve_role_with_fallback(ModelRole::Coder, fallback.clone()),
            architect: self.resolve_role_with_fallback(ModelRole::Architect, fallback.clone()),
            advisor: self.resolve_role_with_fallback(ModelRole::Advisor, fallback.clone()),
            reviewer: self.resolve_role_with_fallback(ModelRole::Reviewer, fallback),
        }
    }
}
