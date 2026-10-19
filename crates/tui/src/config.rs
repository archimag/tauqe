use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
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

    /// Emacs-style chord prefix (`C-` for Ctrl, `M-` for Alt).
    pub fn emacs_prefix(&self) -> &'static str {
        match self {
            PrimaryModifier::Ctrl => "C-",
            PrimaryModifier::Alt => "M-",
        }
    }
}

/// Abstract meta command modifier (`M-`), the Readline/Emacs counterpart of the
/// primary command modifier.
///
/// A keyboard carries two independent command chords: the primary modifier
/// (`C-`, [`PrimaryModifier`]) and the meta modifier (`M-`). A terminal without
/// a dedicated Meta key sends `M-` as either `Alt` (most modern terminals) or
/// `Ctrl` (legacy setups), so the chord is expressed abstractly and resolved
/// here instead of hardcoding `KeyModifiers::ALT` at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetaModifier {
    #[default]
    Alt,
    Ctrl,
}

impl MetaModifier {
    pub fn matches(&self, modifiers: KeyModifiers) -> bool {
        match self {
            MetaModifier::Alt => modifiers.contains(KeyModifiers::ALT),
            MetaModifier::Ctrl => modifiers.contains(KeyModifiers::CONTROL),
        }
    }

    pub fn prefix_str(&self) -> &'static str {
        match self {
            MetaModifier::Alt => "Alt+",
            MetaModifier::Ctrl => "Ctrl+",
        }
    }

    /// Emacs-style chord prefix (`M-` for Alt, `C-` for Ctrl).
    pub fn emacs_prefix(&self) -> &'static str {
        match self {
            MetaModifier::Alt => "M-",
            MetaModifier::Ctrl => "C-",
        }
    }
}

/// Whether the modifiers form the AltGr / compose chord (`Ctrl+Alt`).
///
/// On Windows and X11 AltGr arrives as `CONTROL | ALT`, so neither the primary
/// nor the meta modifier may claim it; otherwise typing composed characters or
/// AltGr digits would trigger application commands.
fn is_compose_chord(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) && modifiers.contains(KeyModifiers::ALT)
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct InputConfig {
    #[serde(default)]
    pub layout: LayoutPreset,
    #[serde(default)]
    pub langmap: Option<String>,
    #[serde(default)]
    pub primary_modifier: PrimaryModifier,
    #[serde(default)]
    pub meta_modifier: MetaModifier,
}

impl InputConfig {
    /// Whether the modifiers encode the abstract primary command modifier
    /// (`C-`), excluding the AltGr / compose chord.
    pub fn is_primary(&self, modifiers: KeyModifiers) -> bool {
        !is_compose_chord(modifiers) && self.primary_modifier.matches(modifiers)
    }

    /// Whether the modifiers encode the abstract meta command modifier (`M-`),
    /// excluding the AltGr / compose chord.
    pub fn is_meta(&self, modifiers: KeyModifiers) -> bool {
        !is_compose_chord(modifiers) && self.meta_modifier.matches(modifiers)
    }
}

/// Process-wide snapshot of the configured input modifiers.
///
/// Rendering surfaces that build key hints without a reference to the
/// application state read the `C-` / `M-` labels from here. The configuration
/// load path installs them once, so drawing a dialog never touches the file
/// system and never hardcodes a modifier string (Conventions §4).
static ACTIVE_PRIMARY_MODIFIER: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
static ACTIVE_META_MODIFIER: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Installs the configured command modifiers for global key projection.
pub fn set_active_input_modifiers(primary: PrimaryModifier, meta: MetaModifier) {
    use std::sync::atomic::Ordering;
    ACTIVE_PRIMARY_MODIFIER.store(
        match primary {
            PrimaryModifier::Ctrl => 0,
            PrimaryModifier::Alt => 1,
        },
        Ordering::Relaxed,
    );
    ACTIVE_META_MODIFIER.store(
        match meta {
            MetaModifier::Alt => 0,
            MetaModifier::Ctrl => 1,
        },
        Ordering::Relaxed,
    );
}

