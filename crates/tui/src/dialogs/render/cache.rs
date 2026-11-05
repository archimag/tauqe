//! Hierarchical browser over the materialized external documentation cache.
//!
//! Rows address a document by its logical cache path, which is exactly the path
//! the context layer and the prompt already carry: the disk prefix the snapshots
//! are stored under never appears in the card (c-outer-docs-logical-paths). The
//! tree folds by identifier, and one removal is guarded by the shared
//! confirmation card whose safe default is `Cancel` (Conventions §5.4, §6).

use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{render_modal_confirm, ConfirmModalParams, ModalTone};
use crate::app::{AppState, OuterDocsCacheDialogState};
use crate::components::modal::{
    modal_geometry, render_dialog_footer, render_modal_container_styled, verb_hint,
};
use crate::i18n::active_t;
use crate::theme::engine::{current_engine, ThemeEngine};
use crate::ui::centered_rect;
use crate::views::context::{CacheTreeKind, CacheTreeRow};

/// Width of the cache browser card, as a percentage of the terminal.
const CACHE_DIALOG_WIDTH_PERCENT: u16 = 70;
/// Height of the cache browser card, as a percentage of the terminal.
const CACHE_DIALOG_HEIGHT_PERCENT: u16 = 62;
/// Width of the cache removal confirmation card.
const CACHE_REMOVE_WIDTH: u16 = 60;
/// Columns one nesting level indents its row by.
const CACHE_TREE_INDENT_STEP: usize = 2;

/// Leading indentation of a tree row, derived from its nesting depth.
///
/// The indent is computed here and nowhere else, so a directory row and its
/// children can never disagree about the hierarchy (Conventions §4.8).
fn cache_row_indent(depth: usize) -> String {
    " ".repeat(depth.saturating_mul(CACHE_TREE_INDENT_STEP))
}

/// Frame of the cache browser card.
pub(crate) fn outer_docs_cache_area(term: Rect) -> Rect {
    centered_rect(
        CACHE_DIALOG_WIDTH_PERCENT,
        CACHE_DIALOG_HEIGHT_PERCENT,
        term,
    )
}

/// Interior bands of the card: the summary line and the tree below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CacheBrowserBody {
    /// One row stating what the cache holds in total.
    summary: Rect,
    /// Foldable rows of the tree.
    tree: Rect,
}

/// Splits the air-aware modal body into its summary and its tree pane.
fn cache_browser_body(body: Rect) -> CacheBrowserBody {
    let summary_height = body.height.min(1);
    let spacer_height = body.height.saturating_sub(summary_height).min(1);
    let tree_y = body
        .y
        .saturating_add(summary_height)
        .saturating_add(spacer_height);
    let tree_height = body
        .height
        .saturating_sub(summary_height)
        .saturating_sub(spacer_height);
    CacheBrowserBody {
        summary: Rect::new(body.x, body.y, body.width, summary_height),
        tree: Rect::new(body.x, tree_y, body.width, tree_height),
    }
}

/// Rows of the tree pane, derived from the terminal and the shared frame.
pub(crate) fn outer_docs_cache_tree_rect(term: Rect) -> Rect {
    cache_browser_body(modal_geometry(outer_docs_cache_area(term)).body).tree
}

/// Rows the tree pane draws at once.
///
/// Rendering, the wheel, and pointer hit-testing all read this one window, so a
/// row can never be drawn where a click would address another one
/// (Conventions §6.6).
pub(crate) fn outer_docs_cache_visible_rows(term: Rect) -> usize {
    usize::from(outer_docs_cache_tree_rect(term).height).max(1)
}

