//! Semantic locale generation prompt, structured-output schema, and
//! deterministic terminal-geometry validation.
//!
//! English is the sole embedded source of truth for interface text (concept
//! c-1). A translated catalog is synthesized on demand by a language model
//! (concept c-2), which receives each English string together with its semantic
//! usage description and an optional terminal column budget. Because a model
//! cannot be trusted to measure physical terminal columns, every translated
//! string is then validated deterministically by width in *terminal cells*
//! rather than bytes or scalar values (concept c-3): a string that exceeds its
//! budget is either intelligently shortened with an ellipsis or reverted to the
//! canonical English term, so the terminal grid is never broken.

use std::collections::BTreeMap;

use serde::Deserialize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use tauqe_protocol::{LocaleGenerateParams, LocaleGenerateResult};

use crate::edits::stream::json::decode::{balanced_object_end, strip_code_fence};
use crate::providers::{JsonSchemaDefinition, ResponseFormat};

/// Builds the system prompt for the semantic locale translation role.
///
/// The instructions are stated as operational invariants (semantic fidelity,
/// terminal column budgeting, key preservation, strict JSON output) rather than
/// task-specific examples, so the prompt generalizes across target languages
/// and catalogues.
pub fn build_locale_system_prompt() -> String {
    let mut prompt = String::new();
    prompt.push_str("You are a meticulous software localization engine for a terminal application. You translate canonical English interface strings into the requested target language.\n\n");
    prompt.push_str("## Core Contract\n");
    prompt.push_str("1. Translate meaning, not words. Every entry carries a semantic context describing where the string is used (a modal confirm button, a tab title, a bounded-turn status). Match that register and intent.\n");
    prompt.push_str("2. Stay faithful to the English source: preserve intent and tone, and keep technical identifiers verbatim. Never translate key names, placeholders, or format specifiers.\n");
    prompt.push_str("3. Respect the terminal column budget. When an entry declares a maximum width, the translated text must occupy no more terminal columns than that budget. Terminal columns are neither bytes nor characters: measure the real rendered width and prefer concise wording over literal expansion. Conversely, when max_width_columns is unbounded (e.g. for help text, command descriptions, or explanatory paragraphs), provide a full, natural, idiomatic translation without artificial truncation or telegram-style abbreviation, while maintaining technical precision and clarity.\n");
    prompt.push_str("4. Emit exactly one translation for every requested key, reusing the key string unchanged. Never invent, merge, or omit keys.\n");
    prompt.push_str("5. Respond strictly with the enforced JSON object matching the schema. Do not add prose, code fences, or explanations.\n");
    prompt
}

/// Builds the user prompt that lists the requested target language and every
/// catalog entry to translate, with its semantic context and width budget.
pub fn build_locale_user_prompt(params: &LocaleGenerateParams) -> String {
    let mut prompt = String::new();
    prompt.push_str("## Target Language\n");
    prompt.push_str(params.language.trim());
    prompt.push_str("\n\n## Interface Strings to Translate\n");
    for item in &params.items {
        prompt.push_str("<string>\n");
        prompt.push_str(&format!("key: {}\n", item.key));
        prompt.push_str(&format!("english: {}\n", item.english));
        prompt.push_str(&format!("context: {}\n", item.context));
        match item.max_width {
            Some(width) => prompt.push_str(&format!("max_width_columns: {}\n", width)),
            None => prompt.push_str("max_width_columns: unbounded\n"),
        }
        prompt.push_str("</string>\n");
    }
    prompt
}

/// Strict JSON Schema for the locale translation structured output.
///
/// Locale synthesis never produces code changes, so it is a Structured Output
/// operation (see `crates/core/Conventions.md`, section 1.2). The model returns
/// machine-readable translation entries instead of free-form text recovered by
/// heuristics.
pub fn locale_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "translations": {
                "type": "array",
                "description": "Translated catalog entries, one per requested key",
                "items": {
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Stable catalog key being translated; must match a requested key exactly"
                        },
                        "translation": {
                            "type": "string",
                            "description": "Translated text, kept within the requested terminal column budget"
                        }
                    },
                    "required": ["key", "translation"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["translations"],
        "additionalProperties": false
    })
}

/// Builds the provider [`ResponseFormat`] that enforces the locale schema.
pub fn locale_response_format() -> ResponseFormat {
    ResponseFormat::JsonSchema {
        json_schema: JsonSchemaDefinition {
            name: "locale_translation".to_string(),
            description: Some(
                "Localized interface strings keyed by the canonical catalog keys".to_string(),
            ),
            schema: locale_response_schema(),
            strict: Some(true),
        },
    }
}

/// Model-emitted locale translation payload.
#[derive(Debug, Deserialize)]
struct LocaleModelOutput {
    #[serde(default)]
    translations: Vec<LocaleTranslationEntry>,
}

