//! Built-in calibrated theme presets.
//!
//! A preset is a complete, hand-tuned [`ThemePalette`]: every semantic role is
//! chosen so text, borders, buttons, statuses, and diffs keep their meaning and
//! stay legible together. Palettes are authored in 24-bit sRGB and projected
//! onto the terminal's color depth by the engine in `color`.
//!
//! Selection is declarative: configuration names a preset (a fixed scheme)
//! rather than individual colors, so user config stays legible and a palette can
//! be recalibrated without touching it. When no preset is named, resolution
//! falls back to the legacy light/dark mode for backward compatibility.
//!
//! The parent module is exempt from the dead-code lint while the design system
//! is adopted incrementally, so this module inherits that allowance.

use serde::{Deserialize, Serialize};

use crate::config::ThemeMode;
use crate::theme::color::RgbColor;
use crate::theme::palette::ThemePalette;

/// A built-in, calibrated theme preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemePreset {
    /// The signature tauqe dark scheme: a restrained graphite-blue palette.
    TauqeDark,
    /// The signature tauqe light scheme: clean paper with muted pastels.
    TauqeLight,
    /// Catppuccin Mocha.
    Catppuccin,
    /// Gruvbox dark: a warm, retro-inspired dark scheme.
    Gruvbox,
    /// Solarized Dark, recalibrated in true color for reliable contrast.
    SolarizedDark,
}

impl ThemePreset {
    /// Every preset, in the stable order used by selectors and documentation.
    pub const ALL: [ThemePreset; 5] = [
        ThemePreset::TauqeDark,
        ThemePreset::TauqeLight,
        ThemePreset::Catppuccin,
        ThemePreset::Gruvbox,
        ThemePreset::SolarizedDark,
    ];

    /// The canonical lowercase identifier used in configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            ThemePreset::TauqeDark => "tauqe-dark",
            ThemePreset::TauqeLight => "tauqe-light",
            ThemePreset::Catppuccin => "catppuccin",
            ThemePreset::Gruvbox => "gruvbox",
            ThemePreset::SolarizedDark => "solarized-dark",
        }
    }

    /// Resolves a preset from a user-provided identifier.
    ///
    /// Matching is lenient: case is ignored and `_`, spaces, and `-` are treated
    /// equivalently, so `Tauqe_Dark` resolves like `tauqe-dark`. Unrecognized
    /// names yield `None`.
    pub fn from_name(name: &str) -> Option<Self> {
        let normalized = name.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        ThemePreset::ALL
            .into_iter()
            .find(|preset| preset.as_str() == normalized)
    }

    /// Resolves the effective preset from an optional name and the light/dark
    /// mode.
    ///
    /// A recognized `name` always wins. Otherwise the preset falls back to the
    /// resolved [`ThemeMode`], keeping configuration written before named
    /// presets existed valid.
    pub fn resolve(name: Option<&str>, mode: ThemeMode) -> Self {
        name.and_then(ThemePreset::from_name)
            .unwrap_or_else(|| match mode.resolve() {
                ThemeMode::Light => ThemePreset::TauqeLight,
                _ => ThemePreset::TauqeDark,
            })
    }

    /// The full semantic palette this preset defines.
    pub fn palette(self) -> ThemePalette {
        match self {
            ThemePreset::TauqeDark => TAUQE_DARK,
            ThemePreset::TauqeLight => TAUQE_LIGHT,
            ThemePreset::Catppuccin => CATPPUCCIN_MOCHA,
            ThemePreset::Gruvbox => GRUVBOX_DARK,
            ThemePreset::SolarizedDark => SOLARIZED_DARK,
        }
    }
}