/// Renders the hierarchical cache browser.
pub(crate) fn render_outer_docs_cache_dialog(
    frame: &mut ratatui::Frame,
    dialog: &mut OuterDocsCacheDialogState,
) {
    let engine = current_engine();
    let palette = engine.palette();
    let area = outer_docs_cache_area(frame.area());
    render_modal_container_styled(
        frame,
        area,
        &active_t("dialog.outer_docs_cache.title"),
        engine.fg(palette.status_success),
    );

    let geometry = modal_geometry(area);
    let bands = cache_browser_body(geometry.body);
    let rows = dialog.rows();

    // The highlight and the window stay inside the tree that is actually drawn,
    // so a fold, a refresh, or a removal can never leave them past the last row
    // (c-outer-docs-cache-tui-deletion).
    if !rows.is_empty() {
        dialog.cursor = dialog.cursor.min(rows.len() - 1);
    }
    dialog.scroll_to_cursor(usize::from(bands.tree.height).max(1));

    let tokens: u64 = dialog
        .entries
        .iter()
        .map(|entry| entry.estimated_tokens)
        .sum();
    let summary = active_t("dialog.outer_docs_cache.summary")
        .replacen("{}", &dialog.entries.len().to_string(), 1)
        .replacen("{}", &tokens.to_string(), 1);
    frame.render_widget(
        Paragraph::new(Span::styled(summary, engine.fg(palette.text_secondary))),
        bands.summary,
    );

    let mut lines: Vec<Line<'static>> = Vec::new();
    if dialog.loading {
        lines.push(Line::from(Span::styled(
            active_t("dialog.outer_docs_cache.loading"),
            engine.fg(palette.status_warning).italic(),
        )));
    } else if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            active_t("dialog.outer_docs_cache.empty"),
            engine.fg(palette.text_muted),
        )));
    } else {
        let visible = usize::from(bands.tree.height).max(1);
        let start = usize::from(dialog.scroll);
        for (index, row) in rows.iter().enumerate().skip(start).take(visible) {
            lines.push(cache_row_line(dialog.cursor == index, row, &engine));
        }
    }
    frame.render_widget(Paragraph::new(lines), bands.tree);

    render_dialog_footer(
        frame,
        geometry.hint_row,
        &verb_hint(active_t("dialog.outer_docs_cache.footer")),
    );
}

/// One drawn row of the tree, indented by its depth and marked by its state.
fn cache_row_line(selected: bool, row: &CacheTreeRow, engine: &ThemeEngine) -> Line<'static> {
    let palette = engine.palette();
    let marker = if selected { "▶" } else { " " };
    let fold = match row.kind {
        CacheTreeKind::Directory if row.collapsed => "▶",
        CacheTreeKind::Directory => "▼",
        CacheTreeKind::Document => " ",
    };
    let label = match row.kind {
        CacheTreeKind::Directory => format!("{}/", row.label),
        CacheTreeKind::Document => row.label.clone(),
    };
    let detail = match row.kind {
        CacheTreeKind::Directory => active_t("dialog.outer_docs_cache.directory_summary")
            .replacen("{}", &row.files.to_string(), 1)
            .replacen("{}", &row.estimated_tokens.to_string(), 1),
        CacheTreeKind::Document => active_t("dialog.outer_docs_cache.document_summary")
            .replacen("{}", &row.estimated_tokens.to_string(), 1)
            .replacen("{}", &row.size_bytes.to_string(), 1),
    };
    let label_style = match row.kind {
        CacheTreeKind::Directory => engine.fg(palette.text_primary).bold(),
        CacheTreeKind::Document => engine.fg(palette.text_primary),
    };

    Line::from(vec![
        Span::raw(cache_row_indent(row.depth)),
        Span::styled(marker, engine.fg(palette.status_info).bold()),
        Span::raw(" "),
        Span::styled(fold, engine.fg(palette.status_warning)),
        Span::raw(" "),
        Span::styled(label, label_style),
        Span::raw("  "),
        Span::styled(detail, engine.fg(palette.text_muted)),
    ])
    .style(if selected {
        engine.bg(palette.overlay)
    } else {
        Style::default()
    })
}

/// Presentation parameters for the cache removal confirmation, shared by the
/// renderer and mouse hit-testing (Conventions §6, §7).
pub(crate) fn outer_docs_cache_remove_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let path = state.modals.confirm_outer_docs_cache_remove.as_deref()?;
    let engine = current_engine();
    let palette = engine.palette();

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.outer_docs_cache.remove.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                active_t("dialog.outer_docs_cache.remove.path_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(path.to_string(), engine.fg(palette.status_info).bold()),
        ]),
        Line::from(Span::styled(
            active_t("dialog.outer_docs_cache.remove.note"),
            engine.fg(palette.text_muted),
        )),
    ];

    Some(ConfirmModalParams {
        title: active_t("dialog.outer_docs_cache.remove.title"),
        tone: ModalTone::Warning,
        body_lines,
        confirm_label: active_t("dialog.button.remove"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.modals.confirm_button,
        destructive: true,
        width: CACHE_REMOVE_WIDTH,
    })
}