/// The active primary command modifier (`C-`).
pub fn active_primary_modifier() -> PrimaryModifier {
    match ACTIVE_PRIMARY_MODIFIER.load(std::sync::atomic::Ordering::Relaxed) {
        1 => PrimaryModifier::Alt,
        _ => PrimaryModifier::Ctrl,
    }
}

/// The active meta command modifier (`M-`).
pub fn active_meta_modifier() -> MetaModifier {
    match ACTIVE_META_MODIFIER.load(std::sync::atomic::Ordering::Relaxed) {
        1 => MetaModifier::Ctrl,
        _ => MetaModifier::Alt,
    }
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
/// `Auto` (default) detects the capability from `COLORTERM`/`TERM`. `Truecolor`,
/// `Ansi256`, and `Ansi16` force a specific depth, letting a user correct
/// detection or opt into the attribute-based fallback on terminals that support
/// 24-bit color without advertising it (for example Windows Terminal or tmux).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorDepth {
    #[default]
    Auto,
    Truecolor,
    Ansi256,
    Ansi16,
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
    /// Explicit built-in preset name (for example `"catppuccin"`). When set and
    /// recognized, it overrides `mode`.
    #[serde(default)]
    pub name: Option<String>,
    /// Legacy light/dark mode, used as the fallback when `name` is unset.
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
            let config = TuiConfig::default();
            config.install_input_modifiers();
            return Ok(config);
        };
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;
        let config = toml::from_str::<TuiConfig>(&content).map_err(|e| {
            format!(
                "Invalid {}: {}",
                path.display(),
                e.to_string().lines().next().unwrap_or("parse error")
            )
        })?;
        config.install_input_modifiers();
        Ok(config)
    }

    /// Publishes the configured command modifiers for global key projection.
    ///
    /// Surfaces that build keybinding hints without a reference to the
    /// application state read them back through [`active_primary_modifier`] and
    /// [`active_meta_modifier`], so `C-` / `M-` labels always track the
    /// developer's configuration.
    fn install_input_modifiers(&self) {
        set_active_input_modifiers(self.input.primary_modifier, self.input.meta_modifier);
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
    save_tui_config_full(layout, langmap, PrimaryModifier::Ctrl, MetaModifier::Alt)
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

# Meta command modifier for text input commands (Emacs-style M-x):
#   "alt" (default) or "ctrl"
meta_modifier = "alt"

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
# Named color preset, overriding `mode` when set:
#   "tauqe-dark" (default) | "tauqe-light" | "catppuccin" |
#   "gruvbox" | "solarized-dark"
# name = "tauqe-dark"
# Legacy light/dark mode, used as the fallback when `name` is unset:
#   "auto" (default, probes the terminal background), "dark", or "light"
# mode = "auto"
# Terminal color depth for RGB styles: "auto" (default, detected from
# COLORTERM/TERM), "truecolor" (force 24-bit), "ansi256" (force the xterm
# 256-color cube), or "ansi16" (force the attribute-based fallback):
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
    meta_modifier: MetaModifier,
) -> std::io::Result<PathBuf> {
    let path = default_tui_config_path();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let source = fs::read_to_string(&path).unwrap_or_else(|_| DEFAULT_TUI_TEMPLATE.to_string());
    let merged = merge_workstation_settings(
        &source,
        layout,
        langmap.as_deref(),
        primary_modifier,
        meta_modifier,
    );
    write_atomically(&path, &merged)?;
    Ok(path)
}

/// Persists the selected theme preset into `tui.toml` under `[theme] name`,
/// preserving every unrelated key and comment. A missing file is seeded from
/// [`DEFAULT_TUI_TEMPLATE`].
pub fn save_theme_preset(name: &str) -> std::io::Result<PathBuf> {
    let path = default_tui_config_path();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let source = fs::read_to_string(&path).unwrap_or_else(|_| DEFAULT_TUI_TEMPLATE.to_string());
    let merged = merge_theme_name(&source, name);
    write_atomically(&path, &merged)?;
    Ok(path)
}

/// Writes `content` into `path` atomically through a temporary sibling file and
/// a final rename, so an interrupted or crashed write never leaves `tui.toml`
/// truncated or partially updated.
fn write_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    let temp_path = temp_sibling_path(path);
    fs::write(&temp_path, content)?;
    if let Err(err) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    Ok(())
}

