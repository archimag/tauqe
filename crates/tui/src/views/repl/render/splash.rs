//! SLIME-style startup greeting for an empty Develop viewport.
//!
//! The greeting is transient: it occupies the response area only while no model
//! output exists and yields to the regular viewport on the first byte of output,
//! so prompt focus and input are never blocked (`#c-1`). Every role it paints
//! resolves through the active semantic palette and the typewriter reads the
//! progress reported by [`ReplView::splash_typing_progress`], keeping the shared
//! timer loop the single animation driver (`#c-2`).
//!
//! While the glyphs assemble, the five monoliths of the wordmark converge from
//! their start displacements toward their final slots with a cubic ease-out. The
//! whole frame is composed in a bounds-checked character grid, so a glyph flying
//! in from outside the viewport is clipped instead of panicking or overwriting
//! the motto line (`#c-glyph-convergence`).

use crossterm::event::KeyCode;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::command::KeyChord;
use crate::config::{active_meta_modifier, active_primary_modifier};
use crate::i18n::active_t;
use crate::theme::engine::{current_engine, ThemeEngine};

use super::super::state::ReplView;

/// Minimum viewport width in columns that fits the block-letter wordmark.
const LOGO_MIN_WIDTH: u16 = 45;

/// Product name used instead of the wordmark on a narrow viewport.
const PRODUCT_NAME: &str = "TAUQE";

/// Cursor glyph appended while the motto is still being typed out.
const TYPEWRITER_CURSOR: &str = "▌";

/// Block-letter `TAUQE` wordmark.
///
/// The wordmark is decorative interface chrome rather than prose: it carries no
/// translatable content, so it is exempt from localization (Conventions §9.4).
const LOGO: [&str; 6] = [
    "████████╗ █████╗ ██╗   ██╗ ██████╗ ███████╗",
    "╚══██╔══╝██╔══██╗██║   ██║██╔═══██╗██╔════╝",
    "   ██║   ███████║██║   ██║██║   ██║█████╗  ",
    "   ██║   ██╔══██║██║   ██║██║▄▄ ██║██╔══╝  ",
    "   ██║   ██║  ██║╚██████╔╝╚██████╔╝███████╗",
    "   ╚═╝   ╚═╝  ╚═╝ ╚═════╝  ╚══▀▀═╝ ╚══════╝",
];

/// Column width of each wordmark glyph in order: `T`, `A`, `U`, `Q`, `E`.
///
/// The widths are fixed, so a glyph is a stable slice of every wordmark row and
/// can be displaced independently without re-measuring the block letters.
const GLYPH_WIDTHS: [usize; 5] = [9, 8, 9, 9, 8];

/// Total wordmark width in columns.
const LOGO_WIDTH: usize = 43;

/// Starting displacement of each glyph in terminal cells: `(dx, dy)`.
///
/// The outer glyphs enter from the sides, the inner ones from the top and the
/// bottom, so the five pieces visibly converge on the center. Downward travel is
/// capped at one row: anything lower would collide with the motto line that owns
/// the row directly beneath the blank spacer.
const GLYPH_START_OFFSETS: [(i16, i16); 5] = [(-16, -3), (-8, 1), (0, -3), (8, 1), (16, -3)];

/// Rows occupied by the greeting while the glyphs assemble: the wordmark, the
/// blank spacer, and the motto line.
const ASSEMBLY_BLOCK_HEIGHT: usize = LOGO.len() + 2;

/// Renders the centered greeting inside the empty response area.
///
/// The block is centered both vertically and horizontally in the viewport, so
/// the greeting stays visually anchored regardless of terminal size. Nothing is
/// drawn when the area has no room, keeping tiny panes free of clipped noise.
pub(super) fn render_splash(frame: &mut ratatui::Frame, model: &ReplView, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // A narrow viewport has no room for the wordmark, and a completed assembly
    // has nothing left to animate: both cases render the settled greeting.
    if area.width < LOGO_MIN_WIDTH || model.splash_assembly_done() {
        render_static_greeting(frame, model, area);
        return;
    }

    frame.render_widget(Paragraph::new(kinetic_lines(model, area)), area);
}

/// Renders the settled greeting: wordmark, typed motto, and quick-start hints.
fn render_static_greeting(frame: &mut ratatui::Frame, model: &ReplView, area: Rect) {
    let lines = splash_lines(model, area.width);
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    let top = area.height.saturating_sub(height) / 2;
    let centered = Rect::new(area.x, area.y.saturating_add(top), area.width, height);

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), centered);
}