/// A single model-emitted translation entry before validation.
#[derive(Debug, Deserialize)]
struct LocaleTranslationEntry {
    #[serde(default)]
    key: String,
    #[serde(default)]
    translation: String,
}

/// Parses the model's structured locale output into a key-to-translation map.
///
/// Surrounding markdown code fences and wrapper prose are tolerated. Entries
/// with a blank key are dropped, and a later entry overrides an earlier one for
/// the same key. When no structured payload can be recovered, an empty map is
/// returned so the caller falls back cleanly to English.
pub fn parse_locale_output(raw: &str) -> BTreeMap<String, String> {
    let Some(output) = parse_locale_output_inner(raw) else {
        return BTreeMap::new();
    };
    let mut map = BTreeMap::new();
    for entry in output.translations {
        let key = entry.key.trim();
        if key.is_empty() {
            continue;
        }
        map.insert(key.to_string(), entry.translation);
    }
    map
}

/// Deserializes a locale payload, tolerating code fences and wrapper prose.
fn parse_locale_output_inner(raw: &str) -> Option<LocaleModelOutput> {
    let cleaned = strip_code_fence(raw);
    if let Ok(output) = serde_json::from_str::<LocaleModelOutput>(cleaned) {
        return Some(output);
    }

    let mut from = 0;
    while let Some(relative) = raw[from..].find('{') {
        let start = from + relative;
        if let Some(end) = balanced_object_end(raw, start) {
            if let Ok(output) = serde_json::from_str::<LocaleModelOutput>(&raw[start..end]) {
                return Some(output);
            }
        }
        from = start + 1;
    }
    None
}

/// Deterministically validates and sanitizes a generated locale against the
/// requested catalog.
///
/// Every catalog entry is resolved in the following order (concept c-3):
///
/// 1. A translation that already fits its column budget (or an entry with no
///    budget) is kept as-is.
/// 2. A translation that exceeds its budget is shortened to fit, reserving one
///    cell for an ellipsis (`…`), provided at least half of the original width
///    survives the cut.
/// 3. Otherwise, or when the translation is missing or blank, the canonical
///    English term is used, itself shortened to fit when necessary.
///
/// Keys absent from the catalog are ignored, so the result never carries
/// orphaned entries. Width is always measured in terminal cells, never bytes.
pub fn validate_and_sanitize_locale(
    params: &LocaleGenerateParams,
    translations: &BTreeMap<String, String>,
) -> LocaleGenerateResult {
    let mut sanitized = BTreeMap::new();
    for item in &params.items {
        let chosen = translations
            .get(&item.key)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|value| fit_translation(value, &item.english, item.max_width))
            .unwrap_or_else(|| fit_translation(&item.english, &item.english, item.max_width));
        sanitized.insert(item.key.clone(), chosen);
    }
    LocaleGenerateResult {
        language: params.language.trim().to_string(),
        translations: sanitized,
    }
}

/// Fits a single string within an optional terminal column budget.
fn fit_translation(translation: &str, english: &str, max_width: Option<u16>) -> String {
    let Some(max_width) = max_width else {
        return translation.to_string();
    };
    let budget = max_width as usize;
    let width = UnicodeWidthStr::width(translation);
    if width <= budget {
        return translation.to_string();
    }

    // Reserve one cell for the ellipsis and keep the shortened form only while
    // at least half of the original width survives the cut.
    if budget >= 2 {
        let available = budget - 1;
        let truncated = truncate_to_width(translation, available);
        let retained = UnicodeWidthStr::width(truncated.as_str());
        if retained * 2 >= width {
            let mut ellipsized = truncated;
            ellipsized.push('…');
            return ellipsized;
        }
    }

    truncate_to_width(english, budget)
}

