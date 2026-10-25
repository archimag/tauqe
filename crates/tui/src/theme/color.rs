//! 24-bit theme colors and their projection onto terminal color depths.
//!
//! Themes are authored in true sRGB. The rendering engine projects every token
//! onto the depth the terminal actually supports, degrading gracefully from
//! 24-bit RGB down to an attribute-based fallback. The lowest 256-color indices
//! (0..=15) are never emitted: terminals remap them to user-configurable host
//! slots, which can render a semantic role invisible or illegible under palettes
//! such as Solarized.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use ratatui::style::{Color, Modifier, Style};

/// The six channel levels of the xterm 6x6x6 color cube.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The first xterm 256-color index that is safe to emit.
///
/// Indices 0..=15 alias the terminal's remappable system palette, so every
/// projection is confined to 16..=255.
const SAFE_ANSI256_START: u8 = 16;

/// Process-wide memo of nearest safe xterm 256-color indices.
///
/// Projection is deterministic and pure, so each distinct color scans the
/// 240 safe candidates at most once per process; the per-span projections
/// of every rendered frame reuse the cached index instead of rescanning.
static ANSI256_NEAREST: LazyLock<Mutex<HashMap<RgbColor, u8>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A 24-bit sRGB color authored in a theme palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RgbColor {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl RgbColor {
    /// Creates a color from its red, green, and blue channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Projects this color onto the nearest xterm 256-color index.
    ///
    /// The search covers the 6x6x6 cube and the grayscale ramp (indices
    /// 16..=255) and never the remappable system slots 0..=15, so the result is
    /// stable regardless of the terminal's host palette. Results are memoized
    /// process-wide through [`ANSI256_NEAREST`]: every distinct color scans at
    /// most once, and the repeated per-span projections of a streamed frame
    /// reuse the cached index instead of rescanning.
    pub fn to_ansi256(self) -> u8 {
        match ANSI256_NEAREST.lock() {
            Ok(mut cache) => *cache.entry(self).or_insert_with(|| self.scan_ansi256()),
            // A poisoned guard cannot be recovered here; degrade to a
            // direct, uncached scan so the projection stays correct.
            Err(_) => self.scan_ansi256(),
        }
    }

    /// Scans every safe xterm index for the nearest match to this color.
    fn scan_ansi256(self) -> u8 {
        (SAFE_ANSI256_START..=u8::MAX)
            .min_by_key(|&index| self.distance_squared(ansi256_to_rgb(index)))
            .unwrap_or(SAFE_ANSI256_START)
    }

    /// Squared euclidean distance to another color, in RGB space.
    fn distance_squared(self, other: RgbColor) -> u32 {
        let dr = u32::from(self.r.abs_diff(other.r));
        let dg = u32::from(self.g.abs_diff(other.g));
        let db = u32::from(self.b.abs_diff(other.b));
        dr * dr + dg * dg + db * db
    }
}

/// Converts an xterm 256-color index in 16..=255 back into its RGB value.
///
/// Visible to sibling theme modules so palette calibration can measure the color
/// a terminal actually paints after projection, instead of duplicating the cube
/// and grayscale arithmetic (single source of truth, Conventions §1.3).
pub(super) fn ansi256_to_rgb(index: u8) -> RgbColor {
    match index {
        16..=231 => {
            let offset = index - SAFE_ANSI256_START;
            let r = CUBE_LEVELS[usize::from(offset / 36)];
            let g = CUBE_LEVELS[usize::from((offset % 36) / 6)];
            let b = CUBE_LEVELS[usize::from(offset % 6)];
            RgbColor::new(r, g, b)
        }
        232..=255 => {
            let level = 8 + 10 * (index - 232);
            RgbColor::new(level, level, level)
        }
        _ => RgbColor::new(0, 0, 0),
    }
}

/// The terminal color capability the projection engine targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit direct color.
    Truecolor,
    /// The fixed xterm 256-color palette (indices 16..=255).
    Ansi256,
    /// A 16-color fallback that conveys roles through text attributes.
    Ansi16,
}

impl ColorDepth {
    /// Resolves the effective depth from the configured override.
    ///
    /// [`crate::config::ColorDepth::Auto`] probes the environment; the explicit
    /// variants force the corresponding depth.
    pub fn resolve(configured: crate::config::ColorDepth) -> Self {
        match configured {
            crate::config::ColorDepth::Truecolor => ColorDepth::Truecolor,
            crate::config::ColorDepth::Ansi256 => ColorDepth::Ansi256,
            crate::config::ColorDepth::Ansi16 => ColorDepth::Ansi16,
            crate::config::ColorDepth::Auto => ColorDepth::detect_from_env(),
        }
    }

    /// Best-effort detection of the terminal's depth from the environment.
    fn detect_from_env() -> Self {
        if env_reports_truecolor() {
            ColorDepth::Truecolor
        } else if env_reports_256_colors() {
            ColorDepth::Ansi256
        } else {
            ColorDepth::Ansi16
        }
    }
}

/// Whether `COLORTERM` or `TERM` advertises 24-bit color support.
fn env_reports_truecolor() -> bool {
    let colorterm_is_truecolor = std::env::var("COLORTERM").is_ok_and(|value| {
        let value = value.to_lowercase();
        value == "truecolor" || value == "24bit"
    });
    colorterm_is_truecolor
        || std::env::var("TERM").is_ok_and(|term| {
            let term = term.to_lowercase();
            term.contains("kitty") || term.contains("alacritty") || term.contains("wezterm")
        })
}