/// Renders the cache removal confirmation card.
pub(crate) fn render_confirm_outer_docs_cache_remove_popup(
    frame: &mut ratatui::Frame,
    state: &AppState,
) {
    if let Some(params) = outer_docs_cache_remove_params(state) {
        render_modal_confirm(frame, params);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tauqe_protocol::OuterDocsCacheEntry;

    fn entry(path: &str, size_bytes: u64, estimated_tokens: u64) -> OuterDocsCacheEntry {
        OuterDocsCacheEntry {
            path: path.to_string(),
            size_bytes,
            estimated_tokens,
        }
    }

    fn browser() -> OuterDocsCacheDialogState {
        OuterDocsCacheDialogState {
            entries: vec![
                entry("openrouter/chat-completion", 2048, 512),
                entry("openrouter/models", 1024, 256),
            ],
            ..Default::default()
        }
    }

    /// A child row is always indented strictly deeper than its parent, and the
    /// indent is a pure function of the depth (Conventions §4.8).
    #[test]
    fn tree_indent_grows_strictly_with_depth() {
        assert_eq!(cache_row_indent(0), String::new());
        assert!(
            cache_row_indent(1).chars().count() > cache_row_indent(0).chars().count(),
            "a child row must be indented deeper than its parent"
        );
        assert_eq!(
            cache_row_indent(3).chars().count(),
            3 * CACHE_TREE_INDENT_STEP
        );
    }

    /// The card draws the logical tree with its metrics, never the disk prefix.
    #[test]
    fn the_card_renders_the_logical_tree_with_its_metrics() {
        let mut dialog = browser();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| render_outer_docs_cache_dialog(frame, &mut dialog))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut screen = String::new();
        for row in 0..40u16 {
            for column in 0..120u16 {
                screen.push_str(buffer[(column, row)].symbol());
            }
            screen.push('\n');
        }
        assert!(screen.contains("External Documentation Cache"), "{screen}");
        assert!(screen.contains("openrouter/"), "{screen}");
        assert!(screen.contains("chat-completion"), "{screen}");
        assert!(screen.contains("512"), "{screen}");
        assert!(!screen.contains(".tauqe"), "{screen}");
    }

    /// The removal card names the logical path and keeps the safe `Cancel`
    /// default, so a narrowed deletion is never confirmed by a stray keystroke.
    #[test]
    fn the_removal_card_names_the_logical_path() {
        let mut st = crate::app::test_state();
        st.modals.confirm_outer_docs_cache_remove = Some("openrouter/models".to_string());

        let params = outer_docs_cache_remove_params(&st).expect("the pending removal renders");

        assert_eq!(
            params.title,
            active_t("dialog.outer_docs_cache.remove.title")
        );
        assert_eq!(params.confirm_label, active_t("dialog.button.remove"));
        assert_eq!(params.cancel_label, active_t("modal.cancel"));
        assert_eq!(
            params.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
        assert!(params.destructive);
        let joined = params
            .body_lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("openrouter/models"), "{joined}");
    }

    /// The tree pane starts one summary row and one spacer under the padded
    /// body, and its height is exactly the window a click can address.
    #[test]
    fn the_tree_pane_starts_below_the_summary_line() {
        let term = Rect::new(0, 0, 120, 40);
        let body = modal_geometry(outer_docs_cache_area(term)).body;
        let bands = cache_browser_body(body);

        assert_eq!(bands.summary.y, body.y);
        assert_eq!(bands.tree.y, body.y + 2);
        assert!(bands.tree.height > 0);
        assert_eq!(
            outer_docs_cache_visible_rows(term),
            usize::from(bands.tree.height).max(1)
        );
    }
}