/// Signature tauqe dark scheme: graphite canvas with a restrained Tokyo Night
/// accent set.
const TAUQE_DARK: ThemePalette = ThemePalette {
    canvas: RgbColor::new(26, 27, 38),
    surface: RgbColor::new(36, 40, 59),
    overlay: RgbColor::new(47, 53, 73),
    border: RgbColor::new(59, 66, 97),
    text_primary: RgbColor::new(192, 202, 245),
    text_secondary: RgbColor::new(169, 177, 214),
    text_muted: RgbColor::new(86, 95, 137),
    text_inverse: RgbColor::new(26, 27, 38),
    button_bg: RgbColor::new(59, 66, 97),
    button_fg: RgbColor::new(192, 202, 245),
    button_active_bg: RgbColor::new(122, 162, 247),
    status_success: RgbColor::new(158, 206, 106),
    status_warning: RgbColor::new(224, 175, 104),
    status_error: RgbColor::new(247, 118, 142),
    status_info: RgbColor::new(125, 207, 255),
    diff_added: RgbColor::new(158, 206, 106),
    diff_removed: RgbColor::new(247, 118, 142),
    diff_context: RgbColor::new(86, 95, 137),
    code_bg: RgbColor::new(31, 35, 53),
};

/// Signature tauqe light scheme: clean paper canvas with muted pastel accents.
const TAUQE_LIGHT: ThemePalette = ThemePalette {
    canvas: RgbColor::new(250, 249, 245),
    surface: RgbColor::new(242, 240, 233),
    overlay: RgbColor::new(232, 229, 220),
    border: RgbColor::new(201, 198, 187),
    text_primary: RgbColor::new(58, 58, 56),
    text_secondary: RgbColor::new(92, 92, 88),
    text_muted: RgbColor::new(138, 138, 132),
    text_inverse: RgbColor::new(250, 249, 245),
    button_bg: RgbColor::new(227, 224, 214),
    button_fg: RgbColor::new(58, 58, 56),
    button_active_bg: RgbColor::new(63, 111, 174),
    status_success: RgbColor::new(63, 125, 78),
    status_warning: RgbColor::new(169, 117, 42),
    status_error: RgbColor::new(184, 74, 74),
    status_info: RgbColor::new(63, 111, 174),
    diff_added: RgbColor::new(63, 125, 78),
    diff_removed: RgbColor::new(184, 74, 74),
    diff_context: RgbColor::new(138, 138, 132),
    code_bg: RgbColor::new(240, 238, 230),
};

/// Catppuccin Mocha.
const CATPPUCCIN_MOCHA: ThemePalette = ThemePalette {
    canvas: RgbColor::new(30, 30, 46),
    surface: RgbColor::new(49, 50, 68),
    overlay: RgbColor::new(69, 71, 90),
    border: RgbColor::new(88, 91, 112),
    text_primary: RgbColor::new(205, 214, 244),
    text_secondary: RgbColor::new(186, 194, 222),
    text_muted: RgbColor::new(127, 132, 156),
    text_inverse: RgbColor::new(17, 17, 27),
    button_bg: RgbColor::new(69, 71, 90),
    button_fg: RgbColor::new(205, 214, 244),
    button_active_bg: RgbColor::new(137, 180, 250),
    status_success: RgbColor::new(166, 227, 161),
    status_warning: RgbColor::new(249, 226, 175),
    status_error: RgbColor::new(243, 139, 168),
    status_info: RgbColor::new(137, 180, 250),
    diff_added: RgbColor::new(166, 227, 161),
    diff_removed: RgbColor::new(243, 139, 168),
    diff_context: RgbColor::new(108, 112, 134),
    code_bg: RgbColor::new(24, 24, 37),
};

/// Gruvbox dark: a warm, retro-inspired scheme.
const GRUVBOX_DARK: ThemePalette = ThemePalette {
    canvas: RgbColor::new(40, 40, 40),
    surface: RgbColor::new(60, 56, 54),
    overlay: RgbColor::new(80, 73, 69),
    border: RgbColor::new(102, 92, 84),
    text_primary: RgbColor::new(235, 219, 178),
    text_secondary: RgbColor::new(213, 196, 161),
    text_muted: RgbColor::new(146, 131, 116),
    text_inverse: RgbColor::new(40, 40, 40),
    button_bg: RgbColor::new(80, 73, 69),
    button_fg: RgbColor::new(235, 219, 178),
    button_active_bg: RgbColor::new(250, 189, 47),
    status_success: RgbColor::new(184, 187, 38),
    status_warning: RgbColor::new(250, 189, 47),
    status_error: RgbColor::new(251, 73, 52),
    status_info: RgbColor::new(131, 165, 152),
    diff_added: RgbColor::new(184, 187, 38),
    diff_removed: RgbColor::new(251, 73, 52),
    diff_context: RgbColor::new(146, 131, 116),
    code_bg: RgbColor::new(29, 32, 33),
};

