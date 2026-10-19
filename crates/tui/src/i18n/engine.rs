//! Runtime localization engine with deterministic width validation.
//!
//! English is the sole embedded source of truth (c-1); the engine resolves
//! interface strings from the canonical catalog and overlays at most one custom
//! locale (c-4). Every resolved string is validated against the catalog's
//! terminal-column budget using `unicode-width`, so a translation that would
//! corrupt the terminal grid silently falls through to its canonical English
//! original (c-3). Missing keys, blank translations, and an absent or corrupt
//! custom locale degrade transparently to English, keeping the interface fully
//! operational (c-5).

use std::collections::HashMap;
use std::sync::LazyLock;

#[cfg(not(test))]
use std::sync::RwLock;

use unicode_width::UnicodeWidthStr;

use super::catalog::canonical_catalog;
use super::storage::CustomLocale;

/// Canonical English string together with its optional terminal-column budget.
struct CatalogEntry {
    english: String,
    max_width: Option<u16>,
}

/// Process-wide cache of the embedded English catalog.
///
/// Built once from [`canonical_catalog`] and reused by every engine instance,
/// every global snapshot installation, and every `english_default` fallback, so
/// a lookup never rebuilds the multi-hundred-entry catalog vector and its
/// thousands of heap-allocated strings.
static EN_CATALOG: LazyLock<HashMap<String, CatalogEntry>> = LazyLock::new(|| {
    canonical_catalog()
        .into_iter()
        .map(|item| {
            (
                item.key,
                CatalogEntry {
                    english: item.english,
                    max_width: item.max_width,
                },
            )
        })
        .collect()
});

/// Resolves interface strings against the embedded English catalog and the
/// optional active custom locale, validating each translation's terminal width.
pub struct I18nEngine {
    custom: Option<CustomLocale>,
}

impl I18nEngine {
    /// Builds the engine, seeding the canonical catalog and activating the
    /// supplied custom locale when it carries a non-blank language name.
    pub fn new(custom: Option<CustomLocale>) -> Self {
        Self {
            custom: normalize_custom(custom),
        }
    }

    /// Builds the engine locked to the canonical English locale.
    pub fn english() -> Self {
        Self::new(None)
    }

    /// Active custom locale language name, or `None` while English is active.
    pub fn language(&self) -> Option<&str> {
        self.custom.as_ref().map(|locale| locale.language.as_str())
    }

    /// Whether a custom locale is currently active.
    pub fn is_custom(&self) -> bool {
        self.custom.is_some()
    }

    /// Resolves the interface string for `key`.
    ///
    /// A translation that exceeds its column budget, is blank, or is missing
    /// falls through to the canonical English original. An unknown key yields
    /// an empty string rather than panicking.
    pub fn t(&self, key: &str) -> &str {
        let Some(entry) = EN_CATALOG.get(key) else {
            return "";
        };
        if let Some(locale) = &self.custom {
            if let Some(translation) = locale.translations.get(key) {
                if !translation.trim().is_empty() && fits(translation, entry.max_width) {
                    return translation.as_str();
                }
            }
        }
        entry.english.as_str()
    }

    /// Hot-swaps the active locale without a restart (c-5).
    ///
    /// A blank language name deactivates the custom locale instead of installing
    /// one, so an empty selection deterministically reverts to English.
    pub fn apply_custom_locale(&mut self, locale: CustomLocale) {
        self.custom = normalize_custom(Some(locale));
    }

    /// Deactivates the custom locale, reverting to embedded English.
    pub fn reset_locale_to_english(&mut self) {
        self.custom = None;
    }
}

/// Process-wide snapshot of the resolved active locale, mirroring the theme
/// engine's global projection (`crate::theme::engine`).
///
/// Presentation modules that render without an `&AppState` — the shared modal
/// footer vocabulary — resolve interface strings from this one source, so a
/// hot-swapped locale takes effect everywhere without threading the engine
/// through every render call. English is the transparent fallback while no
/// locale has been installed (c-1, c-5).
#[cfg(not(test))]
static ACTIVE_LOCALE: RwLock<Option<HashMap<String, String>>> = RwLock::new(None);