/// Builds the greeting lines: wordmark, typed motto, and quick-start hints.
///
/// The quick-start hints stay hidden until the typewriter has revealed the whole
/// motto, so the greeting assembles itself instead of appearing at once.
fn splash_lines(model: &ReplView, width: u16) -> Vec<Line<'static>> {
    let engine = current_engine();
    let palette = engine.palette();

    let mut lines: Vec<Line<'static>> = Vec::new();

    if width >= LOGO_MIN_WIDTH {
        let logo_style = engine.fg(palette.status_info);
        for row in LOGO {
            lines.push(Line::from(Span::styled(row, logo_style)));
        }
    } else {
        lines.push(Line::from(Span::styled(
            PRODUCT_NAME,
            engine.fg(palette.status_info).bold(),
        )));
    }

    lines.push(Line::raw(""));

    let revealed = typewriter_prefix(&active_t("splash.motto"), model.splash_typing_progress());
    let mut motto_spans = vec![Span::styled(revealed, engine.fg(palette.text_primary))];
    if !model.splash_typing_done() {
        motto_spans.push(Span::styled(
            TYPEWRITER_CURSOR,
            engine.fg(palette.button_active_bg).bold(),
        ));
    }
    lines.push(Line::from(motto_spans));

    if model.splash_typing_done() {
        lines.push(Line::raw(""));
        lines.push(hint_line(&engine));
    }

    lines
}

/// Builds the full-width kinetic frame for the assembly phase.
///
/// The wordmark is placed exactly where the settled greeting will render it, so
/// the last assembly frame and the first typewriter frame coincide and the
/// hand-over cannot jump. The frame is emitted as `area.height` lines of clipped
/// text, painted left-aligned because the glyph columns are absolute.
fn kinetic_lines(model: &ReplView, area: Rect) -> Vec<Line<'static>> {
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    let top = usize::from(area.height.saturating_sub(ASSEMBLY_BLOCK_HEIGHT as u16) / 2);
    let left = width.saturating_sub(LOGO_WIDTH) / 2;

    let grid = assemble_logo_grid(model.splash_assembly_progress(), width, height, top, left);

    let engine = current_engine();
    let palette = engine.palette();
    let logo_style = engine.fg(palette.status_info);
    let motto_row = top + LOGO.len() + 1;

    let mut lines = Vec::with_capacity(height);
    for (row, cells) in grid.iter().enumerate() {
        if row == motto_row {
            lines.push(assembly_motto_line(model, &engine, width));
        } else {
            let text: String = cells.iter().collect();
            lines.push(Line::from(Span::styled(
                text.trim_end().to_string(),
                logo_style,
            )));
        }
    }
    lines
}

/// Composes the kinetic frame into a bounds-checked character grid.
///
/// `top` and `left` are the final top-left corner of the assembled wordmark.
/// Every glyph cell is clipped individually, so a glyph that enters from outside
/// the viewport is partially drawn instead of being dropped at once or writing
/// past the terminal edge (`#c-glyph-convergence`).
fn assemble_logo_grid(
    progress: f32,
    width: usize,
    height: usize,
    top: usize,
    left: usize,
) -> Vec<Vec<char>> {
    let mut grid = vec![vec![' '; width]; height];

    for glyph in 0..GLYPH_WIDTHS.len() {
        let (dx, dy) = glyph_offset(progress, glyph);
        let base_col = left as i32 + glyph_start_col(glyph) as i32;

        for (row, logo_row) in LOGO.iter().enumerate() {
            let target_row = top as i32 + row as i32 + dy;
            if target_row < 0 || target_row >= height as i32 {
                continue;
            }

            for (column, glyph_char) in glyph_slice(logo_row, glyph).into_iter().enumerate() {
                let target_col = base_col + column as i32 + dx;
                if target_col < 0 || target_col >= width as i32 {
                    continue;
                }
                grid[target_row as usize][target_col as usize] = glyph_char;
            }
        }
    }

    grid
}

