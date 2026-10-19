use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A single entry of the canonical English locale catalog.
///
/// English is the sole embedded source of truth for interface text (c-1). Each
/// entry pairs the stable machine [`key`](Self::key) with the canonical English
/// phrase and a human-readable description of the semantic intent of the string
/// (for example, a confirmation button in a modal or a bounded-turn phase
/// status). The optional [`max_width`](Self::max_width) records a hard budget in
/// terminal columns so a translation can be validated deterministically without
/// guessing how the glyphs render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocaleCatalogItem {
    /// Stable identifier of the string, used to look up a translation.
    pub key: String,
    /// Canonical English text of the entry.
    pub english: String,
    /// Semantic description of where and how the string is used.
    pub context: String,
    /// Maximum permitted display width in terminal columns, when bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u16>,
}

/// Parameters of a request to synthesize a locale for a target language.
///
/// The client gathers the English catalog and its semantic annotations and asks
/// the daemon to generate a translation through the LLM (c-2). The TUI never
/// contacts a provider directly; it dispatches this payload over JSON-RPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocaleGenerateParams {
    /// Target language requested by the user in free form.
    pub language: String,
    /// Catalog entries to translate, with their semantic context.
    pub items: Vec<LocaleCatalogItem>,
}

/// Result of a locale generation request.
///
/// Carries the requested target language together with the key-to-translation
/// map. A missing key is not an error: the client transparently falls back to
/// the embedded English string (c-5). The map is ordered for stable wire
/// output and deterministic diffs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocaleGenerateResult {
    /// Target language the translations were generated for.
    pub language: String,
    /// Generated translations keyed by the catalog key.
    pub translations: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, max_width: Option<u16>) -> LocaleCatalogItem {
        LocaleCatalogItem {
            key: key.to_string(),
            english: "Confirm".to_string(),
            context: "confirmation button in a modal dialog".to_string(),
            max_width,
        }
    }

    #[test]
    fn test_locale_catalog_item_serde_roundtrip() {
        let bounded = item("modal.confirm", Some(12));
        let json = serde_json::to_string(&bounded).unwrap();
        assert!(json.contains("\"key\":\"modal.confirm\""));
        assert!(json.contains("\"english\":\"Confirm\""));
        assert!(json.contains("\"context\":"));
        assert!(json.contains("\"max_width\":12"));
        assert_eq!(
            serde_json::from_str::<LocaleCatalogItem>(&json).unwrap(),
            bounded
        );

        let unbounded = item("status.done", None);
        let json = serde_json::to_string(&unbounded).unwrap();
        // An absent width budget is omitted from the wire, not serialized null.
        assert!(!json.contains("max_width"));
        assert_eq!(
            serde_json::from_str::<LocaleCatalogItem>(&json).unwrap(),
            unbounded
        );
    }

    #[test]
    fn test_locale_catalog_item_missing_max_width_defaults_to_none() {
        let raw = r#"{"key":"tab.review","english":"Review","context":"tab title"}"#;
        let decoded: LocaleCatalogItem = serde_json::from_str(raw).unwrap();
        assert_eq!(decoded.max_width, None);
    }

    #[test]
    fn test_locale_generate_params_serde_roundtrip() {
        let params = LocaleGenerateParams {
            language: "Quenya".to_string(),
            items: vec![item("modal.confirm", Some(12)), item("tab.review", None)],
        };
        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("\"language\":\"Quenya\""));
        assert!(json.contains("\"items\""));
        assert_eq!(
            serde_json::from_str::<LocaleGenerateParams>(&json).unwrap(),
            params
        );
    }

    #[test]
    fn test_locale_generate_result_serde_roundtrip() {
        let mut translations = BTreeMap::new();
        translations.insert("modal.confirm".to_string(), "Подтвердить".to_string());
        translations.insert("tab.review".to_string(), "Обзор".to_string());
        let result = LocaleGenerateResult {
            language: "Russian".to_string(),
            translations: translations.clone(),
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"language\":\"Russian\""));
        assert!(json.contains("\"translations\""));
        assert!(json.contains("\"modal.confirm\":\"Подтвердить\""));
        let decoded: LocaleGenerateResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, result);
        assert_eq!(decoded.translations, translations);

        // An empty translation set is a valid, lossless result.
        let empty = LocaleGenerateResult {
            language: "German".to_string(),
            translations: BTreeMap::new(),
        };
        assert_eq!(
            serde_json::from_str::<LocaleGenerateResult>(&serde_json::to_string(&empty).unwrap())
                .unwrap(),
            empty
        );
    }
}
