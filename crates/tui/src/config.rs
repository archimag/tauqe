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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NotificationConfig {
    #[serde(default = "default_true")]
    pub sound: bool,
    #[serde(default = "default_true")]
    pub desktop: bool,
    #[serde(default = "default_min_duration")]
    pub min_duration_seconds: u64,
    #[serde(default)]
    pub command: Option<String>,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            sound: true,
            desktop: true,
            min_duration_seconds: 5,
            command: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TuiConfig {
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub notifications: NotificationConfig,
}

impl TuiConfig {
    pub fn load() -> Self {
        let config_path = find_tui_config_path();
        if let Some(path) = config_path {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(cfg) = toml::from_str::<TuiConfig>(&content) {
                    return cfg;
                }
            }
        }
        TuiConfig::default()
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
    content.push_str("# Minimum turn duration in seconds before triggering notification (default: 5):\n");
    content.push_str("min_duration_seconds = 5\n");
    content.push_str("# Optional external command hook executed on long turn completion:\n");
    content.push_str("# command = \"paplay /usr/share/sounds/freedesktop/stereo/complete.oga\"\n");
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