/// Builds the centered motto line that sits beneath the assembling wordmark.
///
/// During the assembly phase the typewriter has revealed nothing yet, so this is
/// the blinking cursor alone; the same routine keeps the line in place at the
/// instant the second phase starts, with no layout shift.
fn assembly_motto_line(model: &ReplView, engine: &ThemeEngine, width: usize) -> Line<'static> {
    let palette = engine.palette();
    let revealed = typewriter_prefix(&active_t("splash.motto"), model.splash_typing_progress());
    let typing_done = model.splash_typing_done();

    let cursor_width = usize::from(!typing_done);
    let line_width = revealed.chars().count().saturating_add(cursor_width);
    let padding = width.saturating_sub(line_width) / 2;

    let mut spans = vec![Span::raw(" ".repeat(padding))];
    spans.push(Span::styled(revealed, engine.fg(palette.text_primary)));
    if !typing_done {
        spans.push(Span::styled(
            TYPEWRITER_CURSOR,
            engine.fg(palette.button_active_bg).bold(),
        ));
    }
    Line::from(spans)
}

/// Cubic ease-out: fast at the start, gently settling at the end.
///
/// The input is clamped to `0.0..=1.0`, so an early or overshooting progress can
/// never produce a displacement beyond the configured start offset.
fn ease_out_cubic(progress: f32) -> f32 {
    let remaining = 1.0 - progress.clamp(0.0, 1.0);
    1.0 - remaining * remaining * remaining
}

/// Current `(dx, dy)` displacement of `glyph` at `progress`.
fn glyph_offset(progress: f32, glyph: usize) -> (i32, i32) {
    let (dx, dy) = GLYPH_START_OFFSETS.get(glyph).copied().unwrap_or((0, 0));
    let remaining = 1.0 - ease_out_cubic(progress);
    (
        (f32::from(dx) * remaining).round() as i32,
        (f32::from(dy) * remaining).round() as i32,
    )
}

/// Column where `glyph` starts inside the assembled wordmark.
fn glyph_start_col(glyph: usize) -> usize {
    GLYPH_WIDTHS.iter().take(glyph).sum()
}

/// Extracts the `glyph` slice of one wordmark row.
///
/// The slice is taken character by character, so a padded or malformed row can
/// never panic on a byte boundary and simply yields a shorter slice
/// (Conventions §4.2).
fn glyph_slice(row: &str, glyph: usize) -> Vec<char> {
    let width = GLYPH_WIDTHS.get(glyph).copied().unwrap_or(0);
    row.chars()
        .skip(glyph_start_col(glyph))
        .take(width)
        .collect()
}

/// Builds the quick-start hint row from the shared chord notation.
///
/// Chords are projected through the configured command modifiers
/// ([`KeyChord::display`]), so the greeting always names the developer's real
/// keys instead of an abstract notation.
fn hint_line(engine: &ThemeEngine) -> Line<'static> {
    let palette = engine.palette();
    let (primary, meta) = (active_primary_modifier(), active_meta_modifier());
    let chord_style = engine.fg(palette.text_secondary).bold();
    let verb_style = engine.fg(palette.text_muted);
    let separator_style = engine.fg(palette.text_muted);

    let hints = [
        (
            KeyChord::Primary(KeyCode::Char('h')).display(primary, meta),
            active_t("splash.hint.help"),
        ),
        (
            KeyChord::Primary(KeyCode::Enter).display(primary, meta),
            active_t("splash.hint.send"),
        ),
    ];

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, (chord, verb)) in hints.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  •  ", separator_style));
        }
        spans.push(Span::styled(chord, chord_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(verb, verb_style));
    }

    Line::from(spans)
}

