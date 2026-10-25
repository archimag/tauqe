//! Shared cache of rendered Markdown bodies for the hierarchical views.
//!
//! # Why
//!
//! Rendering a Markdown description is by far the heaviest part of a tree row:
//! the source is parsed into a pulldown-cmark event stream, every inline span is
//! styled, every fenced block is highlighted, and the result is wrapped to the
//! terminal width. The Plans, Review, and History views draw the same bodies
//! again on every frame, so moving the cursor or scrolling would re-parse text
//! that did not change by a single byte.
//!
//! [`MarkdownBodyCache`] keeps the finished `Line<'static>` vectors and hands
//! them back while the inputs that produced them stay unchanged.
//!
//! # Invalidation
//!
//! A cached body is valid only for the inputs that produced it, so the cache
//! tracks them in two scopes:
//!
//! - per entry, the Markdown source (as a content signature), the rendering
//!   variant (the [`MarkdownTheme`], which carries the resolved palette colors
//!   and the leading prefix that places the body under its row), and the code
//!   block a copy flash currently highlights;
//! - for the whole cache, the wrap width, the light/dark theme mode, and the
//!   terminal color depth, because each of them changes every body at once.
//!
//! The cache holds no global lock and is never shared between threads: it is
//! owned by the state of the view that renders the tree, which is also the only
//! writer of the bodies it stores.

// The Plans, Review, and History trees all render through this component, so a
// body is parsed once per rendering variant instead of once per frame. A few
// accessors are still exercised only by the test build; the allowance keeps the
// zero-warning gate green while the module surface settles.
#![allow(dead_code)]

use std::collections::hash_map::DefaultHasher;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use ratatui::text::Line;

use crate::config::ThemeMode;
use crate::theme::color::ColorDepth;
use crate::ui::wrap_lines;

use super::{render_markdown_with_blocks, MarkdownTheme};

/// Inputs shared by every body of one render pass.
///
/// A change in any of them alters the lines of every cached body at once, so the
/// whole cache is dropped instead of comparing them entry by entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RenderInputs {
    /// Wrap width, or `None` for bodies that are never wrapped.
    width: Option<usize>,
    /// Active light/dark mode.
    theme_mode: ThemeMode,
    /// Resolved terminal color depth.
    color_depth: ColorDepth,
}

/// One rendered body together with the inputs that produced it.
#[derive(Debug, Clone)]
struct CachedBody {
    /// Fingerprint of the Markdown source.
    signature: u64,
    /// Rendering variant the lines were produced with.
    theme: MarkdownTheme,
    /// Copy flash the body was rendered with, if any.
    flash: Option<usize>,
    /// Finished, already wrapped body lines.
    lines: Vec<Line<'static>>,
    /// Fenced code blocks of the body, in wrapped line space.
    blocks: Vec<CachedCodeBlock>,
}

impl CachedBody {
    /// Whether the body was produced by exactly the requested inputs.
    fn is_current(&self, signature: u64, theme: &MarkdownTheme, flash: Option<usize>) -> bool {
        self.signature == signature && self.theme == *theme && self.flash == flash
    }
}

/// A fenced code block located inside a cached body.
///
/// `start` and `end` are inclusive indices into the body lines, so a view can
/// place the copy button and the clickable block region over the exact rows it
/// painted without parsing the Markdown a second time.
#[derive(Debug, Clone)]
pub struct CachedCodeBlock {
    /// Position of the block inside its body, counting from zero.
    pub id: usize,
    /// Fence language, empty for an indented block.
    pub lang: String,
    /// Source text of the block.
    pub content: String,
    /// First wrapped body line of the block.
    pub start: usize,
    /// Last wrapped body line of the block.
    pub end: usize,
}

/// Result of one Markdown render pass.
struct RenderedBody {
    lines: Vec<Line<'static>>,
    blocks: Vec<CachedCodeBlock>,
}

