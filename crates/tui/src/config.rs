use std::fs;
use std::path::PathBuf;
use serde::Deserialize;
use toml_edit::{value, DocumentMut, Item, Table, Value};

use crossterm::event::KeyModifiers;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutPreset {
    #[default]
    None,
    RuJcuken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimaryModifier {
    #[default]
    Ctrl,
    Alt,
}

impl PrimaryModifier {
    pub fn matches(&self, modifiers: KeyModifiers) -> bool {
        match self {
            PrimaryModifier::Ctrl => modifiers.contains(KeyModifiers::CONTROL),
            PrimaryModifier::Alt => modifiers.contains(KeyModifiers::ALT),
        }
    }

    pub fn prefix_str(&self) -> &'static str {
        match self {
            PrimaryModifier::Ctrl => "Ctrl+",
            PrimaryModifier::Alt => "Alt+",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct InputConfig {
    #[serde(default)]
    pub layout: LayoutPreset,
    #[serde(default)]
    pub langmap: Option<String>,
    #[serde(default)]
    pub primary_modifier: PrimaryModifier,
}

fn default_true() -> bool {
    true
}

fn default_min_duration() -> u64 {
    5
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopNotificationProtocol {
    #[default]
    Osc9,
    Osc777,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NotificationConfig {
    #[serde(default = "default_true")]
    pub sound: bool,
    #[serde(default = "default_true")]
    pub desktop: bool,
    #[serde(default)]
    pub desktop_protocol: DesktopNotificationProtocol,
    #[serde(default = "default_min_duration")]
    pub min_duration_seconds: u64,
    #[serde(default = "default_true")]
    pub only_unfocused: bool,
    #[serde(default)]
    pub command: Option<String>,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            sound: true,
            desktop: true,
            desktop_protocol: DesktopNotificationProtocol::Osc9,
            min_duration_seconds: 5,
            only_unfocused: true,
            command: None,
        }
    }
}

impl NotificationConfig {
    /// Whether a completed operation (a single turn or a whole batch) should
    /// raise a notification, applying the configured minimum duration and
    /// terminal focus policy.
    pub fn should_notify(&self, elapsed_seconds: u64, focused: bool) -> bool {
        if self.only_unfocused && focused {
            return false;
        }
        elapsed_seconds >= self.min_duration_seconds
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    Auto,
    Dark,
    Light,
}

/// Terminal color depth for RGB styles.
///
/// `Auto` (default) detects the capability from `COLORTERM`/`TERM`. `Truecolor`
/// and `Ansi256` force the palette, letting a user correct detection on
/// terminals that support 24-bit color without advertising it (for example
/// Windows Terminal or tmux).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorDepth {
    #[default]
    Auto,
    Truecolor,
    Ansi256,
}

impl ThemeMode {
    /// Resolves a configured mode into a concrete one.
    ///
    /// `Auto` probes the terminal background on a best-effort basis; every
    /// other value is returned unchanged. The probe result is cached for the
    /// process lifetime, so repeated calls are cheap.
    pub fn resolve(self) -> ThemeMode {
        match self {
            ThemeMode::Auto => {
                if *LIGHT_BACKGROUND {
                    ThemeMode::Light
                } else {
                    ThemeMode::Dark
                }
            }
            explicit => explicit,
        }
    }
}

/// Best-effort, process-lifetime detection of a light terminal background.
///
/// `COLORFGBG` (set by many terminals as `fg;bg`) is inspected: only the
/// clearly light palette indices (7 and 15) count as light. An unset or
/// unrecognized value falls back to dark, matching the historical default.
static LIGHT_BACKGROUND: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    let Ok(value) = std::env::var("COLORFGBG") else {
        return false;
    };
    let Some(bg) = value.rsplit(';').next() else {
        return false;
    };
    matches!(bg.trim().parse::<u16>(), Ok(7 | 15))
});

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThemeConfig {
    #[serde(default)]
    pub mode: ThemeMode,
    #[serde(default)]
    pub color_depth: ColorDepth,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TuiConfig {
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub theme: ThemeConfig,
}

impl TuiConfig {
    /// Loads `tui.toml`. A missing file yields defaults; an unreadable or
    /// invalid file is reported as an error so the caller can surface it.
    pub fn load_checked() -> Result<Self, String> {
        let Some(path) = find_tui_config_path() else {
            return Ok(TuiConfig::default());
        };
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;
        toml::from_str::<TuiConfig>(&content).map_err(|e| {
            format!(
                "Invalid {}: {}",
                path.display(),
                e.to_string().lines().next().unwrap_or("parse error")
            )
        })
    }

    pub fn load() -> Self {
        Self::load_checked().unwrap_or_default()
    }
}

/// Base directory for TAUQE's per-user TUI configuration.
///
/// Follows the XDG Base Directory Specification while remaining usable on
/// Windows: `XDG_CONFIG_HOME`, then `%APPDATA%`, then `$HOME/.config`, then
/// `%USERPROFILE%\.config`. Returns `None` only when no environment variable
/// identifies a configuration or home directory.
fn config_base_dir() -> Option<PathBuf> {
    if let Some(dir) = non_empty_env("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = non_empty_env("APPDATA") {
        return Some(PathBuf::from(dir));
    }
    if let Some(home) = non_empty_env("HOME") {
        return Some(PathBuf::from(home).join(".config"));
    }
    if let Some(profile) = non_empty_env("USERPROFILE") {
        return Some(PathBuf::from(profile).join(".config"));
    }
    None
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Canonical per-user path of `tui.toml`.
///
/// Reading and writing share this single path so a saved configuration is always
/// discovered on the next start and personal settings never land in the
/// repository working tree (Conventions §6.2).
pub fn default_tui_config_path() -> PathBuf {
    match config_base_dir() {
        Some(base) => base.join("tauqe").join("tui.toml"),
        None => PathBuf::from("tui.toml"),
    }
}

pub fn save_tui_config(layout: LayoutPreset, langmap: Option<String>) -> std::io::Result<PathBuf> {
    save_tui_config_full(layout, langmap, PrimaryModifier::Ctrl)
}

/// Template seeded into a fresh `tui.toml`. It documents the default values and
/// the optional keys so a first-time user can discover them without opening the
/// documentation.
const DEFAULT_TUI_TEMPLATE: &str = r#"[input]
# Keyboard layout mapping for navigation screens and modified shortcuts:
#   "none"      - No translation (default)
#   "ru_jcuken" - Cyrillic JCUKEN -> QWERTY
layout = "none"

# Primary command modifier for text input commands (Emacs-style C-x):
#   "ctrl" (default) or "alt"
primary_modifier = "ctrl"

[notifications]
# Sound bell (\x07 / BEL) on long turn completion (triggers window alert/urgency in Kitty/WezTerm):
sound = true
# Desktop notification via terminal OSC escape codes:
desktop = true
# Desktop notification protocol: "osc9" (default, iTerm2/Windows Terminal), "osc777" (Kitty), or "both":
# desktop_protocol = "osc9"
# Notify only when terminal window has lost focus (requires terminal focus reporting):
only_unfocused = true
# Minimum turn duration in seconds before triggering notification (default: 5):
min_duration_seconds = 5
# Optional external command hook executed on long turn completion:
# command = "paplay /usr/share/sounds/freedesktop/stereo/complete.oga"

[theme]
# Color theme mode: "auto" (default), "dark", or "light":
# mode = "auto"
# Terminal color depth for RGB styles: "auto" (default, detected from
# COLORTERM/TERM), "truecolor" (force 24-bit), or "ansi256" (force fallback):
# color_depth = "auto"
"#;

/// Persists the workstation settings (`layout`, `primary_modifier`, and an
/// optional custom `langmap`) into `tui.toml`.
///
/// The existing file is edited in place with `toml_edit`, so unrelated user keys
/// (`notifications`, `theme`, hooks) and comments survive; a missing file is
/// seeded from [`DEFAULT_TUI_TEMPLATE`].
pub fn save_tui_config_full(
    layout: LayoutPreset,
    langmap: Option<String>,
    primary_modifier: PrimaryModifier,
) -> std::io::Result<PathBuf> {
    let path = default_tui_config_path();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let source = fs::read_to_string(&path).unwrap_or_else(|_| DEFAULT_TUI_TEMPLATE.to_string());
    let merged = merge_workstation_settings(&source, layout, langmap.as_deref(), primary_modifier);
    fs::write(&path, merged)?;
    Ok(path)
}

/// Merges the workstation settings into an existing document while preserving
/// every unrelated key, section, and comment. Malformed input degrades to an
/// empty document instead of failing the save.
fn merge_workstation_settings(
    source: &str,
    layout: LayoutPreset,
    langmap: Option<&str>,
    primary_modifier: PrimaryModifier,
) -> String {
    let mut doc: DocumentMut = source.parse().unwrap_or_else(|_| DocumentMut::new());

    if !doc.contains_key("input") {
        doc.insert("input", Item::Table(Table::new()));
    }

    if let Some(input) = doc.get_mut("input").and_then(|item| item.as_table_mut()) {
        set_input_value(input, "layout", layout_str(layout));
        set_input_value(input, "primary_modifier", primary_modifier_str(primary_modifier));
        match langmap.map(str::trim).filter(|s| !s.is_empty()) {
            Some(lm) => set_input_value(input, "langmap", lm),
            None => {
                input.remove("langmap");
            }
        }
    }

    doc.to_string()
}

fn layout_str(layout: LayoutPreset) -> &'static str {
    match layout {
        LayoutPreset::None => "none",
        LayoutPreset::RuJcuken => "ru_jcuken",
    }
}

fn primary_modifier_str(modifier: PrimaryModifier) -> &'static str {
    match modifier {
        PrimaryModifier::Ctrl => "ctrl",
        PrimaryModifier::Alt => "alt",
    }
}

/// Writes a scalar into the `[input]` table, keeping the surrounding comment
/// decor when the key already exists so hand-written comments survive.
fn set_input_value(table: &mut Table, key: &str, val: &str) {
    if let Some(existing) = table.get_mut(key).and_then(|item| item.as_value_mut()) {
        let decor = existing.decor().clone();
        *existing = Value::from(val);
        *existing.decor_mut() = decor;
    } else {
        table.insert(key, value(val));
    }
}

pub fn find_tui_config_path() -> Option<PathBuf> {
    let path = default_tui_config_path();
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_notify_respects_duration_and_focus() {
        let focused_aware = NotificationConfig {
            min_duration_seconds: 5,
            only_unfocused: true,
            ..NotificationConfig::default()
        };
        assert!(!focused_aware.should_notify(4, false));
        assert!(focused_aware.should_notify(5, false));
        assert!(!focused_aware.should_notify(30, true));

        let always = NotificationConfig {
            only_unfocused: false,
            ..NotificationConfig::default()
        };
        assert!(always.should_notify(30, true));
        assert!(!always.should_notify(1, true));
    }

    #[test]
    fn test_theme_mode_resolve_preserves_explicit_modes() {
        assert_eq!(ThemeMode::Dark.resolve(), ThemeMode::Dark);
        assert_eq!(ThemeMode::Light.resolve(), ThemeMode::Light);
    }

    #[test]
    fn test_theme_config_deserializes_color_depth() {
        let cfg: TuiConfig =
            toml::from_str("[theme]\nmode = \"dark\"\ncolor_depth = \"truecolor\"\n")
                .expect("valid tui config");
        assert_eq!(cfg.theme.mode, ThemeMode::Dark);
        assert_eq!(cfg.theme.color_depth, ColorDepth::Truecolor);

        let default: TuiConfig = toml::from_str("").expect("empty config");
        assert_eq!(default.theme.color_depth, ColorDepth::Auto);
    }

    #[test]
    fn test_merge_workstation_settings_preserves_unrelated_keys_and_comments() {
        let source = r#"# Personal TUI settings
[input]
# my layout
layout = "ru_jcuken"
primary_modifier = "alt"

[notifications]
sound = false
command = "notify-send done"

[theme]
mode = "light"
"#;
        let merged = merge_workstation_settings(
            source,
            LayoutPreset::None,
            Some("йцукен"),
            PrimaryModifier::Ctrl,
        );
        assert!(merged.contains("\"none\""));
        assert!(!merged.contains("ru_jcuken"));
        assert!(merged.contains("\"ctrl\""));
        assert!(merged.contains("langmap"));
        assert!(merged.contains("йцукен"));
        assert!(merged.contains("# Personal TUI settings"));
        assert!(merged.contains("# my layout"));
        assert!(merged.contains("sound = false"));
        assert!(merged.contains("command = \"notify-send done\""));
        assert!(merged.contains("mode = \"light\""));
    }

    #[test]
    fn test_merge_workstation_settings_removes_stale_langmap() {
        let source = "[input]\nlayout = \"none\"\nlangmap = \"abc\"\n";
        let merged = merge_workstation_settings(
            source,
            LayoutPreset::RuJcuken,
            None,
            PrimaryModifier::Ctrl,
        );
        assert!(merged.contains("ru_jcuken"));
        assert!(!merged.contains("langmap"));
    }

    #[test]
    fn test_merge_workstation_settings_seeds_missing_input_table() {
        let merged =
            merge_workstation_settings("", LayoutPreset::None, None, PrimaryModifier::Alt);
        assert!(merged.contains("layout"));
        assert!(merged.contains("\"none\""));
        assert!(merged.contains("primary_modifier"));
        assert!(merged.contains("\"alt\""));
    }
}