/// Truncates a string to at most `max_width` terminal cells, never splitting a
/// multi-byte scalar or a wide glyph.
fn truncate_to_width(text: &str, max_width: usize) -> String {
    let mut result = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        result.push(ch);
        width += ch_width;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::LocaleCatalogItem;

    fn item(key: &str, english: &str, max_width: Option<u16>) -> LocaleCatalogItem {
        LocaleCatalogItem {
            key: key.to_string(),
            english: english.to_string(),
            context: "confirm button in a modal dialog".to_string(),
            max_width,
        }
    }

    fn params(language: &str, items: Vec<LocaleCatalogItem>) -> LocaleGenerateParams {
        LocaleGenerateParams {
            language: language.to_string(),
            items,
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn keeps_translation_that_fits_the_budget() {
        let p = params("Russian", vec![item("modal.confirm", "Confirm", Some(20))]);
        let result = validate_and_sanitize_locale(&p, &map(&[("modal.confirm", "Подтвердить")]));
        assert_eq!(result.language, "Russian");
        assert_eq!(result.translations["modal.confirm"], "Подтвердить");
    }

    #[test]
    fn truncates_over_budget_translation_with_ellipsis() {
        let p = params("Russian", vec![item("modal.confirm", "Confirm", Some(10))]);
        let result = validate_and_sanitize_locale(&p, &map(&[("modal.confirm", "Подтвердить")]));
        let value = &result.translations["modal.confirm"];
        assert_eq!(value, "Подтверди…");
        assert!(UnicodeWidthStr::width(value.as_str()) <= 10);
    }

    #[test]
    fn falls_back_to_english_when_truncation_loses_too_much() {
        let p = params("Russian", vec![item("modal.ok", "OK", Some(2))]);
        let result = validate_and_sanitize_locale(&p, &map(&[("modal.ok", "Подтверждение")]));
        assert_eq!(result.translations["modal.ok"], "OK");
    }

    #[test]
    fn falls_back_to_english_when_translation_missing_or_blank() {
        let p = params(
            "German",
            vec![item("a", "Cancel", Some(10)), item("b", "Retry", Some(10))],
        );
        let result = validate_and_sanitize_locale(&p, &map(&[("b", "   ")]));
        assert_eq!(result.translations["a"], "Cancel");
        assert_eq!(result.translations["b"], "Retry");
    }

    #[test]
    fn truncates_english_when_it_does_not_fit() {
        let p = params("Russian", vec![item("a", "Confirm", Some(4))]);
        let result = validate_and_sanitize_locale(&p, &BTreeMap::new());
        assert_eq!(result.translations["a"], "Conf");
    }

    #[test]
    fn ignores_keys_absent_from_the_catalog() {
        let p = params("Russian", vec![item("a", "OK", Some(10))]);
        let result =
            validate_and_sanitize_locale(&p, &map(&[("a", "Хорошо"), ("ghost", "Призрак")]));
        assert_eq!(result.translations.len(), 1);
        assert!(!result.translations.contains_key("ghost"));
    }

    #[test]
    fn unbounded_entry_keeps_long_translation() {
        let long = "Очень длинный перевод без ограничения по ширине терминала";
        let p = params("Russian", vec![item("a", "Long", None)]);
        let result = validate_and_sanitize_locale(&p, &map(&[("a", long)]));
        assert_eq!(result.translations["a"], long);
    }

    #[test]
    fn width_is_measured_in_terminal_cells_not_scalars() {
        // Two CJK glyphs occupy four terminal cells.
        let p = params("Japanese", vec![item("a", "OK", Some(4))]);
        let result = validate_and_sanitize_locale(&p, &map(&[("a", "確認")]));
        assert_eq!(result.translations["a"], "確認");

        let p = params("Japanese", vec![item("a", "OK", Some(3))]);
        let result = validate_and_sanitize_locale(&p, &map(&[("a", "確認")]));
        assert_eq!(result.translations["a"], "確…");
        assert!(UnicodeWidthStr::width(result.translations["a"].as_str()) <= 3);
    }

    #[test]
    fn parses_structured_output_tolerating_fences_and_prose() {
        let raw = "```json\n{\"translations\":[{\"key\":\"a\",\"translation\":\"Хорошо\"}]}\n```";
        let parsed = parse_locale_output(raw);
        assert_eq!(parsed["a"], "Хорошо");

        let wrapped =
            "Here you go: {\"translations\":[{\"key\":\"b\",\"translation\":\"Да\"}]} done";
        let parsed = parse_locale_output(wrapped);
        assert_eq!(parsed["b"], "Да");
    }

    #[test]
    fn parse_drops_blank_keys_and_recovers_nothing_from_garbage() {
        let parsed =
            parse_locale_output("{\"translations\":[{\"key\":\"  \",\"translation\":\"x\"}]}");
        assert!(parsed.is_empty());
        assert!(parse_locale_output("not json at all").is_empty());
        assert!(parse_locale_output("").is_empty());
    }

    #[test]
    fn schema_is_strict_and_non_recursive() {
        let schema = locale_response_schema();
        let text = schema.to_string();
        assert!(!text.contains("$ref"));
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        assert_eq!(schema["required"], serde_json::json!(["translations"]));
        let item = &schema["properties"]["translations"]["items"];
        assert_eq!(item["required"], serde_json::json!(["key", "translation"]));
        assert_eq!(item["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn prompts_carry_context_and_width_budget() {
        let p = params("Russian", vec![item("modal.confirm", "Confirm", Some(12))]);
        let system = build_locale_system_prompt();
        assert!(system.contains("terminal column budget"));
        assert!(system.contains("unbounded"));
        let user = build_locale_user_prompt(&p);
        assert!(user.contains("Target Language"));
        assert!(user.contains("Russian"));
        assert!(user.contains("key: modal.confirm"));
        assert!(user.contains("english: Confirm"));
        assert!(user.contains("confirm button in a modal dialog"));
        assert!(user.contains("max_width_columns: 12"));
    }
}