impl RenderedBody {
    /// Seals a render result into the entry the cache retains.
    fn into_body(self, signature: u64, theme: &MarkdownTheme, flash: Option<usize>) -> CachedBody {
        CachedBody {
            signature,
            theme: theme.clone(),
            flash,
            lines: self.lines,
            blocks: self.blocks,
        }
    }
}

/// Cache of rendered Markdown bodies, keyed by the item that owns them.
///
/// The default key is the item identifier the tree projections already carry as
/// a `String`, so a lookup borrows it and only a miss clones it (Conventions
/// c-2: the owned row model stays intact and no lifetime reaches the views).
///
/// The cache is cloneable because the view state that owns it is. A cloned cache
/// keeps the bodies it already rendered, which stays correct because every entry
/// records the inputs it was produced from.
#[derive(Debug, Clone)]
pub struct MarkdownBodyCache<K = String> {
    /// Shared inputs of the current render pass.
    inputs: Option<RenderInputs>,
    /// Rendered bodies, keyed by the owning item.
    entries: HashMap<K, CachedBody>,
    /// How many bodies were actually rendered so far.
    render_count: u64,
}

impl<K> MarkdownBodyCache<K> {
    /// Creates an empty cache.
    pub fn new() -> Self {
        Self {
            inputs: None,
            entries: HashMap::new(),
            render_count: 0,
        }
    }

    /// Drops every cached body.
    ///
    /// Callers use this when something outside the tracked inputs changed the
    /// rendering, for example a live palette preset switch.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Number of cached bodies.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no body is cached.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of bodies actually rendered since the cache was created.
    ///
    /// Navigation must never re-parse a body it already rendered, so the
    /// regression tests assert this counter stays flat across a cursor move, a
    /// fold toggle, and a scroll.
    pub fn render_count(&self) -> u64 {
        self.render_count
    }

    /// Drops every body when an input shared by the whole pass changed.
    fn prepare(&mut self, width: Option<usize>, theme_mode: ThemeMode, color_depth: ColorDepth) {
        let inputs = RenderInputs {
            width,
            theme_mode,
            color_depth,
        };
        if self.inputs != Some(inputs) {
            self.entries.clear();
            self.inputs = Some(inputs);
        }
    }
}

impl<K> Default for MarkdownBodyCache<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Clone + Eq + Hash> MarkdownBodyCache<K> {
    /// Returns the wrapped body lines of `text`, rendering them only when the
    /// cached entry no longer matches the requested inputs.
    ///
    /// `theme` carries the rendering variant: the resolved palette colors and
    /// the leading prefix that places the body under its tree row, so the same
    /// description renders correctly at a different depth or in reasoning style.
    pub fn lines_for(
        &mut self,
        key: &K,
        text: &str,
        theme: &MarkdownTheme,
        width: Option<usize>,
        theme_mode: ThemeMode,
    ) -> &[Line<'static>] {
        self.body_for(key, text, theme, width, theme_mode, None)
            .lines
            .as_slice()
    }