/// Builds a unique temporary sibling path next to `target`.
///
/// The process id is combined with the current timestamp so concurrent TAUQE
/// processes, or a coarse system clock that repeats a timestamp, never share an
/// intermediate file.
fn temp_sibling_path(target: &Path) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let filename = format!(
        ".{}.tmp.{}.{}",
        target.file_name().and_then(|n| n.to_str()).unwrap_or("tui"),
        std::process::id(),
        nanos
    );
    match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(filename),
        _ => PathBuf::from(filename),
    }
}

/// Merges a theme preset name into an existing document while preserving every
/// unrelated key, section, and comment. Malformed input degrades to an empty
/// document instead of failing.
fn merge_theme_name(source: &str, name: &str) -> String {
    let mut doc: DocumentMut = source.parse().unwrap_or_else(|_| DocumentMut::new());
    if !doc.contains_key("theme") {
        doc.insert("theme", Item::Table(Table::new()));
    }
    if let Some(theme) = doc.get_mut("theme").and_then(|item| item.as_table_mut()) {
        set_input_value(theme, "name", name);
    }
    doc.to_string()
}

/// Merges the workstation settings into an existing document while preserving
/// every unrelated key, section, and comment. Malformed input degrades to an
/// empty document instead of failing the save.
fn merge_workstation_settings(
    source: &str,
    layout: LayoutPreset,
    langmap: Option<&str>,
    primary_modifier: PrimaryModifier,
    meta_modifier: MetaModifier,
) -> String {
    let mut doc: DocumentMut = source.parse().unwrap_or_else(|_| DocumentMut::new());

    if !doc.contains_key("input") {
        doc.insert("input", Item::Table(Table::new()));
    }

    if let Some(input) = doc.get_mut("input").and_then(|item| item.as_table_mut()) {
        set_input_value(input, "layout", layout_str(layout));
        set_input_value(
            input,
            "primary_modifier",
            primary_modifier_str(primary_modifier),
        );
        set_input_value(input, "meta_modifier", meta_modifier_str(meta_modifier));
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

fn meta_modifier_str(modifier: MetaModifier) -> &'static str {
    match modifier {
        MetaModifier::Alt => "alt",
        MetaModifier::Ctrl => "ctrl",
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
    fn test_theme_config_deserializes_named_preset_and_ansi16() {
        let cfg: TuiConfig =
            toml::from_str("[theme]\nname = \"catppuccin\"\ncolor_depth = \"ansi16\"\n")
                .expect("valid tui config");
        assert_eq!(cfg.theme.name.as_deref(), Some("catppuccin"));
        assert_eq!(cfg.theme.color_depth, ColorDepth::Ansi16);
    }

    #[test]
    fn test_theme_config_name_defaults_to_none_and_legacy_mode_still_parses() {
        let cfg: TuiConfig = toml::from_str("[theme]\nmode = \"dark\"\n").expect("valid");
        assert_eq!(cfg.theme.name, None);
        assert_eq!(cfg.theme.mode, ThemeMode::Dark);
        assert_eq!(cfg.theme.color_depth, ColorDepth::Auto);
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
            MetaModifier::Alt,
        );
        assert!(merged.contains("\"none\""));
        assert!(!merged.contains("ru_jcuken"));
        assert!(merged.contains("\"ctrl\""));
        assert!(merged.contains("meta_modifier"));
        assert!(merged.contains("\"alt\""));
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
            MetaModifier::Alt,
        );
        assert!(merged.contains("ru_jcuken"));
        assert!(!merged.contains("langmap"));
    }

    #[test]
    fn test_merge_workstation_settings_seeds_missing_input_table() {
        let merged = merge_workstation_settings(
            "",
            LayoutPreset::None,
            None,
            PrimaryModifier::Alt,
            MetaModifier::Ctrl,
        );
        assert!(merged.contains("layout"));
        assert!(merged.contains("\"none\""));
        assert!(merged.contains("primary_modifier"));
        assert!(merged.contains("\"alt\""));
        assert!(merged.contains("meta_modifier"));
        assert!(merged.contains("\"ctrl\""));
    }

    #[test]
    fn test_meta_modifier_defaults_parse_and_prefixes() {
        // A missing key resolves to the Alt default (the most common Meta key),
        // while the primary modifier keeps its Ctrl default.
        let default: TuiConfig = toml::from_str("").expect("empty config");
        assert_eq!(default.input.meta_modifier, MetaModifier::Alt);
        assert_eq!(default.input.primary_modifier, PrimaryModifier::Ctrl);

        let cfg: TuiConfig =
            toml::from_str("[input]\nmeta_modifier = \"ctrl\"\n").expect("valid tui config");
        assert_eq!(cfg.input.meta_modifier, MetaModifier::Ctrl);

        assert_eq!(MetaModifier::Alt.prefix_str(), "Alt+");
        assert_eq!(MetaModifier::Alt.emacs_prefix(), "M-");
        assert_eq!(MetaModifier::Ctrl.prefix_str(), "Ctrl+");
        assert_eq!(MetaModifier::Ctrl.emacs_prefix(), "C-");

        assert_eq!(PrimaryModifier::Ctrl.emacs_prefix(), "C-");
        assert_eq!(PrimaryModifier::Alt.emacs_prefix(), "M-");
    }

    #[test]
    fn test_input_config_is_primary_and_is_meta_exclude_altgr() {
        let ctrl_primary = InputConfig {
            primary_modifier: PrimaryModifier::Ctrl,
            meta_modifier: MetaModifier::Alt,
            ..InputConfig::default()
        };
        assert!(ctrl_primary.is_primary(KeyModifiers::CONTROL));
        assert!(ctrl_primary.is_meta(KeyModifiers::ALT));
        assert!(!ctrl_primary.is_primary(KeyModifiers::ALT));
        assert!(!ctrl_primary.is_meta(KeyModifiers::CONTROL));

        // The AltGr / compose chord belongs to neither modifier.
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert!(!ctrl_primary.is_primary(altgr));
        assert!(!ctrl_primary.is_meta(altgr));

        // Swapping the chords keeps the same AltGr exclusion.
        let alt_primary = InputConfig {
            primary_modifier: PrimaryModifier::Alt,
            meta_modifier: MetaModifier::Ctrl,
            ..InputConfig::default()
        };
        assert!(alt_primary.is_primary(KeyModifiers::ALT));
        assert!(alt_primary.is_meta(KeyModifiers::CONTROL));
        assert!(!alt_primary.is_primary(altgr));
        assert!(!alt_primary.is_meta(altgr));
    }

    #[test]
    fn test_merge_workstation_settings_persists_paired_meta_modifier() {
        let merged = merge_workstation_settings(
            "",
            LayoutPreset::None,
            None,
            PrimaryModifier::Alt,
            MetaModifier::Ctrl,
        );
        assert!(merged.contains("primary_modifier = \"alt\""));
        assert!(merged.contains("meta_modifier = \"ctrl\""));
    }

    #[test]
    fn test_merge_theme_name_preserves_unrelated_keys() {
        let source = "# Personal\n[theme]\nmode = \"dark\"\n\n[input]\nlayout = \"none\"\n";
        let merged = merge_theme_name(source, "gruvbox");
        assert!(merged.contains("name = \"gruvbox\""));
        assert!(merged.contains("# Personal"));
        assert!(merged.contains("mode = \"dark\""));
        assert!(merged.contains("layout = \"none\""));
    }

    #[test]
    fn test_write_atomically_replaces_content_without_leftover_temp_files() {
        let dir = std::env::temp_dir().join(format!("tauqe_cfg_atomic_{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("tui.toml");

        write_atomically(&path, "[input]\nlayout = \"none\"\n").expect("first write");
        write_atomically(&path, "[input]\nlayout = \"ru_jcuken\"\n").expect("second write");
        assert_eq!(
            fs::read_to_string(&path).expect("read back"),
            "[input]\nlayout = \"ru_jcuken\"\n"
        );

        let leftovers = fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name() != std::ffi::OsStr::new("tui.toml"))
            .count();
        assert_eq!(leftovers, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_theme_name_seeds_missing_theme_table() {
        let merged = merge_theme_name("", "tauqe-light");
        assert!(merged.contains("[theme]"));
        assert!(merged.contains("name = \"tauqe-light\""));
    }
}