#[cfg(test)]
thread_local! {
    /// Per-thread replacement of the process-wide snapshot, active only in the
    /// crate's own test build.
    ///
    /// The test harness runs every `#[test]` in its own thread inside one
    /// shared process, so a single global snapshot lets a concurrently running
    /// test overwrite the locale another test has just installed. Scoping the
    /// test snapshot to the thread keeps each test's installation private,
    /// while the production build keeps the single process-wide projection.
    static ACTIVE_LOCALE: std::cell::RefCell<Option<HashMap<String, String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Publishes the resolved string snapshot of `engine` as the process-wide active
/// locale. Called whenever the active locale changes, keeping the global view in
/// step with the `AppState` engine (c-4, c-5).
pub fn install_active_locale(engine: &I18nEngine) {
    let snapshot: HashMap<String, String> = EN_CATALOG
        .keys()
        .map(|key| (key.clone(), engine.t(key).to_string()))
        .collect();
    publish_snapshot(snapshot);
}

#[cfg(not(test))]
fn publish_snapshot(snapshot: HashMap<String, String>) {
    if let Ok(mut guard) = ACTIVE_LOCALE.write() {
        *guard = Some(snapshot);
    }
}

#[cfg(test)]
fn publish_snapshot(snapshot: HashMap<String, String>) {
    let _ = ACTIVE_LOCALE.try_with(move |cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            *guard = Some(snapshot);
        }
    });
}

/// Resolves `key` against the process-wide active locale, degrading to the
/// embedded English catalog when no locale has been installed (c-5).
pub fn active_t(key: &str) -> String {
    lookup_snapshot(key).unwrap_or_else(|| english_default(key))
}

#[cfg(not(test))]
fn lookup_snapshot(key: &str) -> Option<String> {
    let guard = ACTIVE_LOCALE.read().ok()?;
    let snapshot = guard.as_ref()?;
    Some(snapshot.get(key).cloned().unwrap_or_default())
}

#[cfg(test)]
fn lookup_snapshot(key: &str) -> Option<String> {
    ACTIVE_LOCALE.with(|cell| {
        let guard = cell.borrow();
        let snapshot = guard.as_ref()?;
        Some(snapshot.get(key).cloned().unwrap_or_default())
    })
}

/// Canonical English text for `key`, the transparent fallback used before any
/// locale has been installed.
///
/// Resolves through the process-wide [`EN_CATALOG`] cache, so the fallback is a
/// single hash lookup rather than a full catalog rebuild on every call.
fn english_default(key: &str) -> String {
    EN_CATALOG
        .get(key)
        .map(|entry| entry.english.clone())
        .unwrap_or_default()
}

fn normalize_custom(custom: Option<CustomLocale>) -> Option<CustomLocale> {
    custom.filter(|locale| !locale.language.trim().is_empty())
}