    /// Returns the wrapped body lines **and** the fenced code blocks they
    /// contain, rendering the body only when the cached entry no longer matches
    /// the requested inputs.
    ///
    /// `flash` belongs to the rendering variant: a view that highlights one code
    /// block passes its index and receives a body drawn for it, so the highlight
    /// needs no rendering path of its own and still lives in the same cache.
    pub fn lines_and_blocks_for(
        &mut self,
        key: &K,
        text: &str,
        theme: &MarkdownTheme,
        width: Option<usize>,
        theme_mode: ThemeMode,
        flash: Option<usize>,
    ) -> (&[Line<'static>], &[CachedCodeBlock]) {
        let body = self.body_for(key, text, theme, width, theme_mode, flash);
        (body.lines.as_slice(), body.blocks.as_slice())
    }

    /// Returns the retained body for `key`, rendering and storing it when the
    /// requested inputs differ from the ones that produced it.
    fn body_for(
        &mut self,
        key: &K,
        text: &str,
        theme: &MarkdownTheme,
        width: Option<usize>,
        theme_mode: ThemeMode,
        flash: Option<usize>,
    ) -> &CachedBody {
        self.prepare(width, theme_mode, crate::theme::engine::current_depth());

        let signature = content_signature(text);
        let mut rendered = 0u64;
        let body = match self.entries.entry(key.clone()) {
            Entry::Occupied(mut occupied) => {
                if !occupied.get().is_current(signature, theme, flash) {
                    rendered += 1;
                    let fresh = render_body(text, theme, width, flash);
                    *occupied.get_mut() = fresh.into_body(signature, theme, flash);
                }
                occupied.into_mut()
            }
            Entry::Vacant(vacant) => {
                rendered += 1;
                let fresh = render_body(text, theme, width, flash);
                vacant.insert(fresh.into_body(signature, theme, flash))
            }
        };
        self.render_count = self.render_count.saturating_add(rendered);
        body
    }
}

/// Renders `text` into the wrapped body lines and code blocks the cache stores.
fn render_body(
    text: &str,
    theme: &MarkdownTheme,
    width: Option<usize>,
    flash: Option<usize>,
) -> RenderedBody {
    let (raw, extracted) = render_markdown_with_blocks(text, theme, flash);

    let Some(width) = width else {
        return RenderedBody {
            blocks: extracted
                .into_iter()
                .map(|block| CachedCodeBlock {
                    id: block.id,
                    lang: block.lang,
                    content: block.content,
                    start: block.start_line,
                    end: block.end_line.max(block.start_line),
                })
                .collect(),
            lines: raw,
        };
    };

    // Wrap one rendered line at a time and remember the visual range it occupies,
    // so a code block keeps its exact position after wrapping.
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(raw.len());
    let mut visual: Vec<(usize, usize)> = Vec::with_capacity(raw.len());
    for line in raw {
        let start = lines.len();
        lines.extend(wrap_lines(vec![line], width));
        visual.push((start, lines.len().saturating_sub(1)));
    }

    let blocks = extracted
        .into_iter()
        .filter_map(|block| {
            let (start, _) = *visual.get(block.start_line)?;
            let end = visual
                .get(block.end_line)
                .map(|(_, end)| *end)
                .unwrap_or(start)
                .max(start);
            Some(CachedCodeBlock {
                id: block.id,
                lang: block.lang,
                content: block.content,
                start,
                end,
            })
        })
        .collect();

    RenderedBody { lines, blocks }
}

/// Fingerprint of the Markdown source behind a cached body.
///
/// Keeping a hash instead of a copy of the source makes a rewritten description
/// detectable without holding every body twice.
fn content_signature(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    /// Body theme with a leading prefix, as the tree views build for a node body.
    fn body_theme(prefix: &str) -> MarkdownTheme {
        let mut theme = MarkdownTheme::answer_themed(ThemeMode::Dark);
        theme.compact = true;
        theme.hard_breaks = true;
        theme.line_prefix = Some(Span::raw(prefix.to_string()));
        theme
    }

    /// Lines a cache miss must reproduce: the renderer plus the wrap pass.
    fn fresh(text: &str, theme: &MarkdownTheme, width: Option<usize>) -> Vec<Line<'static>> {
        render_body(text, theme, width, None).lines
    }