/// Whether `TERM` advertises a 256-color terminal.
fn env_reports_256_colors() -> bool {
    std::env::var("TERM").is_ok_and(|term| term.to_lowercase().contains("256color"))
}

/// Projects theme colors onto a concrete terminal color depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorProjector {
    depth: ColorDepth,
}

impl ColorProjector {
    /// Creates a projector targeting `depth`.
    pub fn new(depth: ColorDepth) -> Self {
        Self { depth }
    }

    /// Returns the depth this projector targets.
    pub fn depth(self) -> ColorDepth {
        self.depth
    }

    /// Projects `color` as a foreground style.
    ///
    /// At [`ColorDepth::Ansi16`] the true color is discarded and `fallback`
    /// attributes (typically [`Modifier::BOLD`]) carry the emphasis, keeping the
    /// text legible under an arbitrary host palette.
    pub fn fg(self, color: RgbColor, fallback: Modifier) -> Style {
        match self.depth {
            ColorDepth::Truecolor => Style::default().fg(Color::Rgb(color.r, color.g, color.b)),
            ColorDepth::Ansi256 => Style::default().fg(Color::Indexed(color.to_ansi256())),
            ColorDepth::Ansi16 => Style::default().add_modifier(fallback),
        }
    }

    /// Projects `color` as a background style.
    ///
    /// At [`ColorDepth::Ansi16`] the true color is discarded and `fallback`
    /// attributes (typically [`Modifier::REVERSED`]) mark the surface, so filled
    /// panels stay visible without relying on remappable palette slots.
    pub fn bg(self, color: RgbColor, fallback: Modifier) -> Style {
        match self.depth {
            ColorDepth::Truecolor => Style::default().bg(Color::Rgb(color.r, color.g, color.b)),
            ColorDepth::Ansi256 => Style::default().bg(Color::Indexed(color.to_ansi256())),
            ColorDepth::Ansi16 => Style::default().add_modifier(fallback),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_COLORS: [RgbColor; 8] = [
        RgbColor::new(0, 0, 0),
        RgbColor::new(255, 255, 255),
        RgbColor::new(128, 128, 128),
        RgbColor::new(255, 0, 0),
        RgbColor::new(0, 255, 0),
        RgbColor::new(0, 0, 255),
        RgbColor::new(1, 2, 3),
        RgbColor::new(250, 12, 200),
    ];

    #[test]
    fn ansi256_projection_never_uses_remappable_slots() {
        for color in SAMPLE_COLORS {
            let index = color.to_ansi256();
            assert!(
                index >= SAFE_ANSI256_START,
                "{color:?} projected to remappable slot {index}"
            );
        }
    }

    #[test]
    fn ansi256_projection_matches_known_cube_and_grayscale_indices() {
        assert_eq!(RgbColor::new(0, 0, 0).to_ansi256(), 16);
        assert_eq!(RgbColor::new(255, 255, 255).to_ansi256(), 231);
        assert_eq!(RgbColor::new(255, 0, 0).to_ansi256(), 196);
        assert_eq!(RgbColor::new(128, 128, 128).to_ansi256(), 244);
    }

    #[test]
    fn ansi256_indices_round_trip_through_rgb() {
        for index in SAFE_ANSI256_START..=u8::MAX {
            let rgb = ansi256_to_rgb(index);
            assert_eq!(rgb.to_ansi256(), index, "index {index} did not round-trip");
        }
    }

    #[test]
    fn memoized_projection_is_byte_identical_to_a_direct_scan() {
        // The first projection populates the memo and later ones read it;
        // every path must equal an uncached scan of the same candidates.
        for color in SAMPLE_COLORS {
            let memoized = color.to_ansi256();
            assert_eq!(
                memoized,
                color.scan_ansi256(),
                "{color:?} memoized to a different index than a direct scan"
            );
        }
    }

    #[test]
    fn truecolor_projection_returns_exact_rgb() {
        let projector = ColorProjector::new(ColorDepth::Truecolor);
        let style = projector.fg(RgbColor::new(12, 34, 56), Modifier::BOLD);
        assert_eq!(style.fg, Some(Color::Rgb(12, 34, 56)));
        assert!(!style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn ansi256_projection_returns_indexed_color() {
        let projector = ColorProjector::new(ColorDepth::Ansi256);
        let style = projector.fg(RgbColor::new(12, 34, 56), Modifier::BOLD);
        match style.fg {
            Some(Color::Indexed(index)) => assert!(index >= SAFE_ANSI256_START),
            other => panic!("expected an indexed color, got {other:?}"),
        }
    }

    #[test]
    fn ansi16_fallback_conveys_roles_through_attributes() {
        let projector = ColorProjector::new(ColorDepth::Ansi16);

        let foreground = projector.fg(RgbColor::new(12, 34, 56), Modifier::BOLD);
        assert_eq!(foreground.fg, None);
        assert!(foreground.add_modifier.contains(Modifier::BOLD));

        let background = projector.bg(RgbColor::new(12, 34, 56), Modifier::REVERSED);
        assert_eq!(background.bg, None);
        assert!(background.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn resolve_maps_explicit_overrides_to_matching_depths() {
        assert_eq!(
            ColorDepth::resolve(crate::config::ColorDepth::Truecolor),
            ColorDepth::Truecolor
        );
        assert_eq!(
            ColorDepth::resolve(crate::config::ColorDepth::Ansi256),
            ColorDepth::Ansi256
        );
    }
}