fn fits(text: &str, max_width: Option<u16>) -> bool {
    match max_width {
        Some(limit) => UnicodeWidthStr::width(text) <= usize::from(limit),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn custom(language: &str, pairs: &[(&str, &str)]) -> CustomLocale {
        let mut translations = BTreeMap::new();
        for (key, value) in pairs {
            translations.insert((*key).to_string(), (*value).to_string());
        }
        CustomLocale::new(language, translations)
    }

    #[test]
    fn english_engine_returns_canonical_text() {
        let engine = I18nEngine::english();
        assert_eq!(engine.t("tab.repl"), "REPL");
        assert_eq!(engine.t("modal.confirm"), "Confirm");
        assert!(!engine.is_custom());
        assert!(engine.language().is_none());
    }

    #[test]
    fn translation_within_width_is_applied() {
        let engine = I18nEngine::new(Some(custom("Russian", &[("tab.repl", "Разраб")])));
        assert_eq!(engine.t("tab.repl"), "Разраб");
        assert!(engine.is_custom());
        assert_eq!(engine.language(), Some("Russian"));
    }

    #[test]
    fn over_width_translation_falls_back_to_english() {
        let engine = I18nEngine::new(Some(custom(
            "Russian",
            &[("status.done", "завершено полностью")],
        )));
        assert_eq!(engine.t("status.done"), "Done");
    }

    #[test]
    fn missing_key_falls_back_to_english() {
        let engine = I18nEngine::new(Some(custom(
            "German",
            &[("modal.confirm", "Bestätigen")],
        )));
        assert_eq!(engine.t("modal.confirm"), "Bestätigen");
        assert_eq!(engine.t("tab.context"), "Context");
    }

    #[test]
    fn empty_translation_falls_back_to_english() {
        let engine = I18nEngine::new(Some(custom("Russian", &[("tab.repl", "   ")])));
        assert_eq!(engine.t("tab.repl"), "REPL");
    }

    #[test]
    fn width_is_measured_in_terminal_cells_not_scalars() {
        // Six CJK scalars occupy twelve columns, exceeding the ten-column budget
        // of `status.done`, so the engine falls back to English.
        let engine = I18nEngine::new(Some(custom(
            "Japanese",
            &[("status.done", "完了完了完了")],
        )));
        assert_eq!(engine.t("status.done"), "Done");

        // Three CJK scalars occupy six columns, well within `modal.close`.
        let engine = I18nEngine::new(Some(custom("Japanese", &[("modal.close", "閉じる")])));
        assert_eq!(engine.t("modal.close"), "閉じる");
    }

    #[test]
    fn unbounded_key_accepts_any_width() {
        let long = "Подтверждение отмены текущего хода";
        let engine = I18nEngine::new(Some(custom(
            "Russian",
            &[("dialog.confirm_cancel.title", long)],
        )));
        assert_eq!(engine.t("dialog.confirm_cancel.title"), long);
    }

    #[test]
    fn unknown_key_returns_empty_fallback() {
        let engine = I18nEngine::english();
        assert_eq!(engine.t("nonexistent.key"), "");
    }

    #[test]
    fn hot_swap_applies_and_resets_locale() {
        let mut engine = I18nEngine::english();
        assert_eq!(engine.t("tab.repl"), "REPL");

        engine.apply_custom_locale(custom("Russian", &[("tab.repl", "Разраб")]));
        assert!(engine.is_custom());
        assert_eq!(engine.t("tab.repl"), "Разраб");

        engine.reset_locale_to_english();
        assert!(!engine.is_custom());
        assert_eq!(engine.t("tab.repl"), "REPL");
    }

    #[test]
    fn applying_blank_language_resets_to_english() {
        let mut engine = I18nEngine::new(Some(custom("Russian", &[("tab.repl", "Разраб")])));
        assert!(engine.is_custom());

        engine.apply_custom_locale(custom("   ", &[("tab.repl", "Разраб")]));
        assert!(!engine.is_custom());
        assert_eq!(engine.t("tab.repl"), "REPL");
    }

    #[test]
    fn every_catalog_key_resolves_non_empty_in_english() {
        let engine = I18nEngine::english();
        for item in canonical_catalog() {
            let resolved = engine.t(&item.key);
            assert!(
                !resolved.is_empty(),
                "catalog key '{}' resolved to an empty string in English",
                item.key
            );
            assert_eq!(
                resolved, item.english,
                "catalog key '{}' did not resolve to its canonical English text",
                item.key
            );
        }
    }

    #[test]
    fn every_catalog_key_resolves_non_empty_under_empty_custom_locale() {
        // A custom locale that supplies no translations must still resolve
        // every registered key to a non-empty string via the English original.
        let engine = I18nEngine::new(Some(CustomLocale::new("Russian", BTreeMap::new())));
        assert!(engine.is_custom());
        for item in canonical_catalog() {
            assert!(
                !engine.t(&item.key).is_empty(),
                "catalog key '{}' resolved to an empty string under a custom locale",
                item.key
            );
        }
    }

    #[test]
    fn complete_custom_locale_resolves_every_catalog_key() {
        // Mirror the canonical English for every key, which by construction
        // fits each key's column budget, and assert every key resolves to the
        // supplied translation instead of falling back to English.
        let mut translations = BTreeMap::new();
        for item in canonical_catalog() {
            translations.insert(item.key.clone(), item.english.clone());
        }
        let engine = I18nEngine::new(Some(CustomLocale::new("Russian", translations)));
        assert!(engine.is_custom());
        for item in canonical_catalog() {
            assert_eq!(
                engine.t(&item.key),
                item.english,
                "catalog key '{}' did not resolve to its supplied translation",
                item.key
            );
        }
    }

    #[test]
    fn global_active_t_resolves_every_catalog_key_non_empty() {
        // Whatever the process-wide locale state is, the global resolver must
        // never yield an empty string for a registered key (c-5 degradation).
        for item in canonical_catalog() {
            assert!(
                !active_t(&item.key).is_empty(),
                "active_t('{}') returned an empty string",
                item.key
            );
        }
    }

    #[test]
    fn install_active_locale_publishes_resolved_snapshot() {
        let engine = I18nEngine::new(Some(custom("Russian", &[("tab.repl", "Разраб")])));
        install_active_locale(&engine);
        assert_eq!(active_t("tab.repl"), "Разраб");
        assert_eq!(active_t("modal.confirm"), "Confirm");
        assert_eq!(active_t("definitely.unknown.key"), "");
        // Clean up after test to avoid bleeding into concurrent/subsequent tests
        install_active_locale(&I18nEngine::english());
    }
}