    /// Plain text of the rendered lines, joined for substring probes.
    fn rendered(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn cached_body_matches_a_fresh_render_and_survives_repeated_lookups() {
        let theme = body_theme("  ");
        let text = "First paragraph.\n\nSecond paragraph.";
        let key = "plan-1".to_string();
        let mut cache = MarkdownBodyCache::new();

        let first = cache
            .lines_for(&key, text, &theme, Some(40), ThemeMode::Dark)
            .to_vec();
        let second = cache
            .lines_for(&key, text, &theme, Some(40), ThemeMode::Dark)
            .to_vec();

        assert_eq!(
            first,
            fresh(text, &theme, Some(40)),
            "the cache must return exactly what a fresh render produces"
        );
        assert_eq!(second, first, "a repeated lookup returns the same body");
        assert_eq!(cache.len(), 1, "a repeated lookup stores one body");
        assert!(rendered(&second).contains("  First paragraph."));
    }

    #[test]
    fn changing_the_wrap_width_drops_every_cached_body() {
        let theme = body_theme("  ");
        let text = "A description long enough to wrap at a narrow width into several lines.";
        let narrow_key = "plan-1".to_string();
        let other_key = "plan-2".to_string();
        let mut cache = MarkdownBodyCache::new();

        cache.lines_for(&narrow_key, text, &theme, Some(40), ThemeMode::Dark);
        cache.lines_for(&other_key, text, &theme, Some(40), ThemeMode::Dark);
        assert_eq!(cache.len(), 2);

        let narrow = cache
            .lines_for(&narrow_key, text, &theme, Some(40), ThemeMode::Dark)
            .to_vec();
        let wide = cache
            .lines_for(&narrow_key, text, &theme, Some(200), ThemeMode::Dark)
            .to_vec();

        assert_eq!(
            cache.len(),
            1,
            "a new wrap width drops the bodies produced at the old one"
        );
        assert!(
            wide.len() < narrow.len(),
            "the wide body must wrap into fewer lines"
        );
        assert_eq!(wide, fresh(text, &theme, Some(200)));
    }

    #[test]
    fn changing_the_theme_mode_drops_every_cached_body() {
        let theme = body_theme("  ");
        let text = "Body text.";
        let first_key = "plan-1".to_string();
        let second_key = "plan-2".to_string();
        let mut cache = MarkdownBodyCache::new();

        cache.lines_for(&first_key, text, &theme, Some(40), ThemeMode::Dark);
        cache.lines_for(&second_key, text, &theme, Some(40), ThemeMode::Dark);
        assert_eq!(cache.len(), 2);

        cache.lines_for(&first_key, text, &theme, Some(40), ThemeMode::Light);
        assert_eq!(
            cache.len(),
            1,
            "a new light/dark mode drops every body of the previous mode"
        );
    }

    #[test]
    fn changing_the_color_depth_drops_every_cached_body() {
        let theme = body_theme("  ");
        let key = "plan-1".to_string();
        let mut cache = MarkdownBodyCache::new();

        cache.lines_for(&key, "Body text.", &theme, Some(40), ThemeMode::Dark);
        assert_eq!(cache.len(), 1);

        // Pick a depth the process certainly does not resolve to, so the test
        // never depends on the capability of the machine running it.
        let other_depth = match crate::theme::engine::current_depth() {
            ColorDepth::Truecolor => ColorDepth::Ansi256,
            _ => ColorDepth::Truecolor,
        };
        cache.prepare(Some(40), ThemeMode::Dark, other_depth);

        assert!(
            cache.is_empty(),
            "a new color depth drops every cached body"
        );
    }

    #[test]
    fn rewriting_the_source_rerenders_its_own_entry_only() {
        let theme = body_theme("  ");
        let rewritten_key = "plan-1".to_string();
        let untouched_key = "plan-2".to_string();
        let mut cache = MarkdownBodyCache::new();

        let before = cache
            .lines_for(
                &rewritten_key,
                "Original cell.",
                &theme,
                None,
                ThemeMode::Dark,
            )
            .to_vec();
        cache.lines_for(
            &untouched_key,
            "Untouched body.",
            &theme,
            None,
            ThemeMode::Dark,
        );
        let after = cache
            .lines_for(
                &rewritten_key,
                "Rewritten cell.",
                &theme,
                None,
                ThemeMode::Dark,
            )
            .to_vec();

        assert_ne!(before, after, "a rewritten source must be re-rendered");
        assert_eq!(after, fresh("Rewritten cell.", &theme, None));
        assert!(rendered(&after).contains("Rewritten cell."));
        assert_eq!(cache.len(), 2, "the unchanged sibling keeps its own entry");
    }

    #[test]
    fn changing_the_rendering_variant_rerenders_the_body() {
        let key = "step-1".to_string();
        let text = "Detail.";
        let mut cache = MarkdownBodyCache::new();

        let shallow = body_theme(" │ ");
        let deep = body_theme("     │ ");

        let before = cache
            .lines_for(&key, text, &shallow, None, ThemeMode::Dark)
            .to_vec();
        let after = cache
            .lines_for(&key, text, &deep, None, ThemeMode::Dark)
            .to_vec();

        assert_ne!(
            before, after,
            "a body must be re-rendered for a new leading prefix"
        );
        assert!(
            rendered(&after).starts_with("     │ "),
            "the body carries the requested prefix: {}",
            rendered(&after)
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn render_count_tracks_only_real_renders() {
        let theme = body_theme("  ");
        let key = "plan-1".to_string();
        let mut cache = MarkdownBodyCache::new();
        assert_eq!(cache.render_count(), 0);

        cache.lines_for(&key, "Body.", &theme, Some(40), ThemeMode::Dark);
        assert_eq!(cache.render_count(), 1, "a miss renders the body once");

        cache.lines_for(&key, "Body.", &theme, Some(40), ThemeMode::Dark);
        assert_eq!(cache.render_count(), 1, "a hit never renders again");

        cache.lines_for(&key, "Rewritten body.", &theme, Some(40), ThemeMode::Dark);
        assert_eq!(cache.render_count(), 2, "a rewritten source renders again");

        cache.lines_for(&key, "Rewritten body.", &theme, Some(80), ThemeMode::Dark);
        assert_eq!(cache.render_count(), 3, "a resize renders again");

        cache.lines_for(&key, "Rewritten body.", &theme, Some(80), ThemeMode::Light);
        assert_eq!(cache.render_count(), 4, "a theme change renders again");
    }

    #[test]
    fn code_blocks_keep_their_rows_and_a_flash_is_its_own_variant() {
        let theme = body_theme("  ");
        let key = "answer-1".to_string();
        let text = "Intro.\n\n```rust\nfn main() {}\n```\n\nOutro.";
        let mut cache = MarkdownBodyCache::new();

        let (lines, blocks) =
            cache.lines_and_blocks_for(&key, text, &theme, Some(40), ThemeMode::Dark, None);
        assert!(!lines.is_empty(), "the body renders");
        assert_eq!(blocks.len(), 1, "the fenced block is reported once");
        let block = blocks[0].clone();
        assert_eq!(block.lang, "rust");
        assert!(block.content.contains("fn main"));
        assert!(
            block.end >= block.start,
            "the block range is never inverted"
        );
        assert!(block.end < lines.len(), "the block stays inside the body");

        let renders = cache.render_count();
        let _ = cache.lines_and_blocks_for(&key, text, &theme, Some(40), ThemeMode::Dark, None);
        assert_eq!(cache.render_count(), renders, "a lookup reuses the body");

        let _ = cache.lines_and_blocks_for(&key, text, &theme, Some(40), ThemeMode::Dark, Some(0));
        assert_eq!(
            cache.render_count(),
            renders + 1,
            "a flash variant is rendered as its own body"
        );
    }

    #[test]
    fn clear_drops_every_cached_body() {
        let theme = body_theme("  ");
        let mut cache = MarkdownBodyCache::new();

        cache.lines_for(
            &"plan-1".to_string(),
            "First body.",
            &theme,
            None,
            ThemeMode::Dark,
        );
        cache.lines_for(
            &"plan-2".to_string(),
            "Second body.",
            &theme,
            None,
            ThemeMode::Dark,
        );
        assert_eq!(cache.len(), 2);

        cache.clear();
        assert!(cache.is_empty(), "an explicit clear drops every body");
    }
}
