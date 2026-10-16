//! Active theme resolution and semantic color projection.
//!
//! The engine is the single point where the configured theme becomes concrete
//! ratatui styles. It resolves the active preset into a semantic
//! [`ThemePalette`] and projects every role onto the terminal's color depth
//! through [`ColorProjector`], so rendering surfaces never hardcode ANSI colors
//! (`#c-1`, `#c-2`).
//!
//! The active preset and color depth are process-wide, mirroring the existing
//! detection globals: the configuration layer sets them once and every surface
//! reads the same resolved theme, so a palette swap restyles the whole interface
//! without threading a theme through every render call.

use std::sync::atomic::{AtomicU8, Ordering};

use ratatui::style::{Color, Modifier, Style};

use crate::config::{ColorDepth as ConfiguredDepth, ThemeMode};
use crate::theme::color::{ColorDepth, ColorProjector, RgbColor};
use crate::theme::palette::ThemePalette;
use crate::theme::presets::ThemePreset;

/// Active preset selector: `0` derives the preset from the light/dark mode,
/// while `1..=N` selects an entry of [`ThemePreset::ALL`] by index.
static ACTIVE_PRESET: AtomicU8 = AtomicU8::new(0);

/// Active color depth: `0` auto-detects, `1` forces truecolor, `2` forces the
/// xterm 256-color palette, and `3` forces the 16-color fallback.
static ACTIVE_DEPTH: AtomicU8 = AtomicU8::new(0);

/// Sets the color depth from the configured override.
pub fn set_color_depth(configured: ConfiguredDepth) {
    let value = match configured {
        ConfiguredDepth::Auto => 0,
        ConfiguredDepth::Truecolor => 1,
        ConfiguredDepth::Ansi256 => 2,
        ConfiguredDepth::Ansi16 => 3,
    };
    ACTIVE_DEPTH.store(value, Ordering::Relaxed);
}

/// Sets the active preset explicitly, or `None` to derive it from the mode.
pub fn set_active_preset(preset: Option<ThemePreset>) {
    let value = match preset {
        None => 0,
        Some(preset) => ThemePreset::ALL
            .iter()
            .position(|candidate| *candidate == preset)
            .map_or(0, |index| index as u8 + 1),
    };
    ACTIVE_PRESET.store(value, Ordering::Relaxed);
}

/// Resolves the active preset for the given fallback mode.
fn active_preset(mode: ThemeMode) -> ThemePreset {
    match ACTIVE_PRESET.load(Ordering::Relaxed) {
        0 => ThemePreset::resolve(None, mode),
        index if (index as usize) <= ThemePreset::ALL.len() => ThemePreset::ALL[index as usize - 1],
        _ => ThemePreset::resolve(None, mode),
    }
}

/// Resolves the active color depth.
fn active_depth() -> ColorDepth {
    let configured = match ACTIVE_DEPTH.load(Ordering::Relaxed) {
        1 => ConfiguredDepth::Truecolor,
        2 => ConfiguredDepth::Ansi256,
        3 => ConfiguredDepth::Ansi16,
        _ => ConfiguredDepth::Auto,
    };
    ColorDepth::resolve(configured)
}

/// The active color depth, exposed for callers that project raw colors.
pub fn current_depth() -> ColorDepth {
    active_depth()
}

/// Builds the engine for the active preset, using `mode` when no preset is set.
pub fn active_engine(mode: ThemeMode) -> ThemeEngine {
    ThemeEngine::new(active_preset(mode), active_depth())
}

/// Builds the engine for surfaces that do not track a light/dark mode,
/// deriving the fallback from the terminal background.
pub fn current_engine() -> ThemeEngine {
    active_engine(ThemeMode::Auto)
}

/// A semantic palette paired with the projector for the active depth.
#[derive(Debug, Clone, Copy)]
pub struct ThemeEngine {
    preset: ThemePreset,
    palette: ThemePalette,
    projector: ColorProjector,
}

impl ThemeEngine {
    /// Creates an engine for a preset at a concrete color depth.
    pub fn new(preset: ThemePreset, depth: ColorDepth) -> Self {
        Self {
            preset,
            palette: preset.palette(),
            projector: ColorProjector::new(depth),
        }
    }

    /// The preset this engine renders.
    pub fn preset(&self) -> ThemePreset {
        self.preset
    }

    /// The semantic palette this engine renders.
    pub fn palette(&self) -> ThemePalette {
        self.palette
    }

    /// The color depth this engine targets.
    pub fn depth(&self) -> ColorDepth {
        self.projector.depth()
    }

    /// Whether the palette targets a light terminal background.
    pub fn is_light(&self) -> bool {
        relative_luminance(self.palette.canvas) > 0.5
    }

    /// Projects a role color as a flat foreground [`Color`] (no modifiers).
    pub fn color(&self, color: RgbColor) -> Color {
        self.projector
            .fg(color, Modifier::empty())
            .fg
            .unwrap_or(Color::Reset)
    }

    /// Projects a role color as a foreground style.
    ///
    /// At the 16-color fallback the color is dropped and the default foreground
    /// is used; callers add emphasis attributes explicitly.
    pub fn fg(&self, color: RgbColor) -> Style {
        self.projector.fg(color, Modifier::empty())
    }

    /// Projects a role color as a background style.
    ///
    /// At the 16-color fallback the surface is conveyed by a reversed attribute,
    /// so filled controls (buttons) stay distinguishable without color.
    pub fn bg(&self, color: RgbColor) -> Style {
        self.projector.bg(color, Modifier::REVERSED)
    }
}

/// WCAG relative luminance of a color in `[0.0, 1.0]`.
fn relative_luminance(color: RgbColor) -> f64 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_projects_roles_at_each_depth() {
        let palette = ThemePreset::TauqeDark.palette();

        let truecolor = ThemeEngine::new(ThemePreset::TauqeDark, ColorDepth::Truecolor);
        assert_eq!(
            truecolor.color(palette.border),
            Color::Rgb(palette.border.r, palette.border.g, palette.border.b),
        );

        let ansi256 = ThemeEngine::new(ThemePreset::TauqeDark, ColorDepth::Ansi256);
        assert!(matches!(ansi256.color(palette.border), Color::Indexed(_)));

        let fallback = ThemeEngine::new(ThemePreset::TauqeDark, ColorDepth::Ansi16);
        assert_eq!(fallback.color(palette.border), Color::Reset);
        assert_eq!(fallback.fg(palette.border).fg, None);
        assert!(fallback
            .bg(palette.border)
            .add_modifier
            .contains(Modifier::REVERSED));
    }

    #[test]
    fn engine_detects_light_and_dark_canvases() {
        assert!(!ThemeEngine::new(ThemePreset::TauqeDark, ColorDepth::Truecolor).is_light());
        assert!(ThemeEngine::new(ThemePreset::TauqeLight, ColorDepth::Truecolor).is_light());
    }
}
