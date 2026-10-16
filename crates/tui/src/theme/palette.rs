//! Semantic design tokens consumed by every TUI surface.
//!
//! Components reference semantic roles such as `border` or `status_error`
//! instead of literal ANSI colors, so a single palette swap restyles the whole
//! interface and the projection engine can degrade every role consistently
//! across terminal color depths.

use crate::theme::color::RgbColor;

/// The full set of semantic color roles a theme defines in 24-bit sRGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePalette {
    /// Base canvas behind every view.
    pub canvas: RgbColor,
    /// Raised panels, popups, and the prompt box.
    pub surface: RgbColor,
    /// Overlay emphasis such as selection rows.
    pub overlay: RgbColor,
    /// Panel and table borders.
    pub border: RgbColor,
    /// Primary body text.
    pub text_primary: RgbColor,
    /// Secondary text such as labels and metadata.
    pub text_secondary: RgbColor,
    /// De-emphasized text such as hints and disabled entries.
    pub text_muted: RgbColor,
    /// Text drawn over a filled, high-contrast surface.
    pub text_inverse: RgbColor,
    /// Resting button background.
    pub button_bg: RgbColor,
    /// Resting button label.
    pub button_fg: RgbColor,
    /// Focused or active button background.
    pub button_active_bg: RgbColor,
    /// Success status.
    pub status_success: RgbColor,
    /// Warning status.
    pub status_warning: RgbColor,
    /// Error status.
    pub status_error: RgbColor,
    /// Informational status.
    pub status_info: RgbColor,
    /// Added lines in a diff.
    pub diff_added: RgbColor,
    /// Removed lines in a diff.
    pub diff_removed: RgbColor,
    /// Context lines in a diff.
    pub diff_context: RgbColor,
    /// Background of fenced code blocks.
    pub code_bg: RgbColor,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::color::{ColorDepth, ColorProjector};
    use ratatui::style::{Color, Modifier};

    fn sample_palette() -> ThemePalette {
        ThemePalette {
            canvas: RgbColor::new(20, 22, 30),
            surface: RgbColor::new(28, 30, 40),
            overlay: RgbColor::new(40, 44, 58),
            border: RgbColor::new(70, 78, 100),
            text_primary: RgbColor::new(220, 225, 235),
            text_secondary: RgbColor::new(170, 178, 192),
            text_muted: RgbColor::new(120, 128, 145),
            text_inverse: RgbColor::new(16, 18, 24),
            button_bg: RgbColor::new(48, 54, 72),
            button_fg: RgbColor::new(225, 230, 240),
            button_active_bg: RgbColor::new(80, 120, 200),
            status_success: RgbColor::new(120, 200, 140),
            status_warning: RgbColor::new(230, 190, 100),
            status_error: RgbColor::new(230, 110, 110),
            status_info: RgbColor::new(120, 170, 230),
            diff_added: RgbColor::new(110, 180, 120),
            diff_removed: RgbColor::new(220, 120, 120),
            diff_context: RgbColor::new(120, 128, 145),
            code_bg: RgbColor::new(24, 27, 36),
        }
    }

    #[test]
    fn semantic_roles_flow_through_every_projection_depth() {
        let palette = sample_palette();

        let truecolor = ColorProjector::new(ColorDepth::Truecolor);
        assert_eq!(
            truecolor.fg(palette.border, Modifier::BOLD).fg,
            Some(Color::Rgb(70, 78, 100))
        );

        let ansi256 = ColorProjector::new(ColorDepth::Ansi256);
        assert!(matches!(
            ansi256.fg(palette.border, Modifier::BOLD).fg,
            Some(Color::Indexed(_))
        ));

        let ansi16 = ColorProjector::new(ColorDepth::Ansi16);
        let style = ansi16.fg(palette.border, Modifier::BOLD);
        assert_eq!(style.fg, None);
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }
}
