use std::fs;
use std::path::PathBuf;
use serde::Deserialize;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    Auto,
    Dark,
    Light,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThemeConfig {
    #[serde(default)]
    pub mode: ThemeMode,
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

pub fn default_tui_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("tauqe").join("tui.toml")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config").join("tauqe").join("tui.toml")
    } else {
        PathBuf::from("tui.toml")
    }
}

pub fn save_tui_config(layout: LayoutPreset, langmap: Option<String>) -> std::io::Result<PathBuf> {
    save_tui_config_full(layout, langmap, PrimaryModifier::Ctrl)
}

pub fn save_tui_config_full(
    layout: LayoutPreset,
    langmap: Option<String>,
    primary_modifier: PrimaryModifier,
) -> std::io::Result<PathBuf> {
    let path = default_tui_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let layout_str = match layout {
        LayoutPreset::None => "none",
        LayoutPreset::RuJcuken => "ru_jcuken",
    };
    let mod_str = match primary_modifier {
        PrimaryModifier::Ctrl => "ctrl",
        PrimaryModifier::Alt => "alt",
    };
    let mut content = String::new();
    content.push_str("[input]\n");
    content.push_str("# Keyboard layout mapping for navigation screens and modified shortcuts:\n");
    content.push_str("#   \"none\"      - No translation (default)\n");
    content.push_str("#   \"ru_jcuken\" - Cyrillic JCUKEN -> QWERTY\n");
    content.push_str(&format!("layout = \"{}\"\n", layout_str));
    content.push_str("\n# Primary command modifier for text input commands (Emacs-style C-x):\n");
    content.push_str("#   \"ctrl\" (default) or \"alt\"\n");
    content.push_str(&format!("primary_modifier = \"{}\"\n", mod_str));
    if let Some(lm) = langmap {
        let trimmed = lm.trim();
        if !trimmed.is_empty() {
            content.push_str("\n# Custom character mapping (langmap):\n");
            content.push_str(&format!("langmap = \"{}\"\n", trimmed));
        }
    }
    content.push_str("\n[notifications]\n");
    content.push_str("# Sound bell (\\x07 / BEL) on long turn completion (triggers window alert/urgency in Kitty/WezTerm):\n");
    content.push_str("sound = true\n");
    content.push_str("# Desktop notification via terminal OSC escape codes:\n");
    content.push_str("desktop = true\n");
    content.push_str("# Desktop notification protocol: \"osc9\" (default, iTerm2/Windows Terminal), \"osc777\" (Kitty), or \"both\":\n");
    content.push_str("# desktop_protocol = \"osc9\"\n");
    content.push_str("# Notify only when terminal window has lost focus (requires terminal focus reporting):\n");
    content.push_str("only_unfocused = true\n");
    content.push_str("# Minimum turn duration in seconds before triggering notification (default: 5):\n");
    content.push_str("min_duration_seconds = 5\n");
    content.push_str("# Optional external command hook executed on long turn completion:\n");
    content.push_str("# command = \"paplay /usr/share/sounds/freedesktop/stereo/complete.oga\"\n");
    content.push_str("\n[theme]\n");
    content.push_str("# Color theme mode: \"auto\" (default), \"dark\", or \"light\":\n");
    content.push_str("# mode = \"auto\"\n");
    fs::write(&path, content)?;
    Ok(path)
}

pub fn find_tui_config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let p = PathBuf::from(xdg).join("tauqe").join("tui.toml");
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join(".config").join("tauqe").join("tui.toml");
        if p.exists() {
            return Some(p);
        }
    }
    None
}