/// Reveals `progress` of `text`, always cutting on a Unicode scalar boundary.
///
/// Slicing by byte offset would panic inside a multi-byte character, so the
/// prefix is taken character by character (Conventions §4.2). A progress of
/// `0.0` yields an empty string and `1.0` the whole text.
fn typewriter_prefix(text: &str, progress: f32) -> String {
    let total = text.chars().count();
    if total == 0 {
        return String::new();
    }
    let revealed = (progress.clamp(0.0, 1.0) * total as f32).ceil() as usize;
    text.chars().take(revealed.min(total)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{Duration, Instant};

    /// A view whose typewriter budget has already been spent.
    fn finished_splash() -> ReplView {
        let started = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .expect("system uptime exceeds the splash budget");
        ReplView {
            splash_started: started,
            ..ReplView::default()
        }
    }

    #[test]
    fn typewriter_prefix_reveals_text_on_char_boundaries() {
        let text = "Инженерное дело";

        assert_eq!(typewriter_prefix(text, 0.0), "");
        assert_eq!(typewriter_prefix(text, 1.0), text);

        let half = typewriter_prefix(text, 0.5);
        assert!(text.starts_with(&half));
        let revealed = half.chars().count();
        assert!(revealed > 0 && revealed < text.chars().count());
    }

    #[test]
    fn typewriter_prefix_clamps_out_of_range_progress() {
        let text = "TAUQE";

        assert_eq!(typewriter_prefix(text, -1.0), "");
        assert_eq!(typewriter_prefix(text, 4.0), text);
        assert_eq!(typewriter_prefix("", 0.5), "");
    }

    #[test]
    fn greeting_shows_the_wordmark_on_a_wide_viewport() {
        let lines = splash_lines(&finished_splash(), LOGO_MIN_WIDTH);
        let rendered: String = lines
            .first()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .unwrap_or_default();

        assert_eq!(rendered, LOGO[0]);
    }

    #[test]
    fn greeting_falls_back_to_the_product_name_on_a_narrow_viewport() {
        let lines = splash_lines(&finished_splash(), 20);
        let rendered: String = lines
            .first()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .unwrap_or_default();

        assert_eq!(rendered, PRODUCT_NAME);
    }

    #[test]
    fn quick_start_hints_appear_only_after_the_typewriter_finishes() {
        let typing = splash_lines(&ReplView::default(), 80);
        let finished = splash_lines(&finished_splash(), 80);

        assert!(finished.len() > typing.len());
    }

    #[test]
    fn glyph_widths_tile_the_wordmark_exactly() {
        assert_eq!(GLYPH_WIDTHS.iter().sum::<usize>(), LOGO_WIDTH);
        for row in LOGO {
            assert_eq!(row.chars().count(), LOGO_WIDTH);
        }
    }

    #[test]
    fn every_glyph_slice_keeps_its_declared_width() {
        for (glyph, width) in GLYPH_WIDTHS.iter().enumerate() {
            for row in LOGO {
                assert_eq!(glyph_slice(row, glyph).len(), *width);
            }
        }
    }

    #[test]
    fn ease_out_cubic_is_monotonic_and_clamped() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert!((ease_out_cubic(1.0) - 1.0).abs() < f32::EPSILON);
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert!((ease_out_cubic(4.0) - 1.0).abs() < f32::EPSILON);

        let mut previous = ease_out_cubic(0.0);
        for step in 1..=20 {
            let value = ease_out_cubic(step as f32 / 20.0);
            assert!(value >= previous);
            previous = value;
        }

        // Ease-out front-loads the motion: the midpoint is already past halfway.
        assert!(ease_out_cubic(0.5) > 0.5);
    }

    #[test]
    fn glyph_offsets_start_at_the_configured_displacement_and_settle_to_zero() {
        for (glyph, (start_dx, start_dy)) in GLYPH_START_OFFSETS.iter().copied().enumerate() {
            assert_eq!(
                glyph_offset(0.0, glyph),
                (i32::from(start_dx), i32::from(start_dy))
            );
            assert_eq!(glyph_offset(1.0, glyph), (0, 0));
        }
    }

    #[test]
    fn completed_assembly_reproduces_the_wordmark() {
        let grid = assemble_logo_grid(1.0, 80, 20, 6, 18);
        for (row, logo_row) in LOGO.iter().enumerate() {
            let rendered: String = grid[6 + row][18..18 + LOGO_WIDTH].iter().collect();
            assert_eq!(&rendered, logo_row);
        }
    }

    #[test]
    fn assembly_clips_glyphs_that_leave_the_viewport() {
        // A tiny grid keeps every glyph fully or partially outside the frame.
        let grid = assemble_logo_grid(0.0, 20, 3, 0, 0);
        assert_eq!(grid.len(), 3);
        assert!(grid.iter().all(|row| row.len() == 20));

        // A wordmark pushed above the top edge must not panic either.
        let offscreen = assemble_logo_grid(0.0, 20, 3, 0, 0);
        assert_eq!(offscreen.iter().map(Vec::len).sum::<usize>(), 60);
    }

    #[test]
    fn kinetic_frame_fills_the_viewport_and_keeps_the_motto_below_the_wordmark() {
        let area = Rect::new(0, 0, 80, 24);
        let model = ReplView::default();
        let lines = kinetic_lines(&model, area);

        assert_eq!(lines.len(), usize::from(area.height));
        let motto = usize::from(ASSEMBLY_BLOCK_HEIGHT as u16);
        assert!(motto < lines.len());

        // The words of the first wordmark row are already separated by blanks,
        // so the rendered row is a prefix-free, space-padded line no wider than
        // the viewport.
        for line in &lines {
            let width: usize = line
                .spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum();
            assert!(width <= usize::from(area.width));
        }
    }
}
