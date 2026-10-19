use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Persistent custom locale data object.
///
/// TAUQE maintains at most one alternative custom locale (c-4), stored as a
/// single JSON document at `$XDG_DATA_HOME/tauqe/custom_locale.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomLocale {
    /// Human-readable target language name (e.g. "Russian", "German", "Elvish").
    pub language: String,
    /// Generated translations keyed by the stable machine key.
    pub translations: BTreeMap<String, String>,
}

impl CustomLocale {
    pub fn new(language: impl Into<String>, translations: BTreeMap<String, String>) -> Self {
        Self {
            language: language.into(),
            translations,
        }
    }
}

/// Resolves the base directory for per-user persistent data.
///
/// Follows the XDG Base Directory Specification (`$XDG_DATA_HOME`), falling back
/// to `%LOCALAPPDATA%` on Windows, `$HOME/.local/share`, or `%USERPROFILE%/.local/share`.
fn data_base_dir() -> Option<PathBuf> {
    if let Some(dir) = non_empty_env("XDG_DATA_HOME") {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = non_empty_env("LOCALAPPDATA") {
        return Some(PathBuf::from(dir));
    }
    if let Some(home) = non_empty_env("HOME") {
        return Some(PathBuf::from(home).join(".local").join("share"));
    }
    if let Some(profile) = non_empty_env("USERPROFILE") {
        return Some(PathBuf::from(profile).join(".local").join("share"));
    }
    None
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Canonical filesystem path for the single custom locale file (c-4).
pub fn default_custom_locale_path() -> PathBuf {
    match data_base_dir() {
        Some(base) => base.join("tauqe").join("custom_locale.json"),
        None => PathBuf::from("custom_locale.json"),
    }
}

/// Loads the custom locale from the default storage path.
///
/// Returns `None` if the file does not exist or if the content is corrupt,
/// ensuring graceful degradation to English (c-5).
pub fn load_custom_locale() -> Option<CustomLocale> {
    load_custom_locale_from(&default_custom_locale_path())
}

/// Loads the custom locale from an explicit file path.
///
/// Recovers cleanly on missing files, I/O errors, or malformed JSON payloads.
pub fn load_custom_locale_from(path: &Path) -> Option<CustomLocale> {
    if !path.is_file() {
        return None;
    }
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str::<CustomLocale>(&content).ok()
}

/// Atomically persists the custom locale to the default storage path.
pub fn save_custom_locale(locale: &CustomLocale) -> io::Result<PathBuf> {
    let path = default_custom_locale_path();
    save_custom_locale_to(&path, locale)?;
    Ok(path)
}

/// Atomically writes the custom locale to the designated path.
///
/// Automatically creates missing parent directories and uses a temporary sibling
/// file before renaming, preventing partial file writes on unexpected termination.
pub fn save_custom_locale_to(path: &Path, locale: &CustomLocale) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let payload = serde_json::to_string_pretty(locale)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let temp_path = temp_sibling_path(path);
    fs::write(&temp_path, payload)?;

    if let Err(err) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    Ok(())
}

/// Deletes the custom locale file at the default storage location (e.g. on reset to English).
pub fn delete_custom_locale() -> io::Result<()> {
    delete_custom_locale_at(&default_custom_locale_path())
}

/// Deletes the custom locale file at the given path if it exists.
pub fn delete_custom_locale_at(path: &Path) -> io::Result<()> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Builds a unique temporary sibling path next to `target`.
///
/// The name combines the process id with the current timestamp so two
/// concurrent TAUQE processes (or a coarse clock that repeats a timestamp)
/// never share the same intermediate file and clobber each other's write.
fn temp_sibling_path(target: &Path) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let filename = format!(
        ".{}.tmp.{}.{}",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("custom_locale"),
        std::process::id(),
        nanos
    );
    match target.parent() {
        Some(p) => p.join(filename),
        None => PathBuf::from(filename),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_locale() -> CustomLocale {
        let mut map = BTreeMap::new();
        map.insert("tab.develop".to_string(), "Разработка".to_string());
        map.insert("modal.confirm".to_string(), "Принять".to_string());
        CustomLocale::new("Russian", map)
    }

    #[test]
    fn test_save_and_load_custom_locale() {
        let temp_dir = std::env::temp_dir().join(format!("tauqe_test_{}", std::process::id()));
        let file_path = temp_dir.join("subdir").join("custom_locale.json");

        let locale = sample_locale();
        assert!(save_custom_locale_to(&file_path, &locale).is_ok());
        assert!(file_path.is_file());

        let loaded = load_custom_locale_from(&file_path).expect("loaded successfully");
        assert_eq!(loaded, locale);

        assert!(delete_custom_locale_at(&file_path).is_ok());
        assert!(!file_path.exists());
        assert_eq!(load_custom_locale_from(&file_path), None);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_load_nonexistent_returns_none() {
        let path = PathBuf::from("nonexistent_tauqe_locale_file.json");
        assert_eq!(load_custom_locale_from(&path), None);
    }

    #[test]
    fn test_load_corrupted_json_returns_none() {
        let temp_file =
            std::env::temp_dir().join(format!("corrupt_locale_{}.json", std::process::id()));
        fs::write(&temp_file, b"not valid json {{{").expect("write file");

        let loaded = load_custom_locale_from(&temp_file);
        assert_eq!(loaded, None);

        let _ = fs::remove_file(&temp_file);
    }

    #[test]
    fn test_temp_sibling_path_includes_process_id() {
        let target = Path::new("/tmp/custom_locale.json");
        let temp = temp_sibling_path(target);
        let name = temp
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        assert!(name.starts_with(".custom_locale.json.tmp."));
        assert!(name.contains(&format!(".tmp.{}.", std::process::id())));
        assert_eq!(temp.parent(), target.parent());
    }

    #[test]
    fn test_delete_nonexistent_is_noop() {
        let path = PathBuf::from("nonexistent_tauqe_locale_delete.json");
        assert!(delete_custom_locale_at(&path).is_ok());
    }
}