/// Solarized Dark, recalibrated in true color: borders and buttons are darkened
/// and brightened so they keep reliable contrast against the base tones.
const SOLARIZED_DARK: ThemePalette = ThemePalette {
    canvas: RgbColor::new(0, 43, 54),
    surface: RgbColor::new(7, 54, 66),
    overlay: RgbColor::new(10, 74, 90),
    border: RgbColor::new(88, 110, 117),
    text_primary: RgbColor::new(147, 161, 161),
    text_secondary: RgbColor::new(131, 148, 150),
    text_muted: RgbColor::new(101, 123, 131),
    text_inverse: RgbColor::new(0, 43, 54),
    button_bg: RgbColor::new(7, 54, 66),
    button_fg: RgbColor::new(147, 161, 161),
    button_active_bg: RgbColor::new(38, 139, 210),
    status_success: RgbColor::new(133, 153, 0),
    status_warning: RgbColor::new(181, 137, 0),
    status_error: RgbColor::new(220, 50, 47),
    status_info: RgbColor::new(42, 161, 152),
    diff_added: RgbColor::new(133, 153, 0),
    diff_removed: RgbColor::new(220, 50, 47),
    diff_context: RgbColor::new(88, 110, 117),
    code_bg: RgbColor::new(0, 34, 43),
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::color::{ansi256_to_rgb, ColorDepth};
    use crate::theme::engine::ThemeEngine;
    use ratatui::style::{Color, Modifier};

    /// WCAG relative luminance of a color in `[0.0, 1.0]`.
    fn luminance(color: RgbColor) -> f64 {
        fn channel(value: u8) -> f64 {
            let c = f64::from(value) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
    }

    /// WCAG contrast ratio between two colors (>= 1.0).
    fn contrast(a: RgbColor, b: RgbColor) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (high, low) = if la >= lb { (la, lb) } else { (lb, la) };
        (high + 0.05) / (low + 0.05)
    }

    #[test]
    fn presets_are_pairwise_distinct() {
        for (index, preset) in ThemePreset::ALL.iter().enumerate() {
            for other in &ThemePreset::ALL[index + 1..] {
                assert_ne!(
                    preset.palette(),
                    other.palette(),
                    "{preset:?} and {other:?} share an identical palette"
                );
            }
        }
    }

    /// Projects a role color exactly as the terminal will paint it at `depth`.
    ///
    /// Truecolor keeps the authored RGB. The 256-color path quantizes onto the
    /// safe xterm cube and decodes the resulting index back to RGB, so the
    /// contrast of what the terminal actually draws is measured rather than the
    /// contrast of the source palette.
    fn project(color: RgbColor, depth: ColorDepth) -> RgbColor {
        match depth {
            ColorDepth::Ansi256 => ansi256_to_rgb(color.to_ansi256()),
            _ => color,
        }
    }

    /// Asserts every semantic role keeps its required WCAG contrast once
    /// projected onto `depth`.
    ///
    /// The thresholds encode intended usage: `text_primary` must clear the AA
    /// ratio for body text (4.5); secondary and status roles the large-text and
    /// non-text ratio (3.0); muted hints a lower bar (2.0); and a border only
    /// needs to be distinguishable from its own surface.
    fn assert_roles_legible(preset: ThemePreset, depth: ColorDepth) {
        let palette = preset.palette();
        let pair = |a: RgbColor, b: RgbColor| contrast(project(a, depth), project(b, depth));

        assert!(
            pair(palette.text_primary, palette.canvas) >= 4.5,
            "{preset:?}/{depth:?}: primary text is illegible on the canvas"
        );
        assert!(
            pair(palette.text_secondary, palette.canvas) >= 3.0,
            "{preset:?}/{depth:?}: secondary text is illegible on the canvas"
        );
        assert!(
            pair(palette.text_muted, palette.canvas) >= 2.0,
            "{preset:?}/{depth:?}: muted text is illegible on the canvas"
        );
        assert!(
            pair(palette.button_fg, palette.button_bg) >= 3.0,
            "{preset:?}/{depth:?}: resting button label lacks contrast"
        );
        assert!(
            pair(palette.text_inverse, palette.button_active_bg) >= 3.0,
            "{preset:?}/{depth:?}: active button label lacks contrast"
        );
        assert!(
            pair(palette.status_error, palette.canvas) >= 3.0,
            "{preset:?}/{depth:?}: error status lacks contrast"
        );
        assert!(
            pair(palette.status_success, palette.canvas) >= 3.0,
            "{preset:?}/{depth:?}: success status lacks contrast"
        );
        assert!(
            pair(palette.border, palette.surface) >= 1.2,
            "{preset:?}/{depth:?}: border is indistinguishable from its surface"
        );
    }

    #[test]
    fn presets_keep_roles_legible_across_their_palette() {
        for preset in ThemePreset::ALL {
            assert_roles_legible(preset, ColorDepth::Truecolor);
        }
    }

    #[test]
    fn ansi256_projection_preserves_role_contrast() {
        for preset in ThemePreset::ALL {
            assert_roles_legible(preset, ColorDepth::Ansi256);
        }
    }

    #[test]
    fn ansi16_fallback_conveys_roles_without_color() {
        for preset in ThemePreset::ALL {
            let engine = ThemeEngine::new(preset, ColorDepth::Ansi16);

            // No true or indexed color survives: the foreground falls back to the
            // terminal default and filled surfaces are marked with a reversed band,
            // so controls stay distinguishable where color cannot be trusted.
            assert!(engine.fg(preset.palette().text_primary).fg.is_none());
            assert_eq!(engine.color(preset.palette().border), Color::Reset);
            assert!(engine.bg(preset.palette().surface).bg.is_none());
            assert!(engine
                .bg(preset.palette().button_active_bg)
                .add_modifier
                .contains(Modifier::REVERSED));
        }
    }

    #[test]
    fn from_name_is_lenient_and_rejects_unknown_names() {
        assert_eq!(
            ThemePreset::from_name("tauqe-dark"),
            Some(ThemePreset::TauqeDark)
        );
        assert_eq!(
            ThemePreset::from_name("  Tauqe_Dark "),
            Some(ThemePreset::TauqeDark)
        );
        assert_eq!(
            ThemePreset::from_name("SOLARIZED-DARK"),
            Some(ThemePreset::SolarizedDark)
        );
        assert_eq!(ThemePreset::from_name("gruvbox"), Some(ThemePreset::Gruvbox));
        assert_eq!(ThemePreset::from_name("nope"), None);
    }

    #[test]
    fn identifiers_round_trip_through_from_name() {
        for preset in ThemePreset::ALL {
            assert_eq!(ThemePreset::from_name(preset.as_str()), Some(preset));
        }
    }

    #[test]
    fn resolve_prefers_a_named_preset_then_falls_back_to_mode() {
        assert_eq!(
            ThemePreset::resolve(Some("catppuccin"), ThemeMode::Light),
            ThemePreset::Catppuccin
        );
        assert_eq!(
            ThemePreset::resolve(Some("unknown"), ThemeMode::Dark),
            ThemePreset::TauqeDark
        );
        assert_eq!(
            ThemePreset::resolve(None, ThemeMode::Dark),
            ThemePreset::TauqeDark
        );
        assert_eq!(
            ThemePreset::resolve(None, ThemeMode::Light),
            ThemePreset::TauqeLight
        );
    }
}
